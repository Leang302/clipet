//! Platform-independent text-expansion logic.
//!
//! The native keyboard hook (see `macos.rs` / `windows.rs`) classifies each key-down into a
//! [`KeyInput`] and feeds it here. The engine keeps, in memory only, the short
//! stretch of text it knows sits left of the caret (at most 256 characters,
//! cleared on every click, arrow key or app switch) — enough to find the
//! current word even after Backspace — and decides synchronously (inside the
//! hook) whether the trigger key must be swallowed and replaced.
//!
//! Keystrokes alone can't follow edits made out of order (type `name`, Home,
//! `;`, End, Space): navigation forgets the word. When the keystrokes can't
//! tell, the engine answers [`Decision::Check`] and the hook reads the few
//! characters before the caret from the focused field ([`CaretContext`]),
//! which [`reconcile`] then matches with exactly the same rules. Fields that
//! can't be read fall back to the keystroke buffer.

use super::params::{self, Parameter};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Longest word the engine tracks. A longer run of non-space characters can't
/// be a shortcut (keywords are capped well below this), so it is dropped.
const MAX_WORD_CHARS: usize = 64;
pub const MAX_KEYWORD_CHARS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerKey {
    Space,
    Tab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerMode {
    Space,
    Tab,
    Both,
}

impl TriggerMode {
    fn accepts(self, key: TriggerKey) -> bool {
        matches!(
            (self, key),
            (TriggerMode::Both, _)
                | (TriggerMode::Space, TriggerKey::Space)
                | (TriggerMode::Tab, TriggerKey::Tab)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub prefix: char,
    pub mode: TriggerMode,
    pub escape_enabled: bool,
    /// Backspace right after an expansion puts the typed shortcut back.
    pub undo_backspace: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            prefix: ';',
            mode: TriggerMode::Space,
            escape_enabled: true,
            undo_backspace: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSnippet {
    pub id: String,
    pub content: String,
    /// An image snippet: `content` is unused and the hook pastes the image.
    pub image: bool,
    /// Empty for a static snippet (expands exactly as saved, unchanged
    /// behavior). Non-empty makes `content` a `{{name}}` template that only
    /// Tab (never Space) can complete or prompt for — see `evaluate`.
    pub parameters: Vec<Parameter>,
}

/// Enabled snippets keyed by keyword (the trigger without its prefix), lower-
/// cased when matching is case-insensitive. Rebuilt whenever snippets or
/// settings change; lookups are a single hash probe.
#[derive(Default)]
pub struct SnippetIndex {
    by_keyword: HashMap<String, IndexedSnippet>,
    case_sensitive: bool,
}

impl SnippetIndex {
    pub fn new<'a>(
        entries: impl IntoIterator<Item = (&'a str, IndexedSnippet)>,
        case_sensitive: bool,
    ) -> Self {
        let by_keyword = entries
            .into_iter()
            .map(|(keyword, snippet)| (Self::key(keyword, case_sensitive), snippet))
            .collect();
        Self {
            by_keyword,
            case_sensitive,
        }
    }

    fn key(keyword: &str, case_sensitive: bool) -> String {
        if case_sensitive {
            keyword.to_string()
        } else {
            keyword.to_lowercase()
        }
    }

    pub fn lookup(&self, keyword: &str) -> Option<&IndexedSnippet> {
        self.by_keyword
            .get(&Self::key(keyword, self.case_sensitive))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyInput<'a> {
    /// Characters produced by an ordinary key press.
    Text(&'a str),
    Backspace,
    /// Cursor movement, Return, Escape, a click, a shortcut chord, a focus
    /// change: the word being typed is no longer known.
    Reset,
    /// Paste, cut, undo or redo: the text changed in a way keystrokes don't
    /// show, so it may now hold a shortcut that was never typed.
    Edit,
    Trigger(TriggerKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Let the key through untouched.
    Pass,
    /// Swallow the trigger key, erase `delete_chars` characters, then insert
    /// `insert`: the snippet exactly as saved, plus its own whitespace setting
    /// (folded into the indexed content). The trigger key is never inserted.
    Expand {
        snippet_id: String,
        delete_chars: usize,
        insert: String,
    },
    /// Swallow the trigger key, erase `delete_chars` characters, then paste
    /// the snippet's image. The trigger key itself is not inserted.
    ExpandImage {
        snippet_id: String,
        delete_chars: usize,
    },
    /// Same mechanics as `Expand`, for `;;text` → `;text`.
    Escape { delete_chars: usize, insert: String },
    /// Swallow the trigger key and erase `delete_chars` characters (the
    /// whole invocation: prefix, keyword, and any args already typed), then
    /// open the parameter popup for `snippet_id`, prefilled with whatever
    /// positional args were already typed (by parameter name). Produced
    /// when a parameterized snippet is missing a required value, got an
    /// invalid one, or got too many positional arguments — the popup
    /// re-validates and reports the specific problem rather than the engine
    /// guessing at UI text.
    Prompt {
        snippet_id: String,
        delete_chars: usize,
        prefill: HashMap<String, String>,
        /// The exact invocation text (`delete_chars` long, ASCII) that will be
        /// replaced — kept in memory only, never logged — so the insertion
        /// can confirm the field still ends with it after focus returns.
        typed: String,
    },
    /// Keystrokes can't tell what precedes the caret (the cursor moved, or
    /// text was pasted, since the shortcut prefix appeared). Swallow the
    /// trigger key and ask the focused field; see [`reconcile`].
    Check { key: TriggerKey },
    /// Backspace right after an expansion: erase `delete_chars` characters
    /// (the inserted text, `expect`) and put back `insert`, the shortcut as
    /// typed. The Backspace itself is not delivered.
    Undo {
        delete_chars: usize,
        insert: String,
        expect: String,
    },
}

impl Decision {
    pub fn swallows_trigger(&self) -> bool {
        !matches!(self, Decision::Pass)
    }

    /// For diagnostics: never includes any text.
    pub fn label(&self) -> &'static str {
        match self {
            Decision::Pass => "pass",
            Decision::Expand { .. } => "expand",
            Decision::ExpandImage { .. } => "expand image",
            Decision::Escape { .. } => "escape",
            Decision::Prompt { .. } => "prompt",
            Decision::Check { .. } => "check field",
            Decision::Undo { .. } => "undo",
        }
    }
}

/// How much known text left of the caret is remembered (in memory only), and
/// the most a field is asked for.
pub const MAX_LINE_CHARS: usize = 256;

/// Longest expansion Backspace can undo. Undo erases the inserted text one
/// Backspace per character, so it is offered only when that is exact.
const MAX_UNDO_CHARS: usize = 500;

/// What a focused field reports around its caret. Read on demand, used for
/// one decision and dropped — never stored or logged.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CaretContext {
    /// Up to [`MAX_LINE_CHARS`] characters immediately before the caret.
    pub before: String,
    /// `before` begins at the very start of the field.
    pub at_start: bool,
    /// The character right after the caret: `None` at the end of the field
    /// or when the field doesn't say.
    pub after: Option<char>,
}

/// What the hook learned from the focused field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldText {
    /// A password field, or text is selected (typing replaces it): never
    /// expand.
    Blocked,
    /// The field doesn't expose its text or caret; fall back to keystrokes.
    Unavailable,
    Known(CaretContext),
}

impl FieldText {
    pub fn label(&self) -> &'static str {
        match self {
            FieldText::Blocked => "blocked",
            FieldText::Unavailable => "unavailable",
            FieldText::Known(_) => "read",
        }
    }
}

/// The final decision once the field has been consulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciled {
    pub decision: Decision,
    /// The decision came from the field's text rather than the keystroke
    /// buffer, so the engine must be told what happened ([`Engine::adopt`]).
    pub from_field: bool,
    /// The shortcut text being replaced, when it came from the field (for
    /// undo).
    pub replaced: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UndoRecord {
    inserted: String,
    original: String,
}

pub struct Engine {
    /// Text known to sit immediately left of the caret: what was typed plus
    /// what Clipet inserted, since the last reset (click, arrow key, app
    /// switch…). Remembering inserted text lets the user Backspace over an
    /// expansion and retype a shortcut without losing track of the word.
    line: VecDeque<char>,
    /// The prefix was typed, or text was pasted, since the last trigger key.
    /// Survives cursor movement: that's exactly when a shortcut can be
    /// completed out of order and only the field can confirm it.
    prefix_pending: bool,
    /// The expansion that the very next Backspace may undo.
    undo: Option<UndoRecord>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            line: VecDeque::new(),
            prefix_pending: false,
            undo: None,
        }
    }

    /// The caret moved or the text changed unseen: forget the known text. A
    /// pending prefix is kept (see `prefix_pending`).
    pub fn reset(&mut self) {
        self.line.clear();
        self.undo = None;
    }

    /// A different app, or settings changed: forget everything.
    pub fn forget(&mut self) {
        self.reset();
        self.prefix_pending = false;
    }

    /// True when nothing Clipet saw precedes the current word, so whether the
    /// word is glued to earlier text (`abc;name`) is unknown from keystrokes
    /// alone. The hook then asks the focused field before replacing.
    pub fn word_starts_line(&self) -> bool {
        word_len(self.line.iter()) == self.line.len()
    }

    /// Length of the word left of the caret (for diagnostics).
    pub fn word_chars(&self) -> usize {
        word_len(self.line.iter())
    }

    fn push(&mut self, ch: char) {
        if self.line.len() == MAX_LINE_CHARS {
            self.line.pop_front();
        }
        self.line.push_back(ch);
    }

    /// Feed one key press. Only a `Trigger` (or a Backspace that undoes an
    /// expansion) can produce a non-`Pass` decision.
    pub fn feed(
        &mut self,
        input: KeyInput<'_>,
        config: &EngineConfig,
        index: &SnippetIndex,
    ) -> Decision {
        let undo = self.undo.take();
        match input {
            KeyInput::Text(text) => {
                for ch in text.chars() {
                    if ch == config.prefix {
                        self.prefix_pending = true;
                    }
                    if ch.is_control() && !ch.is_whitespace() {
                        self.line.clear();
                    } else {
                        self.push(ch);
                    }
                }
                Decision::Pass
            }
            KeyInput::Backspace => {
                if let Some(undo) = undo.filter(|_| config.undo_backspace) {
                    let decision = Decision::Undo {
                        delete_chars: undo.inserted.chars().count(),
                        insert: undo.original.clone(),
                        expect: undo.inserted,
                    };
                    self.apply(None, &decision, None);
                    return decision;
                }
                // Past the start of `line` the deleted text was never seen;
                // `word_starts_line` reports that uncertainty.
                self.line.pop_back();
                Decision::Pass
            }
            KeyInput::Reset => {
                self.reset();
                Decision::Pass
            }
            KeyInput::Edit => {
                self.reset();
                self.prefix_pending = true;
                Decision::Pass
            }
            KeyInput::Trigger(key) => {
                let pending = std::mem::take(&mut self.prefix_pending);
                let accepted = config.mode.accepts(key);
                let chars: Vec<char> = self.line.iter().copied().collect();
                let mut decision = if accepted {
                    evaluate_line(&chars, key, config, index)
                } else {
                    Decision::Pass
                };
                // Out-of-order edits: the buffer only holds the tail of the
                // word (or nothing) and a prefix was typed or pasted where the
                // keystrokes can't place it. Only then is the field read, so
                // ordinary typing never waits on it. (A buffered word that
                // already holds the prefix can't turn into a different match.)
                let word = &chars[chars.len() - word_len(chars.iter())..];
                if decision == Decision::Pass
                    && accepted
                    && pending
                    && self.word_starts_line()
                    && word.len() <= MAX_WORD_CHARS
                    && word.iter().all(|c| c.is_ascii() && *c != config.prefix)
                {
                    decision = Decision::Check { key };
                }
                let replaced = match &decision {
                    Decision::Expand { delete_chars, .. } => {
                        Some(chars[chars.len() - delete_chars..].iter().collect::<String>())
                    }
                    _ => None,
                };
                self.apply(Some(key), &decision, replaced.as_deref());
                decision
            }
        }
    }

    /// Tracks what the text will look like once `decision` is carried out.
    /// (If the hook later has to skip a replacement, it resets the engine.)
    fn apply(&mut self, key: Option<TriggerKey>, decision: &Decision, replaced: Option<&str>) {
        match decision {
            Decision::Pass if key == Some(TriggerKey::Space) => self.push(' '),
            // A plain Tab may move focus to another field.
            Decision::Pass => self.reset(),
            Decision::Expand {
                delete_chars,
                insert,
                ..
            }
            | Decision::Escape {
                delete_chars,
                insert,
            }
            | Decision::Undo {
                delete_chars,
                insert,
                ..
            } => {
                for _ in 0..*delete_chars {
                    self.line.pop_back();
                }
                for ch in insert.chars() {
                    self.push(ch);
                }
                if let (Decision::Expand { insert, .. }, Some(replaced)) = (decision, replaced) {
                    if undo_is_exact(insert) && replaced.is_ascii() {
                        let space = if key == Some(TriggerKey::Space) { " " } else { "" };
                        self.undo = Some(UndoRecord {
                            inserted: insert.clone(),
                            original: format!("{replaced}{space}"),
                        });
                    }
                }
            }
            // The text around a pasted image is unknown, and so is whatever
            // the popup will eventually do (insert or the user cancels) for
            // `Prompt`. For `Check`, the hook reports back via `adopt`.
            Decision::ExpandImage { .. } | Decision::Prompt { .. } | Decision::Check { .. } => {
                self.reset()
            }
        }
    }

    /// Records a decision that [`reconcile`] made from the field's text: the
    /// text before it is not kept (it came from another app), only what
    /// Clipet itself is about to insert.
    pub fn adopt(&mut self, key: Option<TriggerKey>, reconciled: &Reconciled) {
        self.reset();
        self.apply(key, &reconciled.decision, reconciled.replaced.as_deref());
    }
}

/// Backspace erases one character per press only for simple characters;
/// combining marks, emoji and line breaks can take more or fewer presses.
fn undo_is_exact(text: &str) -> bool {
    let count = text.chars().count();
    count > 0
        && count <= MAX_UNDO_CHARS
        && text.chars().all(|c| (' '..'\u{0300}').contains(&c))
}

fn word_len<'a>(chars: impl DoubleEndedIterator<Item = &'a char>) -> usize {
    chars.rev().take_while(|c| !c.is_whitespace()).count()
}

/// A character that would glue an expansion to the following text.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Matches the text before the caret, as read from the field, with the same
/// rules as typed text.
pub fn evaluate_context(
    context: &CaretContext,
    key: TriggerKey,
    config: &EngineConfig,
    index: &SnippetIndex,
) -> Decision {
    if !config.mode.accepts(key) {
        return Decision::Pass;
    }
    // The caret sits inside a word (`;na|me`, `;name|x`): replacing the
    // part before it would glue the expansion to the rest.
    if context.after.is_some_and(is_word_char) {
        return Decision::Pass;
    }
    let mut chars: Vec<char> = context.before.chars().collect();
    if chars.len() > MAX_LINE_CHARS {
        chars.drain(..chars.len() - MAX_LINE_CHARS);
    }
    if !context.at_start || context.before.chars().count() > MAX_LINE_CHARS {
        // The first word may be cut off; never treat it as a word start.
        let partial = chars.iter().take_while(|c| !c.is_whitespace()).count();
        chars.drain(..partial);
    }
    evaluate_line(&chars, key, config, index)
}

/// The final decision for a swallowed key, given what the focused field
/// reported. `anchored`: the keystroke buffer saw whitespace before the
/// word, so it knows the whole word.
pub fn reconcile(
    decision: Decision,
    key: Option<TriggerKey>,
    anchored: bool,
    field: &FieldText,
    config: &EngineConfig,
    index: &SnippetIndex,
) -> Reconciled {
    let keep = |decision| Reconciled {
        decision,
        from_field: false,
        replaced: None,
    };
    let from_context = |context: &CaretContext, key| {
        let decision = evaluate_context(context, key, config, index);
        let replaced = match &decision {
            Decision::Expand { delete_chars, .. } => {
                let chars: Vec<char> = context.before.chars().collect();
                chars
                    .len()
                    .checked_sub(*delete_chars)
                    .map(|start| chars[start..].iter().collect())
            }
            _ => None,
        };
        Reconciled {
            decision,
            from_field: true,
            replaced,
        }
    };
    if *field == FieldText::Blocked {
        return keep(Decision::Pass);
    }
    match (decision, field) {
        (Decision::Pass, _) => keep(Decision::Pass),
        (Decision::Check { key }, FieldText::Known(context)) => from_context(context, key),
        // Nothing to go on: never erase based on a guess.
        (Decision::Check { .. }, _) => keep(Decision::Pass),
        (Decision::Undo { expect, .. }, FieldText::Known(context))
            if !context.before.ends_with(expect.as_str()) =>
        {
            keep(Decision::Pass)
        }
        (undo @ Decision::Undo { .. }, _) => keep(undo),
        (_, FieldText::Known(context)) if context.after.is_some_and(is_word_char) => {
            keep(Decision::Pass)
        }
        // The buffer saw the word start after a reset but not what came
        // before it (`abc|` click `;name`): the field settles it.
        (_, FieldText::Known(context)) if !anchored && key.is_some() => {
            from_context(context, key.unwrap())
        }
        // Typed in one go after whitespace, or the field can't be read: the
        // keystrokes are authoritative (an accessibility tree may also lag a
        // fast typist by a few milliseconds).
        (decision, _) => keep(decision),
    }
}

/// Evaluates the text left of the caret (`line`, oldest first). A word that
/// starts `line` is treated as starting a word.
fn evaluate_line(
    line: &[char],
    key: TriggerKey,
    config: &EngineConfig,
    index: &SnippetIndex,
) -> Decision {
    let word_chars = word_len(line.iter());
    if word_chars == 0 || word_chars > MAX_WORD_CHARS {
        return Decision::Pass;
    }
    let word: String = line[line.len() - word_chars..].iter().collect();
    let prefix = config.prefix;
    let trailing = if key == TriggerKey::Space { " " } else { "" };

    // Replacement erases matched text with one Backspace per character.
    // That is only exact for single-code-point characters; combining
    // marks (Khmer, Hangul jamo, accents) can erase differently per app,
    // so such text is left untouched rather than risk corrupting it.
    if word.is_ascii() {
        if let Some(after_prefix) = word.strip_prefix(prefix) {
            if let Some(escaped) = after_prefix.strip_prefix(prefix) {
                // `;;text` is either an escape or plain text; it never expands.
                if config.escape_enabled && !escaped.is_empty() {
                    return Decision::Escape {
                        delete_chars: word_chars,
                        insert: format!("{after_prefix}{trailing}"),
                    };
                }
                return Decision::Pass;
            }

            match index.lookup(after_prefix) {
                Some(snippet) if snippet.image => {
                    return Decision::ExpandImage {
                        snippet_id: snippet.id.clone(),
                        delete_chars: word_chars,
                    }
                }
                // A parameterized snippet typed with no args yet: Tab
                // either fills in from defaults alone or opens the popup.
                // Space never triggers it (Part D3: Tab is the reliable
                // delimiter for parameterized snippets; this also keeps
                // "type a word, hit space" sentences from expanding one).
                Some(snippet) if !snippet.parameters.is_empty() => {
                    return if key == TriggerKey::Tab {
                        build_param_decision(snippet, &word, "")
                    } else {
                        Decision::Pass
                    };
                }
                Some(snippet) => {
                    return Decision::Expand {
                        snippet_id: snippet.id.clone(),
                        delete_chars: word_chars,
                        insert: snippet.content.clone(),
                    }
                }
                // Falls through: the trailing word alone isn't a known
                // keyword, but on Tab it might be the tail end of
                // `;keyword arg1 arg2` — checked below.
                None => {}
            }
        }
    }

    // Backward scan for `;keyword <already-typed args>` ending at the
    // caret. Only engaged for Tab, and only acted on when the matched
    // snippet actually declares parameters, so plain static snippets
    // keep matching only the single trailing word exactly as before.
    if key == TriggerKey::Tab {
        if let Some((delete_chars, keyword, raw_args)) = find_invocation(line, prefix) {
            if let Some(snippet) = index.lookup(&keyword) {
                if !snippet.parameters.is_empty() && raw_args.is_ascii() {
                    let typed: String = line[line.len() - delete_chars..].iter().collect();
                    return build_param_decision(snippet, &typed, &raw_args);
                }
            }
        }
    }

    Decision::Pass
}

/// Looks backward through `line` for the most recent token starting with
/// `prefix` (preceded by start-of-line or whitespace) that looks like a
/// valid keyword, and returns how many trailing characters it plus
/// everything typed after it spans, the keyword itself (without the
/// prefix), and the raw text typed after it (potential arguments,
/// unparsed — quoting is resolved later by `params::tokenize_arguments`).
fn find_invocation(chars: &[char], prefix: char) -> Option<(usize, String, String)> {
    let mut i = 0;
    let mut best_start = None;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        if chars[start] == prefix {
            let rest = &chars[start + 1..i];
            let valid = !rest.is_empty()
                && rest.len() <= MAX_KEYWORD_CHARS
                && rest
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if valid {
                best_start = Some(start); // keep the rightmost match
            }
        }
    }
    let start = best_start?;
    let mut keyword_end = start + 1;
    while keyword_end < chars.len() && !chars[keyword_end].is_whitespace() {
        keyword_end += 1;
    }
    let keyword: String = chars[start + 1..keyword_end].iter().collect();
    let raw_args: String = chars[keyword_end..].iter().collect();
    let delete_chars = chars.len() - start;
    Some((delete_chars, keyword, raw_args))
}

/// Turns a matched parameterized snippet plus whatever argument text was
/// already typed into either a ready-to-insert `Expand` (every value
/// present and valid) or a `Prompt` to open the popup — never a partial or
/// guessed-at insertion. Malformed template configuration fails safe to
/// `Pass` rather than inserting something wrong (Part D4).
fn build_param_decision(snippet: &IndexedSnippet, typed: &str, raw_args: &str) -> Decision {
    if params::placeholders_in_order(&snippet.content).is_err() {
        return Decision::Pass;
    }
    let delete_chars = typed.chars().count();
    // Positional args follow the configured parameter order rather than this
    // template's placeholder order, so `;cmd a b` fills the same parameters
    // whichever per-OS variant is active.
    let order: Vec<String> = snippet.parameters.iter().map(|p| p.name.clone()).collect();
    let prefill_on_error = |supplied: HashMap<String, String>| Decision::Prompt {
        snippet_id: snippet.id.clone(),
        delete_chars,
        prefill: supplied,
        typed: typed.to_string(),
    };
    let Ok(args) = params::tokenize_arguments(raw_args) else {
        return prefill_on_error(HashMap::new());
    };
    let Ok(supplied) = params::map_positional_args(&order, &args) else {
        return prefill_on_error(HashMap::new());
    };
    match params::validate_all(&snippet.parameters, &supplied) {
        Ok(validated) => match params::render(&snippet.content, &validated.values) {
            Ok(text) => Decision::Expand {
                snippet_id: snippet.id.clone(),
                delete_chars,
                insert: text,
            },
            Err(_) => prefill_on_error(supplied),
        },
        Err(_) => prefill_on_error(supplied),
    }
}

/// Keywords are ASCII so erasing them is exact (see `Engine::evaluate`).
pub fn validate_keyword(keyword: &str, prefix: char) -> Result<(), String> {
    if keyword.is_empty() {
        return Err("Enter a shortcut after the prefix, for example ;name".to_string());
    }
    if keyword.chars().count() > MAX_KEYWORD_CHARS {
        return Err(format!(
            "Shortcuts can be at most {MAX_KEYWORD_CHARS} characters"
        ));
    }
    if keyword.contains(prefix) {
        return Err(format!("A shortcut can't contain the prefix \"{prefix}\""));
    }
    if !keyword
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(
            "Shortcuts can only use English letters, digits, - _ and .".to_string(),
        );
    }
    Ok(())
}

pub fn validate_prefix(prefix: &str) -> Result<char, String> {
    let mut chars = prefix.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None)
            if c.is_ascii_punctuation() && !matches!(c, '-' | '_' | '.') =>
        {
            Ok(c)
        }
        _ => Err("The prefix must be a single symbol such as ; / or !".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(case_sensitive: bool) -> SnippetIndex {
        SnippetIndex::new(
            [
                (
                    "name",
                    IndexedSnippet {
                        id: "s-name".into(),
                        content: "KimLeang".into(),
                        image: false,
                        parameters: Vec::new(),
                    },
                ),
                (
                    "sig",
                    IndexedSnippet {
                        id: "s-sig".into(),
                        content: "Best,\nKim".into(),
                        image: false,
                        parameters: Vec::new(),
                    },
                ),
            ],
            case_sensitive,
        )
    }

    fn type_word(engine: &mut Engine, text: &str, config: &EngineConfig, index: &SnippetIndex) {
        for ch in text.chars() {
            let s = ch.to_string();
            assert_eq!(engine.feed(KeyInput::Text(&s), config, index), Decision::Pass);
        }
    }

    fn run(text: &str, key: TriggerKey, config: &EngineConfig) -> Decision {
        let idx = index(true);
        let mut engine = Engine::new();
        type_word(&mut engine, text, config, &idx);
        engine.feed(KeyInput::Trigger(key), config, &idx)
    }

    #[test]
    fn space_trigger_is_not_inserted() {
        let decision = run("Hello ;name", TriggerKey::Space, &EngineConfig::default());
        assert_eq!(
            decision,
            Decision::Expand {
                snippet_id: "s-name".into(),
                delete_chars: 5,
                insert: "KimLeang".into()
            }
        );
    }

    #[test]
    fn saved_whitespace_is_inserted_exactly() {
        let idx = SnippetIndex::new(
            [(
                "pad",
                IndexedSnippet {
                    id: "s-pad".into(),
                    content: "  Hello World \n".into(),
                    image: false,
                    parameters: Vec::new(),
                },
            )],
            true,
        );
        let config = EngineConfig::default();
        let mut engine = Engine::new();
        type_word(&mut engine, ";pad", &config, &idx);
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Expand {
                snippet_id: "s-pad".into(),
                delete_chars: 4,
                insert: "  Hello World \n".into()
            }
        );
    }

    #[test]
    fn tab_expands_without_inserting_anything_extra() {
        let config = EngineConfig {
            mode: TriggerMode::Tab,
            ..EngineConfig::default()
        };
        assert_eq!(
            run(";name", TriggerKey::Tab, &config),
            Decision::Expand {
                snippet_id: "s-name".into(),
                delete_chars: 5,
                insert: "KimLeang".into()
            }
        );
        // Space is not a trigger in Tab-only mode.
        assert_eq!(run(";name", TriggerKey::Space, &config), Decision::Pass);
    }

    #[test]
    fn both_mode_accepts_either_key() {
        let config = EngineConfig {
            mode: TriggerMode::Both,
            ..EngineConfig::default()
        };
        assert!(run(";name", TriggerKey::Space, &config).swallows_trigger());
        assert!(run(";name", TriggerKey::Tab, &config).swallows_trigger());
    }

    #[test]
    fn default_mode_leaves_tab_alone() {
        assert_eq!(
            run(";name", TriggerKey::Tab, &EngineConfig::default()),
            Decision::Pass
        );
    }

    #[test]
    fn unknown_shortcut_passes_through() {
        assert_eq!(
            run(";unknown", TriggerKey::Space, &EngineConfig::default()),
            Decision::Pass
        );
        let tab = EngineConfig {
            mode: TriggerMode::Both,
            ..EngineConfig::default()
        };
        assert_eq!(run(";unknown", TriggerKey::Tab, &tab), Decision::Pass);
    }

    #[test]
    fn shortcut_inside_a_word_does_not_expand() {
        assert_eq!(
            run("abc;name", TriggerKey::Space, &EngineConfig::default()),
            Decision::Pass
        );
    }

    #[test]
    fn double_prefix_escapes_registered_and_unregistered_words() {
        let config = EngineConfig::default();
        assert_eq!(
            run(";;name", TriggerKey::Space, &config),
            Decision::Escape {
                delete_chars: 6,
                insert: ";name ".into()
            }
        );
        assert_eq!(
            run(";;jhjh", TriggerKey::Space, &config),
            Decision::Escape {
                delete_chars: 6,
                insert: ";jhjh ".into()
            }
        );
        let tab = EngineConfig {
            mode: TriggerMode::Tab,
            ..EngineConfig::default()
        };
        assert_eq!(
            run(";;name", TriggerKey::Tab, &tab),
            Decision::Escape {
                delete_chars: 6,
                insert: ";name".into()
            }
        );
    }

    #[test]
    fn escape_removes_only_one_prefix() {
        assert_eq!(
            run(";;;x", TriggerKey::Space, &EngineConfig::default()),
            Decision::Escape {
                delete_chars: 4,
                insert: ";;x ".into()
            }
        );
    }

    #[test]
    fn bare_double_prefix_is_left_alone() {
        assert_eq!(
            run(";;", TriggerKey::Space, &EngineConfig::default()),
            Decision::Pass
        );
    }

    #[test]
    fn disabled_escape_keeps_double_prefix_literal_and_never_expands() {
        let config = EngineConfig {
            escape_enabled: false,
            ..EngineConfig::default()
        };
        assert_eq!(run(";;name", TriggerKey::Space, &config), Decision::Pass);
    }

    #[test]
    fn backspace_edits_the_tracked_word() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        type_word(&mut engine, ";namx", &config, &idx);
        engine.feed(KeyInput::Backspace, &config, &idx);
        type_word(&mut engine, "e", &config, &idx);
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            .swallows_trigger());
    }

    #[test]
    fn backspacing_into_unknown_text_blocks_until_whitespace() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        // "abc " typed, then the space erased: the cursor now sits after "abc".
        type_word(&mut engine, "abc ", &config, &idx);
        engine.feed(KeyInput::Backspace, &config, &idx);
        type_word(&mut engine, ";name", &config, &idx);
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Pass
        );
        // After that space the next word is eligible again.
        type_word(&mut engine, ";name", &config, &idx);
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            .swallows_trigger());
    }

    #[test]
    fn deleting_an_expansion_and_retyping_expands_again() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        for _ in 0..3 {
            type_word(&mut engine, ";name", &config, &idx);
            let Decision::Expand { insert, .. } =
                engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            else {
                panic!("should expand");
            };
            // Erase the inserted "KimLeang " entirely, then try again.
            for _ in 0..insert.chars().count() {
                engine.feed(KeyInput::Backspace, &config, &idx);
            }
        }
    }

    #[test]
    fn expansion_after_earlier_text_and_partial_delete() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        type_word(&mut engine, "Hi ;name", &config, &idx);
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            .swallows_trigger());
        // "Hi KimLeang" → delete "KimLeang" → "Hi " → retype.
        for _ in 0.."KimLeang".len() {
            engine.feed(KeyInput::Backspace, &config, &idx);
        }
        type_word(&mut engine, ";name", &config, &idx);
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            .swallows_trigger());
    }

    #[test]
    fn unseen_text_before_the_word_is_flagged_for_verification() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        // Select-all + Delete, or holding Backspace past the start: the field
        // may be empty or the caret may follow unseen text. The engine still
        // matches, but flags that the hook must check the field.
        engine.feed(KeyInput::Backspace, &config, &idx);
        type_word(&mut engine, ";name", &config, &idx);
        assert!(engine.word_starts_line());
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            .swallows_trigger());
        // With known whitespace before it, no verification is needed. (Exact
        // insertion adds no space, so the person types one.)
        engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx);
        type_word(&mut engine, ";name", &config, &idx);
        assert!(!engine.word_starts_line());
    }

    #[test]
    fn escaped_text_is_tracked_as_plain_text() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        type_word(&mut engine, ";;name", &config, &idx);
        assert!(matches!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Escape { .. }
        ));
        // Backspace over the escaped ";name " and the next ";name" expands.
        for _ in 0..";name ".len() {
            engine.feed(KeyInput::Backspace, &config, &idx);
        }
        type_word(&mut engine, ";name", &config, &idx);
        assert!(matches!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Expand { .. }
        ));
    }

    #[test]
    fn navigation_resets_the_word() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        type_word(&mut engine, ";na", &config, &idx);
        engine.feed(KeyInput::Reset, &config, &idx);
        type_word(&mut engine, "me", &config, &idx);
        // The keystrokes alone no longer know the word, so they never erase
        // anything; the field is asked instead (see `field_*` tests).
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Check {
                key: TriggerKey::Space
            }
        );
        // Without a prefix since the last trigger nothing is checked.
        engine.feed(KeyInput::Reset, &config, &idx);
        type_word(&mut engine, "me", &config, &idx);
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Pass
        );
    }

    #[test]
    fn trigger_resets_so_the_same_word_never_expands_twice() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        type_word(&mut engine, ";name", &config, &idx);
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx)
            .swallows_trigger());
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Pass
        );
    }

    #[test]
    fn case_sensitivity_follows_the_index() {
        let config = EngineConfig::default();
        let mut engine = Engine::new();
        let sensitive = index(true);
        type_word(&mut engine, ";NAME", &config, &sensitive);
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &sensitive),
            Decision::Pass
        );
        let insensitive = index(false);
        type_word(&mut engine, ";NAME", &config, &insensitive);
        assert!(engine
            .feed(KeyInput::Trigger(TriggerKey::Space), &config, &insensitive)
            .swallows_trigger());
    }

    #[test]
    fn non_ascii_words_are_never_rewritten() {
        let config = EngineConfig::default();
        assert_eq!(run(";;ឈ្មោះ", TriggerKey::Space, &config), Decision::Pass);
        assert_eq!(run(";이름", TriggerKey::Space, &config), Decision::Pass);
    }

    #[test]
    fn very_long_words_do_not_match() {
        let long = format!(";{}", "a".repeat(200));
        assert_eq!(
            run(&long, TriggerKey::Space, &EngineConfig::default()),
            Decision::Pass
        );
    }

    #[test]
    fn multiline_content_is_preserved() {
        assert_eq!(
            run(";sig", TriggerKey::Space, &EngineConfig::default()),
            Decision::Expand {
                snippet_id: "s-sig".into(),
                delete_chars: 4,
                insert: "Best,\nKim".into()
            }
        );
    }

    #[test]
    fn image_snippet_pastes_without_inserting_text_and_forgets_the_line() {
        let idx = SnippetIndex::new(
            [(
                "logo",
                IndexedSnippet {
                    id: "s-logo".into(),
                    content: String::new(),
                    image: true,
                    parameters: Vec::new(),
                },
            )],
            true,
        );
        let config = EngineConfig {
            mode: TriggerMode::Both,
            ..EngineConfig::default()
        };
        for key in [TriggerKey::Space, TriggerKey::Tab] {
            let mut engine = Engine::new();
            type_word(&mut engine, "Hi ;logo", &config, &idx);
            assert_eq!(
                engine.feed(KeyInput::Trigger(key), &config, &idx),
                Decision::ExpandImage {
                    snippet_id: "s-logo".into(),
                    delete_chars: 5
                }
            );
            // After the image nothing is known about the line, so a word typed
            // next doesn't count as following "Hi ".
            type_word(&mut engine, "x", &config, &idx);
            assert!(engine.word_starts_line());
        }
    }

    #[test]
    fn custom_prefix() {
        let config = EngineConfig {
            prefix: '/',
            ..EngineConfig::default()
        };
        assert!(run("/name", TriggerKey::Space, &config).swallows_trigger());
        assert_eq!(run(";name", TriggerKey::Space, &config), Decision::Pass);
        assert_eq!(
            run("//name", TriggerKey::Space, &config),
            Decision::Escape {
                delete_chars: 6,
                insert: "/name ".into()
            }
        );
    }

    #[test]
    fn keyword_and_prefix_validation() {
        assert!(validate_keyword("name", ';').is_ok());
        assert!(validate_keyword("e-mail_2.x", ';').is_ok());
        assert!(validate_keyword("", ';').is_err());
        assert!(validate_keyword("a;b", ';').is_err());
        assert!(validate_keyword("has space", ';').is_err());
        assert!(validate_keyword("이름", ';').is_err());
        assert!(validate_keyword(&"a".repeat(33), ';').is_err());
        assert_eq!(validate_prefix(";"), Ok(';'));
        assert!(validate_prefix("a").is_err());
        assert!(validate_prefix(";;").is_err());
        assert!(validate_prefix("").is_err());
    }

    fn killport_index() -> SnippetIndex {
        SnippetIndex::new(
            [(
                "killport",
                IndexedSnippet {
                    id: "s-killport".into(),
                    content: "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs kill".into(),
                    image: false,
                    parameters: vec![Parameter {
                        name: "port".into(),
                        kind: params::ParamType::Integer {
                            min: Some(1),
                            max: Some(65535),
                        },
                        required: true,
                        default: None,
                    }],
                },
            )],
            true,
        )
    }

    fn hello_index() -> SnippetIndex {
        SnippetIndex::new(
            [(
                "hello",
                IndexedSnippet {
                    id: "s-hello".into(),
                    content: "Hello {{name}}, your project {{project}} is ready.".into(),
                    image: false,
                    parameters: vec![
                        Parameter {
                            name: "name".into(),
                            kind: params::ParamType::Text {
                                min_length: None,
                                max_length: None,
                                pattern: None,
                            },
                            required: true,
                            default: None,
                        },
                        Parameter {
                            name: "project".into(),
                            kind: params::ParamType::Text {
                                min_length: None,
                                max_length: None,
                                pattern: None,
                            },
                            required: true,
                            default: None,
                        },
                    ],
                },
            )],
            true,
        )
    }

    fn run_with(text: &str, key: TriggerKey, config: &EngineConfig, idx: &SnippetIndex) -> Decision {
        let mut engine = Engine::new();
        type_word(&mut engine, text, config, idx);
        engine.feed(KeyInput::Trigger(key), config, idx)
    }

    fn tab_config() -> EngineConfig {
        EngineConfig {
            mode: TriggerMode::Tab,
            ..EngineConfig::default()
        }
    }

    #[test]
    fn inline_parameterized_expansion_with_tab() {
        let config = tab_config();
        let decision = run_with(";killport 5173", TriggerKey::Tab, &config, &killport_index());
        assert_eq!(
            decision,
            Decision::Expand {
                snippet_id: "s-killport".into(),
                delete_chars: ";killport 5173".chars().count(),
                insert: "lsof -tiTCP:5173 -sTCP:LISTEN | xargs kill".into(),
            }
        );
    }

    #[test]
    fn parameterized_snippet_ignores_space_trigger() {
        let config = EngineConfig {
            mode: TriggerMode::Both,
            ..EngineConfig::default()
        };
        // Space must never complete a parameterized snippet, even mid-args.
        assert_eq!(
            run_with(";killport", TriggerKey::Space, &config, &killport_index()),
            Decision::Pass
        );
    }

    #[test]
    fn missing_required_parameter_opens_prompt_not_expand() {
        let config = tab_config();
        let decision = run_with(";killport", TriggerKey::Tab, &config, &killport_index());
        assert_eq!(
            decision,
            Decision::Prompt {
                snippet_id: "s-killport".into(),
                delete_chars: ";killport".chars().count(),
                prefill: HashMap::new(),
                typed: ";killport".into(),
            }
        );
    }

    #[test]
    fn invalid_inline_argument_opens_prompt_with_typed_value_preserved() {
        let config = tab_config();
        let decision = run_with(";killport abc", TriggerKey::Tab, &config, &killport_index());
        let mut expected_prefill = HashMap::new();
        expected_prefill.insert("port".to_string(), "abc".to_string());
        assert_eq!(
            decision,
            Decision::Prompt {
                snippet_id: "s-killport".into(),
                delete_chars: ";killport abc".chars().count(),
                prefill: expected_prefill,
                typed: ";killport abc".into(),
            }
        );
    }

    #[test]
    fn multi_parameter_quoted_inline_expansion() {
        let config = tab_config();
        let decision = run_with(
            ";hello \"John Smith\" Clipet",
            TriggerKey::Tab,
            &config,
            &hello_index(),
        );
        assert_eq!(
            decision,
            Decision::Expand {
                snippet_id: "s-hello".into(),
                delete_chars: ";hello \"John Smith\" Clipet".chars().count(),
                insert: "Hello John Smith, your project Clipet is ready.".into(),
            }
        );
    }

    #[test]
    fn unrelated_text_after_a_static_snippet_does_not_expand_it() {
        // Guards against the backward scan loosening plain static-snippet
        // matching: `;name` is static (no parameters), so trailing text
        // after it must behave exactly as before — no match at all.
        let config = tab_config();
        assert_eq!(
            run_with(";name is here", TriggerKey::Tab, &config, &index(true)),
            Decision::Pass
        );
    }

    #[test]
    fn positional_args_follow_parameter_order_not_template_order() {
        // A per-OS variant may use the placeholders in a different order;
        // `;cmd 1 2` must still mean a=1, b=2 everywhere.
        let text = |name: &str| Parameter {
            name: name.into(),
            kind: params::ParamType::Text { min_length: None, max_length: None, pattern: None },
            required: true,
            default: None,
        };
        let idx = SnippetIndex::new(
            [(
                "cmd",
                IndexedSnippet {
                    id: "s-cmd".into(),
                    content: "{{b}}-{{a}}".into(),
                    image: false,
                    parameters: vec![text("a"), text("b")],
                },
            )],
            true,
        );
        assert_eq!(
            run_with(";cmd 1 2", TriggerKey::Tab, &tab_config(), &idx),
            Decision::Expand { snippet_id: "s-cmd".into(), delete_chars: 8, insert: "2-1".into() }
        );
    }

    #[test]
    fn expanding_with_args_resets_the_tracked_line() {
        let config = tab_config();
        let mut engine = Engine::new();
        type_word(&mut engine, ";killport 5173", &config, &killport_index());
        engine.feed(KeyInput::Trigger(TriggerKey::Tab), &config, &killport_index());
        // A later plain Tab with nothing typed must not re-match anything.
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Tab), &config, &killport_index()),
            Decision::Pass
        );
    }

    // ---- Cursor-aware detection: out-of-order edits, field reconciliation ----

    const NAME: &str = "KimLeang";

    fn known(before: &str, at_start: bool, after: Option<char>) -> FieldText {
        FieldText::Known(CaretContext {
            before: before.into(),
            at_start,
            after,
        })
    }

    fn expand_name(delete_chars: usize) -> Decision {
        Decision::Expand {
            snippet_id: "s-name".into(),
            delete_chars,
            insert: NAME.into(),
        }
    }

    /// Feeds keys; `|` stands for a navigation key (Home, End, arrows, a
    /// click), `^` for a paste and `<` for Backspace.
    fn keys(engine: &mut Engine, script: &str, config: &EngineConfig, idx: &SnippetIndex) {
        for ch in script.chars() {
            let input = match ch {
                '|' => KeyInput::Reset,
                '^' => KeyInput::Edit,
                '<' => KeyInput::Backspace,
                _ => {
                    let s = ch.to_string();
                    engine.feed(KeyInput::Text(&s), config, idx);
                    continue;
                }
            };
            engine.feed(input, config, idx);
        }
    }

    /// Runs a key script, presses `key`, and settles the decision against
    /// what the field reports — what the platform hook does.
    fn settle(script: &str, key: TriggerKey, config: &EngineConfig, field: &FieldText) -> Decision {
        let idx = index(true);
        let mut engine = Engine::new();
        keys(&mut engine, script, config, &idx);
        let anchored = !engine.word_starts_line();
        let decision = engine.feed(KeyInput::Trigger(key), config, &idx);
        if !decision.swallows_trigger() {
            return Decision::Pass;
        }
        reconcile(decision, Some(key), anchored, field, config, &idx).decision
    }

    #[test]
    fn acceptance_prefix_inserted_with_home_and_end() {
        // Type `name`, Home, `;`, End, Space → the field holds `;name`.
        let config = EngineConfig::default();
        assert_eq!(
            settle("name|;|", TriggerKey::Space, &config, &known(";name", true, None)),
            expand_name(5)
        );
        // Same after earlier text on the line: only `;name` is replaced.
        assert_eq!(
            settle("name|;|", TriggerKey::Space, &config, &known("hello ;name", true, None)),
            expand_name(5)
        );
    }

    #[test]
    fn normal_typing_needs_no_field() {
        let config = EngineConfig::default();
        // The keystrokes already know the answer; an unreadable field changes
        // nothing.
        assert_eq!(
            settle("hello ;name", TriggerKey::Space, &config, &FieldText::Unavailable),
            expand_name(5)
        );
    }

    #[test]
    fn typo_fixed_with_arrows_and_backspace() {
        // `;nmae`, Left ×2, Backspace ×2, `am`, End, Space.
        let config = EngineConfig::default();
        assert_eq!(
            settle(";nmae|<<am|", TriggerKey::Space, &config, &known(";name", true, None)),
            expand_name(5)
        );
        // Backspace alone (no cursor movement) is handled by the keystrokes.
        assert_eq!(
            settle(";nmae<<<ame", TriggerKey::Space, &config, &FieldText::Unavailable),
            expand_name(5)
        );
    }

    #[test]
    fn pasted_trigger_is_checked_in_the_field() {
        let config = EngineConfig::default();
        assert_eq!(
            settle("^", TriggerKey::Space, &config, &known("say ;name", true, None)),
            expand_name(5)
        );
        // Pasting `;` then typing the keyword.
        assert_eq!(
            settle("^name", TriggerKey::Space, &config, &known(";name", true, None)),
            expand_name(5)
        );
    }

    #[test]
    fn selection_replaced_then_completed() {
        // A selection replaced by `;` leaves whatever the field reports; it
        // is matched by the same rules (`;e` is not a shortcut).
        let config = EngineConfig::default();
        assert_eq!(
            settle("name|;|", TriggerKey::Space, &config, &known(";e", true, None)),
            Decision::Pass
        );
    }

    #[test]
    fn field_word_boundaries() {
        let config = EngineConfig::default();
        for (before, expected) in [
            ("abc;name", Decision::Pass),
            ("abc ;name", expand_name(5)),
            ("line one\n;name", expand_name(5)),
            ("(;name", Decision::Pass),
            (";name ", Decision::Pass),
            ("", Decision::Pass),
        ] {
            assert_eq!(
                settle("name|;|", TriggerKey::Space, &config, &known(before, true, None)),
                expected,
                "{before:?}"
            );
        }
    }

    #[test]
    fn caret_inside_a_word_never_expands() {
        let config = EngineConfig::default();
        // `;na|me`
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";na", true, Some('m'))),
            Decision::Pass
        );
        // `;name|x`, even with the keystrokes certain about `;name`.
        assert_eq!(
            settle(" ;name", TriggerKey::Space, &config, &known(" ;name", true, Some('x'))),
            Decision::Pass
        );
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";name", true, Some('é'))),
            Decision::Pass
        );
        // Punctuation or whitespace after the caret is a boundary.
        for after in [')', ',', ' ', '\n'] {
            assert_eq!(
                settle("|;|", TriggerKey::Space, &config, &known(";name", true, Some(after))),
                expand_name(5),
                "{after:?}"
            );
        }
    }

    #[test]
    fn unknown_or_partial_triggers_in_the_field_pass() {
        let config = EngineConfig::default();
        for before in [";unknown", ";nam", ";name2", "name", ";"] {
            assert_eq!(
                settle("|;|", TriggerKey::Space, &config, &known(before, true, None)),
                Decision::Pass,
                "{before:?}"
            );
        }
    }

    #[test]
    fn several_triggers_only_the_last_word_expands() {
        let config = EngineConfig::default();
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";sig ;name", true, None)),
            expand_name(5)
        );
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";name ;sig", true, None)),
            Decision::Expand {
                snippet_id: "s-sig".into(),
                delete_chars: 4,
                insert: "Best,\nKim".into()
            }
        );
    }

    #[test]
    fn unicode_text_around_the_trigger() {
        let config = EngineConfig::default();
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known("សួស្តី ;name", true, None)),
            expand_name(5)
        );
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known("日本語 ;name", true, Some('。'))),
            expand_name(5)
        );
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known("日本語;name", true, None)),
            Decision::Pass
        );
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";이름", true, None)),
            Decision::Pass
        );
        // Emoji (two UTF-16 units) before the word don't shift the count.
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known("👋 ;name", true, None)),
            expand_name(5)
        );
    }

    #[test]
    fn a_truncated_read_never_treats_its_first_word_as_a_start() {
        let config = EngineConfig::default();
        // The read began mid-text: `;name` may really be `x;name`.
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";name", false, None)),
            Decision::Pass
        );
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known("xx ;name", false, None)),
            expand_name(5)
        );
        // More than the window: only the tail is used.
        let long = format!("{} ;name", "a".repeat(MAX_LINE_CHARS * 2));
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(&long, true, None)),
            expand_name(5)
        );
    }

    #[test]
    fn space_and_tab_follow_the_trigger_mode_through_the_field() {
        let tab = EngineConfig {
            mode: TriggerMode::Tab,
            ..EngineConfig::default()
        };
        let field = known(";name", true, None);
        assert_eq!(settle("name|;|", TriggerKey::Tab, &tab, &field), expand_name(5));
        // Space isn't a trigger in Tab mode: nothing is checked or swallowed.
        assert_eq!(settle("name|;|", TriggerKey::Space, &tab, &field), Decision::Pass);
        let idx = index(true);
        let mut engine = Engine::new();
        keys(&mut engine, "name|;|", &EngineConfig::default(), &idx);
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Tab), &EngineConfig::default(), &idx),
            Decision::Pass
        );
    }

    #[test]
    fn escape_and_parameters_work_from_the_field() {
        let config = EngineConfig::default();
        assert_eq!(
            settle("|;|", TriggerKey::Space, &config, &known(";;name", true, None)),
            Decision::Escape {
                delete_chars: 6,
                insert: ";name ".into()
            }
        );
        let idx = killport_index();
        let mut engine = Engine::new();
        keys(&mut engine, "killport 5173|;|", &tab_config(), &idx);
        let decision = engine.feed(KeyInput::Trigger(TriggerKey::Tab), &tab_config(), &idx);
        assert_eq!(decision, Decision::Check { key: TriggerKey::Tab });
        let field = known(";killport 5173", true, None);
        assert_eq!(
            reconcile(decision, Some(TriggerKey::Tab), false, &field, &tab_config(), &idx).decision,
            Decision::Expand {
                snippet_id: "s-killport".into(),
                delete_chars: 14,
                insert: "lsof -tiTCP:5173 -sTCP:LISTEN | xargs kill".into(),
            }
        );
    }

    #[test]
    fn protected_fields_never_expand() {
        let config = EngineConfig::default();
        assert_eq!(settle(";name", TriggerKey::Space, &config, &FieldText::Blocked), Decision::Pass);
        assert_eq!(settle(" ;name", TriggerKey::Space, &config, &FieldText::Blocked), Decision::Pass);
        assert_eq!(settle("name|;|", TriggerKey::Space, &config, &FieldText::Blocked), Decision::Pass);
        let undo = Decision::Undo {
            delete_chars: 8,
            insert: ";name ".into(),
            expect: NAME.into(),
        };
        let idx = index(true);
        assert_eq!(
            reconcile(undo, None, true, &FieldText::Blocked, &config, &idx).decision,
            Decision::Pass
        );
    }

    #[test]
    fn unreadable_fields_fall_back_to_keystrokes() {
        let config = EngineConfig::default();
        // Nothing known: never erase on a guess.
        assert_eq!(
            settle("name|;|", TriggerKey::Space, &config, &FieldText::Unavailable),
            Decision::Pass
        );
        // The word was typed after a reset: as before, assume it starts a word.
        assert_eq!(
            settle("|;name", TriggerKey::Space, &config, &FieldText::Unavailable),
            expand_name(5)
        );
    }

    #[test]
    fn a_stale_buffer_is_overruled_by_the_field() {
        let config = EngineConfig::default();
        // Typed `;name` after a click, but the field shows it glued to text.
        assert_eq!(
            settle("|;name", TriggerKey::Space, &config, &known("abc;name", true, None)),
            Decision::Pass
        );
        // Typed in one go after whitespace: a lagging accessibility tree
        // (still missing the last letter) doesn't block the expansion.
        assert_eq!(
            settle("hi ;name", TriggerKey::Space, &config, &known("hi ;nam", true, None)),
            expand_name(5)
        );
    }

    #[test]
    fn a_pending_prefix_is_checked_once() {
        let config = EngineConfig::default();
        let idx = index(true);
        let mut engine = Engine::new();
        keys(&mut engine, "name|;|", &config, &idx);
        assert!(matches!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Check { .. }
        ));
        // The hook settles it (here: Pass) and replays the Space.
        engine.reset();
        engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx);
        keys(&mut engine, "|more|", &config, &idx);
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Pass
        );
        // Switching apps forgets a pending prefix too.
        keys(&mut engine, ";|", &config, &idx);
        engine.forget();
        assert_eq!(
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx),
            Decision::Pass
        );
    }

    #[test]
    fn field_expansion_preserves_exact_spacing() {
        // The trigger key is swallowed and only the matched characters are
        // erased; the snippet's own suffix decides any trailing space.
        let config = EngineConfig::default();
        let idx = SnippetIndex::new(
            [(
                "name",
                IndexedSnippet {
                    id: "s".into(),
                    content: "Keo Kimleang".into(),
                    image: false,
                    parameters: Vec::new(),
                },
            )],
            true,
        );
        let mut engine = Engine::new();
        keys(&mut engine, "name|;|", &config, &idx);
        let decision = engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx);
        let field = known("Hi  ;name", true, Some('\n'));
        let r = reconcile(decision, Some(TriggerKey::Space), false, &field, &config, &idx);
        assert_eq!(
            r.decision,
            Decision::Expand {
                snippet_id: "s".into(),
                delete_chars: 5,
                insert: "Keo Kimleang".into()
            }
        );
        assert!(r.from_field);
        assert_eq!(r.replaced.as_deref(), Some(";name"));
    }

    fn undo_config() -> EngineConfig {
        EngineConfig {
            undo_backspace: true,
            ..EngineConfig::default()
        }
    }

    #[test]
    fn backspace_right_after_an_expansion_restores_the_shortcut() {
        let config = undo_config();
        let idx = index(true);
        let mut engine = Engine::new();
        keys(&mut engine, "Hi ;name", &config, &idx);
        assert_eq!(engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx), expand_name(5));
        let undo = engine.feed(KeyInput::Backspace, &config, &idx);
        assert_eq!(
            undo,
            Decision::Undo {
                delete_chars: NAME.len(),
                insert: ";name ".into(),
                expect: NAME.into()
            }
        );
        // The restored `;name ` is plain text now; a second Backspace just
        // deletes the space, and Space then expands again.
        assert_eq!(engine.feed(KeyInput::Backspace, &config, &idx), Decision::Pass);
        assert_eq!(engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx), expand_name(5));
    }

    #[test]
    fn undo_restores_a_tab_trigger_without_a_tab() {
        let config = EngineConfig {
            mode: TriggerMode::Tab,
            ..undo_config()
        };
        let idx = index(true);
        let mut engine = Engine::new();
        keys(&mut engine, ";name", &config, &idx);
        engine.feed(KeyInput::Trigger(TriggerKey::Tab), &config, &idx);
        assert!(matches!(
            engine.feed(KeyInput::Backspace, &config, &idx),
            Decision::Undo { insert, .. } if insert == ";name"
        ));
    }

    #[test]
    fn undo_is_off_by_default_and_only_immediate() {
        let idx = index(true);
        let mut engine = Engine::new();
        let off = EngineConfig::default();
        keys(&mut engine, ";name", &off, &idx);
        engine.feed(KeyInput::Trigger(TriggerKey::Space), &off, &idx);
        assert_eq!(engine.feed(KeyInput::Backspace, &off, &idx), Decision::Pass);

        let on = undo_config();
        for between in ["x", "|", "^"] {
            let mut engine = Engine::new();
            keys(&mut engine, ";name", &on, &idx);
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &on, &idx);
            keys(&mut engine, between, &on, &idx);
            assert_eq!(engine.feed(KeyInput::Backspace, &on, &idx), Decision::Pass, "{between:?}");
        }
    }

    #[test]
    fn undo_is_not_offered_when_backspace_count_is_uncertain() {
        let config = undo_config();
        // Multi-line content.
        let mut engine = Engine::new();
        let idx = index(true);
        keys(&mut engine, ";sig", &config, &idx);
        engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx);
        assert_eq!(engine.feed(KeyInput::Backspace, &config, &idx), Decision::Pass);
        // Combining marks / emoji.
        for content in ["ឈ្មោះ", "hi 👋", "e\u{301}"] {
            let idx = SnippetIndex::new(
                [(
                    "x",
                    IndexedSnippet {
                        id: "s".into(),
                        content: content.into(),
                        image: false,
                        parameters: Vec::new(),
                    },
                )],
                true,
            );
            let mut engine = Engine::new();
            keys(&mut engine, ";x", &config, &idx);
            engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx);
            assert_eq!(engine.feed(KeyInput::Backspace, &config, &idx), Decision::Pass, "{content:?}");
        }
    }

    #[test]
    fn undo_is_verified_against_the_field() {
        let config = undo_config();
        let idx = index(true);
        let undo = Decision::Undo {
            delete_chars: NAME.len(),
            insert: ";name ".into(),
            expect: NAME.into(),
        };
        let ok = known("Hi KimLeang", true, None);
        let moved = known("Hi KimLea", true, Some('n'));
        assert_eq!(reconcile(undo.clone(), None, true, &ok, &config, &idx).decision, undo);
        assert_eq!(reconcile(undo.clone(), None, true, &moved, &config, &idx).decision, Decision::Pass);
        assert_eq!(
            reconcile(undo.clone(), None, true, &FieldText::Unavailable, &config, &idx).decision,
            undo
        );
    }

    #[test]
    fn undo_works_after_a_field_based_expansion() {
        let config = undo_config();
        let idx = index(true);
        let mut engine = Engine::new();
        keys(&mut engine, "name|;|", &config, &idx);
        let decision = engine.feed(KeyInput::Trigger(TriggerKey::Space), &config, &idx);
        let field = known("x ;name", true, None);
        let r = reconcile(decision, Some(TriggerKey::Space), false, &field, &config, &idx);
        engine.adopt(Some(TriggerKey::Space), &r);
        assert_eq!(
            engine.feed(KeyInput::Backspace, &config, &idx),
            Decision::Undo {
                delete_chars: NAME.len(),
                insert: ";name ".into(),
                expect: NAME.into()
            }
        );
    }
}

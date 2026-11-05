//! Platform-independent text-expansion logic.
//!
//! The native keyboard hook (see `macos.rs`) classifies each key-down into a
//! [`KeyInput`] and feeds it here. The engine keeps, in memory only, the short
//! stretch of text it knows sits left of the caret (at most 256 characters,
//! cleared on every click, arrow key or app switch) — enough to find the
//! current word even after Backspace — and decides synchronously (inside the
//! hook) whether the trigger key must be swallowed and replaced.

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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            prefix: ';',
            mode: TriggerMode::Space,
            escape_enabled: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSnippet {
    pub id: String,
    pub content: String,
    /// An image snippet: `content` is unused and the hook pastes the image.
    pub image: bool,
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
    Trigger(TriggerKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Let the key through untouched.
    Pass,
    /// Swallow the trigger key, erase `delete_chars` characters, then insert
    /// `insert` (which already includes the trailing space for Space).
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
}

impl Decision {
    pub fn swallows_trigger(&self) -> bool {
        !matches!(self, Decision::Pass)
    }
}


/// How much known text left of the caret is remembered (in memory only).
const MAX_LINE_CHARS: usize = 256;

pub struct Engine {
    /// Text known to sit immediately left of the caret: what was typed plus
    /// what Clipet inserted, since the last reset (click, arrow key, app
    /// switch…). Remembering inserted text lets the user Backspace over an
    /// expansion and retype a shortcut without losing track of the word.
    line: VecDeque<char>,
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
        }
    }

    pub fn reset(&mut self) {
        self.line.clear();
    }

    /// True when nothing Clipet saw precedes the current word, so whether the
    /// word is glued to earlier text (`abc;name`) is unknown from keystrokes
    /// alone. The hook then asks the focused field before replacing.
    pub fn word_starts_line(&self) -> bool {
        self.word_len() == self.line.len()
    }

    /// Length of the word left of the caret (for diagnostics).
    pub fn word_chars(&self) -> usize {
        self.word_len()
    }

    fn word_len(&self) -> usize {
        self.line
            .iter()
            .rev()
            .take_while(|c| !c.is_whitespace())
            .count()
    }

    fn push(&mut self, ch: char) {
        if self.line.len() == MAX_LINE_CHARS {
            self.line.pop_front();
        }
        self.line.push_back(ch);
    }

    /// Feed one key press. Only a `Trigger` can produce a non-`Pass` decision.
    pub fn feed(
        &mut self,
        input: KeyInput<'_>,
        config: &EngineConfig,
        index: &SnippetIndex,
    ) -> Decision {
        match input {
            KeyInput::Text(text) => {
                for ch in text.chars() {
                    if ch.is_control() && !ch.is_whitespace() {
                        self.line.clear();
                    } else {
                        self.push(ch);
                    }
                }
                Decision::Pass
            }
            KeyInput::Backspace => {
                // Past the start of `line` the deleted text was never seen;
                // `word_starts_line` reports that uncertainty.
                self.line.pop_back();
                Decision::Pass
            }
            KeyInput::Reset => {
                self.reset();
                Decision::Pass
            }
            KeyInput::Trigger(key) => {
                let decision = if config.mode.accepts(key) {
                    self.evaluate(key, config, index)
                } else {
                    Decision::Pass
                };
                // Track what the text will look like once the decision is
                // carried out. (If the hook later has to skip a replacement,
                // it resets the engine.)
                match &decision {
                    Decision::Pass if key == TriggerKey::Space => self.push(' '),
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
                    } => {
                        for _ in 0..*delete_chars {
                            self.line.pop_back();
                        }
                        for ch in insert.chars() {
                            self.push(ch);
                        }
                    }
                    // The text around a pasted image is unknown.
                    Decision::ExpandImage { .. } => self.reset(),
                }
                decision
            }
        }
    }

    fn evaluate(&self, key: TriggerKey, config: &EngineConfig, index: &SnippetIndex) -> Decision {
        let word_chars = self.word_len();
        if word_chars == 0 || word_chars > MAX_WORD_CHARS {
            return Decision::Pass;
        }
        let word: String = self.line.iter().skip(self.line.len() - word_chars).collect();
        // Replacement erases the word with one Backspace per character. That
        // is only exact for single-code-point characters; combining marks
        // (Khmer, Hangul jamo, accents) can erase differently per app, so such
        // words are left untouched rather than risk corrupting the text.
        if !word.is_ascii() {
            return Decision::Pass;
        }
        let trailing = if key == TriggerKey::Space { " " } else { "" };
        let prefix = config.prefix;
        let Some(after_prefix) = word.strip_prefix(prefix) else {
            return Decision::Pass;
        };

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
            Some(snippet) if snippet.image => Decision::ExpandImage {
                snippet_id: snippet.id.clone(),
                delete_chars: word_chars,
            },
            Some(snippet) => Decision::Expand {
                snippet_id: snippet.id.clone(),
                delete_chars: word_chars,
                insert: format!("{}{trailing}", snippet.content),
            },
            None => Decision::Pass,
        }
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
                    },
                ),
                (
                    "sig",
                    IndexedSnippet {
                        id: "s-sig".into(),
                        content: "Best,\nKim".into(),
                        image: false,
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
    fn space_expands_with_one_trailing_space() {
        let decision = run("Hello ;name", TriggerKey::Space, &EngineConfig::default());
        assert_eq!(
            decision,
            Decision::Expand {
                snippet_id: "s-name".into(),
                delete_chars: 5,
                insert: "KimLeang ".into()
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
        // "Hi KimLeang " → delete "KimLeang " → "Hi " → retype.
        for _ in 0.."KimLeang ".len() {
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
        // With known whitespace before it, no verification is needed.
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
                insert: "Best,\nKim ".into()
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
}

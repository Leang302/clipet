//! Parameterized-snippet core: placeholder parsing, argument tokenizing,
//! type validation, and deterministic template rendering.
//!
//! Pure, synchronous, dependency-free of any keyboard/clipboard/UI state so
//! it can be unit-tested without a running app (spec Part C2/J). Nothing
//! here executes a generated value as code, a shell command, or a template
//! expression — substitution is always literal string replacement.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A `{{name}}` placeholder's configured type and validation rules.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ParamType {
    Text {
        min_length: Option<u32>,
        max_length: Option<u32>,
        /// A regular expression the value must fully match, if set.
        pattern: Option<String>,
    },
    Integer {
        min: Option<i64>,
        max: Option<i64>,
    },
    Url {
        /// Allowed schemes, e.g. `["http", "https"]`. Any scheme if `None`.
        schemes: Option<Vec<String>>,
    },
    /// Output format is `strftime`-style; documented default is `%Y-%m-%d`.
    Date {
        format: Option<String>,
    },
    Dropdown {
        options: Vec<String>,
    },
}

impl ParamType {
    fn date_format(&self) -> &str {
        match self {
            ParamType::Date { format: Some(f) } => f,
            _ => "%Y-%m-%d",
        }
    }
}

/// One parameter's full configuration, as stored alongside a snippet.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    #[serde(flatten)]
    pub kind: ParamType,
    pub required: bool,
    pub default: Option<String>,
}

/// Parses a snippet's stored `parameters` column (a JSON array). Corrupt or
/// unrecognized JSON is treated as "no parameters" rather than failing the
/// whole snippet load — matches the store's general backward-compatibility
/// stance (spec Part I).
pub fn parse_parameters(json: &str) -> Vec<Parameter> {
    serde_json::from_str(json).unwrap_or_default()
}

pub fn parameters_to_json(params: &[Parameter]) -> String {
    serde_json::to_string(params).unwrap_or_else(|_| "[]".to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamError {
    /// The template itself is malformed (unterminated `{{`, empty/invalid name).
    MalformedTemplate(String),
    /// An invocation argument could not be tokenized (e.g. unterminated quote).
    MalformedArguments(String),
    /// Required parameters with no supplied value and no default.
    MissingRequired(Vec<String>),
    /// A supplied value failed a parameter's type/range/pattern validation.
    InvalidValue { name: String, reason: String },
    /// Rendering referenced a name with no value and no default — should be
    /// unreachable if `validate_all` ran first; guards against silent gaps.
    MissingValue(String),
    /// More positional arguments were supplied than the template declares.
    TooManyArguments { expected: usize, got: usize },
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamError::MalformedTemplate(m) => write!(f, "malformed template: {m}"),
            ParamError::MalformedArguments(m) => write!(f, "malformed arguments: {m}"),
            ParamError::MissingRequired(names) => {
                write!(f, "missing required parameter(s): {}", names.join(", "))
            }
            ParamError::InvalidValue { name, reason } => {
                write!(f, "invalid value for '{name}': {reason}")
            }
            ParamError::MissingValue(name) => write!(f, "no value for '{name}'"),
            ParamError::TooManyArguments { expected, got } => {
                write!(f, "expected at most {expected} argument(s), got {got}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Text(String),
    Param(String),
}

/// Splits a template into literal text and `{{name}}` references, honoring
/// `\{{name}}` as an escaped literal (the backslash is dropped, `{{name}}`
/// is emitted verbatim and not treated as a placeholder).
fn scan_segments(template: &str) -> Result<Vec<Segment>, ParamError> {
    let mut segments = Vec::new();
    let mut rest = template;
    let mut literal = String::new();
    loop {
        let Some(idx) = rest.find("{{") else {
            literal.push_str(rest);
            break;
        };
        let escaped = idx > 0 && rest.as_bytes()[idx - 1] == b'\\';
        let before = &rest[..if escaped { idx - 1 } else { idx }];
        literal.push_str(before);

        let after_open = &rest[idx + 2..];
        let Some(close) = after_open.find("}}") else {
            return Err(ParamError::MalformedTemplate(
                "unterminated '{{' placeholder".into(),
            ));
        };
        let name_raw = &after_open[..close];

        if escaped {
            literal.push_str("{{");
            literal.push_str(name_raw);
            literal.push_str("}}");
        } else {
            let name = name_raw.trim();
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Err(ParamError::MalformedTemplate(format!(
                    "invalid placeholder name '{{{{{name_raw}}}}}'"
                )));
            }
            segments.push(Segment::Text(std::mem::take(&mut literal)));
            segments.push(Segment::Param(name.to_string()));
        }
        rest = &after_open[close + 2..];
    }
    segments.push(Segment::Text(literal));
    Ok(segments)
}

/// Unique placeholder names in first-occurrence order. Repeated placeholders
/// (e.g. `{{name}}` used twice) collapse to one entry, matching the
/// requirement that they share one parameter configuration and one value.
pub fn placeholders_in_order(template: &str) -> Result<Vec<String>, ParamError> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for seg in scan_segments(template)? {
        if let Segment::Param(name) = seg {
            if seen.insert(name.clone()) {
                order.push(name);
            }
        }
    }
    Ok(order)
}

/// Tokenizes an invocation's argument text (everything after the keyword),
/// splitting on whitespace while honoring double-quoted multi-word
/// arguments and `\"`/`\\` escapes inside them. Deterministic, no shell
/// semantics beyond quoting/escaping.
pub fn tokenize_arguments(input: &str) -> Result<Vec<String>, ParamError> {
    let mut args = Vec::new();
    let mut chars = input.chars().peekable();

    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let mut current = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => {
                        return Err(ParamError::MalformedArguments(
                            "unterminated quoted argument".into(),
                        ))
                    }
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some('"') => current.push('"'),
                        Some('\\') => current.push('\\'),
                        Some(other) => {
                            current.push('\\');
                            current.push(other);
                        }
                        None => {
                            return Err(ParamError::MalformedArguments(
                                "trailing backslash in quoted argument".into(),
                            ))
                        }
                    },
                    Some(c) => current.push(c),
                }
            }
            // `"foo"bar` glues onto one argument, same as common shell rules.
            while matches!(chars.peek(), Some(c) if !c.is_whitespace()) {
                current.push(chars.next().unwrap());
            }
        } else {
            while matches!(chars.peek(), Some(c) if !c.is_whitespace()) {
                current.push(chars.next().unwrap());
            }
        }
        args.push(current);
    }
    Ok(args)
}

/// Validates one raw value against its parameter's type, returning the
/// normalized value to substitute.
fn validate_value(param: &Parameter, raw: &str) -> Result<String, ParamError> {
    let fail = |reason: String| ParamError::InvalidValue {
        name: param.name.clone(),
        reason,
    };
    match &param.kind {
        ParamType::Text {
            min_length,
            max_length,
            pattern,
        } => {
            let len = raw.chars().count() as u32;
            if let Some(min) = min_length {
                if len < *min {
                    return Err(fail(format!("must be at least {min} characters")));
                }
            }
            if let Some(max) = max_length {
                if len > *max {
                    return Err(fail(format!("must be at most {max} characters")));
                }
            }
            if let Some(pattern) = pattern {
                let re = Regex::new(&format!("^(?:{pattern})$"))
                    .map_err(|e| fail(format!("invalid pattern configured: {e}")))?;
                if !re.is_match(raw) {
                    return Err(fail("does not match the required pattern".into()));
                }
            }
            Ok(raw.to_string())
        }
        ParamType::Integer { min, max } => {
            let n: i64 = raw
                .trim()
                .parse()
                .map_err(|_| fail(format!("'{raw}' is not a whole number")))?;
            if let Some(min) = min {
                if n < *min {
                    return Err(fail(format!("must be at least {min}")));
                }
            }
            if let Some(max) = max {
                if n > *max {
                    return Err(fail(format!("must be at most {max}")));
                }
            }
            Ok(n.to_string())
        }
        ParamType::Url { schemes } => {
            let (scheme, remainder) = raw
                .split_once("://")
                .ok_or_else(|| fail("not a valid URL (missing scheme, e.g. 'https://')".into()))?;
            if scheme.is_empty() || remainder.is_empty() {
                return Err(fail("not a valid URL".into()));
            }
            if !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            {
                return Err(fail("URL scheme contains invalid characters".into()));
            }
            if let Some(allowed) = schemes {
                if !allowed.iter().any(|s| s.eq_ignore_ascii_case(scheme)) {
                    return Err(fail(format!(
                        "scheme '{scheme}' is not allowed (allowed: {})",
                        allowed.join(", ")
                    )));
                }
            }
            Ok(raw.to_string())
        }
        ParamType::Date { .. } => {
            let format = param.kind.date_format();
            chrono::NaiveDate::parse_from_str(raw, format)
                .map_err(|_| fail(format!("does not match the expected format '{format}'")))?;
            Ok(raw.to_string())
        }
        ParamType::Dropdown { options } => {
            if options.iter().any(|o| o == raw) {
                Ok(raw.to_string())
            } else {
                Err(fail(format!(
                    "'{raw}' is not one of: {}",
                    options.join(", ")
                )))
            }
        }
    }
}

/// Outcome of matching supplied values against a snippet's configured
/// parameters: either every value is present and valid, or some are
/// missing/invalid and the caller (inline expansion vs. popup) decides what
/// to do next.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedValues {
    pub values: HashMap<String, String>,
}

/// Validates and fills in defaults for every configured parameter from a
/// map of supplied raw values (by name). Returns every missing *required*
/// parameter together rather than failing on the first one, so a popup can
/// present them all at once.
pub fn validate_all(
    params: &[Parameter],
    supplied: &HashMap<String, String>,
) -> Result<ValidatedValues, ParamError> {
    let mut values = HashMap::new();
    let mut missing = Vec::new();

    for param in params {
        let raw = supplied
            .get(&param.name)
            .filter(|v| !v.is_empty())
            .or(param.default.as_ref());
        match raw {
            Some(raw) => {
                let validated = validate_value(param, raw)?;
                values.insert(param.name.clone(), validated);
            }
            None => {
                if param.required {
                    missing.push(param.name.clone());
                } else {
                    values.insert(param.name.clone(), String::new());
                }
            }
        }
    }

    if !missing.is_empty() {
        return Err(ParamError::MissingRequired(missing));
    }
    Ok(ValidatedValues { values })
}

/// Per-field validation for interactive UIs (the parameter popup, the
/// editor's test panel): every parameter ends up with either a validated
/// value or a human-readable error, so all problems show at once next to
/// their own fields instead of one at a time.
pub fn validate_fields(
    params: &[Parameter],
    supplied: &HashMap<String, String>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut values = HashMap::new();
    let mut errors = HashMap::new();
    for param in params {
        let raw = supplied
            .get(&param.name)
            .filter(|v| !v.is_empty())
            .or(param.default.as_ref());
        match raw {
            Some(raw) => match validate_value(param, raw) {
                Ok(v) => {
                    values.insert(param.name.clone(), v);
                }
                Err(ParamError::InvalidValue { reason, .. }) => {
                    errors.insert(param.name.clone(), reason);
                }
                Err(e) => {
                    errors.insert(param.name.clone(), e.to_string());
                }
            },
            None if param.required => {
                errors.insert(param.name.clone(), "Required".to_string());
            }
            None => {
                values.insert(param.name.clone(), String::new());
            }
        }
    }
    (values, errors)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Preview {
    /// The exact text that would be inserted; `None` while any field has an error.
    pub text: Option<String>,
    /// Parameter name → what's wrong with its value.
    pub errors: HashMap<String, String>,
}

/// Validates `supplied` field by field and, only when everything is valid,
/// renders the template. `Err` is reserved for a malformed template.
pub fn preview(
    template: &str,
    params: &[Parameter],
    supplied: &HashMap<String, String>,
) -> Result<Preview, ParamError> {
    scan_segments(template)?;
    let (values, errors) = validate_fields(params, supplied);
    let text = if errors.is_empty() {
        Some(render(template, &values)?)
    } else {
        None
    };
    Ok(Preview { text, errors })
}

/// Maps positional invocation arguments onto parameters in template order.
/// Deterministic: argument *i* always fills the *i*-th placeholder as it
/// first appears in the template, regardless of parameter type.
pub fn map_positional_args(
    order: &[String],
    args: &[String],
) -> Result<HashMap<String, String>, ParamError> {
    if args.len() > order.len() {
        return Err(ParamError::TooManyArguments {
            expected: order.len(),
            got: args.len(),
        });
    }
    Ok(order
        .iter()
        .cloned()
        .zip(args.iter().cloned())
        .collect::<HashMap<_, _>>())
}

/// Renders the final text: literal segments pass through unchanged, each
/// `{{name}}` is replaced with its validated value with no surrounding
/// whitespace added or removed. The only mutation beyond substitution is
/// dropping the backslash of an escaped `\{{...}}` (handled in `scan_segments`).
pub fn render(template: &str, values: &HashMap<String, String>) -> Result<String, ParamError> {
    let segments = scan_segments(template)?;
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Text(t) => out.push_str(&t),
            Segment::Param(name) => {
                let v = values
                    .get(&name)
                    .ok_or_else(|| ParamError::MissingValue(name.clone()))?;
                out.push_str(v);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(name: &str, required: bool) -> Parameter {
        Parameter {
            name: name.to_string(),
            kind: ParamType::Text {
                min_length: None,
                max_length: None,
                pattern: None,
            },
            required,
            default: None,
        }
    }

    fn int(name: &str, min: Option<i64>, max: Option<i64>) -> Parameter {
        Parameter {
            name: name.to_string(),
            kind: ParamType::Integer { min, max },
            required: true,
            default: None,
        }
    }

    #[test]
    fn static_snippet_with_no_placeholders_renders_unchanged() {
        assert_eq!(placeholders_in_order("plain text, no params").unwrap(), Vec::<String>::new());
        assert_eq!(
            render("plain text, no params", &HashMap::new()).unwrap(),
            "plain text, no params"
        );
    }

    #[test]
    fn single_parameter_inline_expansion() {
        let template = "http://localhost:{{port}}";
        let order = placeholders_in_order(template).unwrap();
        assert_eq!(order, vec!["port".to_string()]);
        let args = tokenize_arguments("5173").unwrap();
        let supplied = map_positional_args(&order, &args).unwrap();
        let params = vec![int("port", Some(1), Some(65535))];
        let validated = validate_all(&params, &supplied).unwrap();
        assert_eq!(render(template, &validated.values).unwrap(), "http://localhost:5173");
    }

    #[test]
    fn exact_whitespace_no_stray_spaces() {
        let template = "http://localhost:{{port}}";
        let args = tokenize_arguments("5173").unwrap();
        let order = placeholders_in_order(template).unwrap();
        let supplied = map_positional_args(&order, &args).unwrap();
        let params = vec![int("port", None, None)];
        let validated = validate_all(&params, &supplied).unwrap();
        let out = render(template, &validated.values).unwrap();
        assert_eq!(out, "http://localhost:5173");
        assert!(!out.starts_with(' ') && !out.ends_with(' '));
    }

    #[test]
    fn repeated_placeholder_shares_one_value() {
        let template = "{{name}} == {{name}}";
        let order = placeholders_in_order(template).unwrap();
        assert_eq!(order, vec!["name".to_string()]);
        let mut supplied = HashMap::new();
        supplied.insert("name".to_string(), "John".to_string());
        let params = vec![text("name", true)];
        let validated = validate_all(&params, &supplied).unwrap();
        assert_eq!(render(template, &validated.values).unwrap(), "John == John");
    }

    #[test]
    fn multi_parameter_quoted_arguments() {
        let template = "Hello {{name}}, your project {{project}} is ready.";
        let order = placeholders_in_order(template).unwrap();
        assert_eq!(order, vec!["name".to_string(), "project".to_string()]);
        let args = tokenize_arguments("\"John Smith\" Clipet").unwrap();
        assert_eq!(args, vec!["John Smith".to_string(), "Clipet".to_string()]);
        let supplied = map_positional_args(&order, &args).unwrap();
        let params = vec![text("name", true), text("project", true)];
        let validated = validate_all(&params, &supplied).unwrap();
        assert_eq!(
            render(template, &validated.values).unwrap(),
            "Hello John Smith, your project Clipet is ready."
        );
    }

    #[test]
    fn escaped_quotes_and_backslashes_in_arguments() {
        let args = tokenize_arguments(r#""say \"hi\"" plain\back"#).unwrap();
        assert_eq!(args, vec![r#"say "hi""#.to_string(), r#"plain\back"#.to_string()]);
    }

    #[test]
    fn missing_required_parameter_reports_all_names() {
        let template = "Hello {{name}}, deploy to {{environment}}.";
        let order = placeholders_in_order(template).unwrap();
        let supplied = map_positional_args(&order, &[]).unwrap();
        let params = vec![text("name", true), text("environment", true)];
        let err = validate_all(&params, &supplied).unwrap_err();
        assert_eq!(
            err,
            ParamError::MissingRequired(vec!["name".into(), "environment".into()])
        );
    }

    #[test]
    fn optional_parameter_defaults_to_empty_when_absent() {
        let params = vec![Parameter {
            name: "suffix".into(),
            kind: ParamType::Text {
                min_length: None,
                max_length: None,
                pattern: None,
            },
            required: false,
            default: None,
        }];
        let validated = validate_all(&params, &HashMap::new()).unwrap();
        assert_eq!(validated.values.get("suffix").unwrap(), "");
    }

    #[test]
    fn configured_default_value_is_used_when_absent() {
        let params = vec![Parameter {
            name: "env".into(),
            kind: ParamType::Dropdown {
                options: vec!["development".into(), "production".into()],
            },
            required: true,
            default: Some("development".into()),
        }];
        let validated = validate_all(&params, &HashMap::new()).unwrap();
        assert_eq!(validated.values.get("env").unwrap(), "development");
    }

    #[test]
    fn empty_supplied_value_falls_back_to_default_not_empty_string() {
        let params = vec![Parameter {
            name: "greeting".into(),
            kind: ParamType::Text {
                min_length: None,
                max_length: None,
                pattern: None,
            },
            required: true,
            default: Some("Hello".into()),
        }];
        let mut supplied = HashMap::new();
        supplied.insert("greeting".into(), "".into());
        let validated = validate_all(&params, &supplied).unwrap();
        assert_eq!(validated.values.get("greeting").unwrap(), "Hello");
    }

    #[test]
    fn invalid_integer_is_rejected_with_reason() {
        let params = vec![int("port", Some(1), Some(65535))];
        let mut supplied = HashMap::new();
        supplied.insert("port".into(), "abc".into());
        let err = validate_all(&params, &supplied).unwrap_err();
        assert!(matches!(err, ParamError::InvalidValue { name, .. } if name == "port"));
    }

    #[test]
    fn integer_out_of_range_is_rejected() {
        let params = vec![int("port", Some(1), Some(65535))];
        let mut supplied = HashMap::new();
        supplied.insert("port".into(), "99999".into());
        assert!(validate_all(&params, &supplied).is_err());
    }

    #[test]
    fn invalid_url_is_rejected() {
        let params = vec![Parameter {
            name: "site".into(),
            kind: ParamType::Url { schemes: Some(vec!["https".into()]) },
            required: true,
            default: None,
        }];
        let mut supplied = HashMap::new();
        supplied.insert("site".into(), "ftp://example.com".into());
        let err = validate_all(&params, &supplied).unwrap_err();
        assert!(matches!(err, ParamError::InvalidValue { .. }));
    }

    #[test]
    fn valid_url_with_allowed_scheme_passes() {
        let params = vec![Parameter {
            name: "site".into(),
            kind: ParamType::Url { schemes: Some(vec!["https".into()]) },
            required: true,
            default: None,
        }];
        let mut supplied = HashMap::new();
        supplied.insert("site".into(), "https://example.com".into());
        assert!(validate_all(&params, &supplied).is_ok());
    }

    #[test]
    fn dropdown_rejects_values_outside_options() {
        let params = vec![Parameter {
            name: "env".into(),
            kind: ParamType::Dropdown {
                options: vec!["development".into(), "staging".into(), "production".into()],
            },
            required: true,
            default: None,
        }];
        let mut supplied = HashMap::new();
        supplied.insert("env".into(), "qa".into());
        assert!(validate_all(&params, &supplied).is_err());
    }

    #[test]
    fn date_validates_against_documented_format() {
        let params = vec![Parameter {
            name: "due".into(),
            kind: ParamType::Date { format: None },
            required: true,
            default: None,
        }];
        let mut supplied = HashMap::new();
        supplied.insert("due".into(), "2026-11-07".into());
        assert!(validate_all(&params, &supplied).is_ok());
        supplied.insert("due".into(), "11/07/2026".into());
        assert!(validate_all(&params, &supplied).is_err());
    }

    #[test]
    fn unicode_parameter_values_round_trip() {
        let template = "សួស្តី {{name}}";
        let order = placeholders_in_order(template).unwrap();
        let mut supplied = HashMap::new();
        supplied.insert("name".into(), "កម្លាំង".into());
        let params = vec![text("name", true)];
        let validated = validate_all(&params, &supplied).unwrap();
        assert_eq!(render(template, &validated.values).unwrap(), "សួស្តី កម្លាំង");
        assert_eq!(order, vec!["name".to_string()]);
    }

    #[test]
    fn multiline_template_preserves_newlines() {
        let template = "Line one\n{{name}}\nLine three";
        let mut supplied = HashMap::new();
        supplied.insert("name".into(), "middle".into());
        assert_eq!(
            render(template, &supplied).unwrap(),
            "Line one\nmiddle\nLine three"
        );
    }

    #[test]
    fn unterminated_placeholder_is_a_malformed_template_error() {
        assert!(matches!(
            placeholders_in_order("Hello {{name"),
            Err(ParamError::MalformedTemplate(_))
        ));
    }

    #[test]
    fn empty_or_invalid_placeholder_name_is_rejected() {
        assert!(matches!(
            placeholders_in_order("{{}}"),
            Err(ParamError::MalformedTemplate(_))
        ));
        assert!(matches!(
            placeholders_in_order("{{bad name}}"),
            Err(ParamError::MalformedTemplate(_))
        ));
    }

    #[test]
    fn literal_escaped_placeholder_is_not_substituted() {
        let template = r"Use \{{literal}} as-is, but {{real}} expands.";
        let order = placeholders_in_order(template).unwrap();
        assert_eq!(order, vec!["real".to_string()]);
        let mut supplied = HashMap::new();
        supplied.insert("real".into(), "VALUE".into());
        assert_eq!(
            render(template, &supplied).unwrap(),
            "Use {{literal}} as-is, but VALUE expands."
        );
    }

    #[test]
    fn rendering_with_no_value_for_a_declared_placeholder_errors_instead_of_silently_blanking() {
        let template = "{{name}}";
        let err = render(template, &HashMap::new()).unwrap_err();
        assert_eq!(err, ParamError::MissingValue("name".into()));
    }

    #[test]
    fn too_many_positional_arguments_is_an_error_not_silent_drop() {
        let order = vec!["port".to_string()];
        let args = tokenize_arguments("5173 extra").unwrap();
        let err = map_positional_args(&order, &args).unwrap_err();
        assert_eq!(err, ParamError::TooManyArguments { expected: 1, got: 2 });
    }

    #[test]
    fn empty_quoted_argument_is_an_explicit_empty_string() {
        let args = tokenize_arguments(r#""" next"#).unwrap();
        assert_eq!(args, vec!["".to_string(), "next".to_string()]);
    }

    #[test]
    fn unterminated_quote_is_a_malformed_arguments_error() {
        assert!(matches!(
            tokenize_arguments("\"unterminated"),
            Err(ParamError::MalformedArguments(_))
        ));
    }

    #[test]
    fn preview_reports_every_field_error_at_once() {
        let params = vec![int("port", Some(1), Some(65535)), text("name", true)];
        let mut supplied = HashMap::new();
        supplied.insert("port".into(), "abc".into());
        let p = preview("{{name}}:{{port}}", &params, &supplied).unwrap();
        assert_eq!(p.text, None);
        assert!(p.errors.get("port").unwrap().contains("not a whole number"));
        assert_eq!(p.errors.get("name").unwrap(), "Required");
    }

    #[test]
    fn preview_renders_when_all_fields_valid() {
        let params = vec![int("port", Some(1), Some(65535))];
        let mut supplied = HashMap::new();
        supplied.insert("port".into(), " 8989 ".into());
        let p = preview("http://localhost:{{port}}", &params, &supplied).unwrap();
        assert_eq!(p.text.as_deref(), Some("http://localhost:8989"));
        assert!(p.errors.is_empty());
    }

    #[test]
    fn preview_rejects_malformed_template() {
        assert!(preview("{{oops", &[], &HashMap::new()).is_err());
    }

    #[test]
    fn killport_scenario_end_to_end() {
        let template = "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs kill";
        let order = placeholders_in_order(template).unwrap();
        let args = tokenize_arguments("5173").unwrap();
        let supplied = map_positional_args(&order, &args).unwrap();
        let params = vec![int("port", Some(1), Some(65535))];
        let validated = validate_all(&params, &supplied).unwrap();
        assert_eq!(
            render(template, &validated.values).unwrap(),
            "lsof -tiTCP:5173 -sTCP:LISTEN | xargs kill"
        );
    }

    #[test]
    fn invalid_killport_argument_is_rejected_before_rendering() {
        let template = "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs kill";
        let order = placeholders_in_order(template).unwrap();
        let args = tokenize_arguments("abc").unwrap();
        let supplied = map_positional_args(&order, &args).unwrap();
        let params = vec![int("port", Some(1), Some(65535))];
        assert!(validate_all(&params, &supplied).is_err());
    }
}

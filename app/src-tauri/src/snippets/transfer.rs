//! Snippet export / import (spec Part I) as a small versioned JSON file.
//!
//! Export covers text snippets (image snippets reference files in Clipet's
//! data folder and are skipped and counted). Import validates every record
//! with the same rules as the editor — keyword, content, whitespace setting,
//! parameters matching placeholders, per-OS variants — and never overwrites:
//! a keyword already in use (or repeated in the file) is skipped and reported.
//! Imported text is stored as data only; nothing in it is ever run.

use super::{SnippetService, engine, params, store};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::Manager;

pub const FORMAT: &str = "clipet-snippets";
pub const VERSION: u32 = 1;
/// Plenty for thousands of snippets; refuses accidentally picked huge files.
const MAX_IMPORT_BYTES: usize = 5 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct ExportFile {
    format: String,
    version: u32,
    snippets: Vec<ExportedSnippet>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct ExportedSnippet {
    #[serde(default)]
    name: String,
    keyword: String,
    content: String,
    #[serde(default)]
    suffix: Option<String>,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default)]
    parameters: Vec<params::Parameter>,
    #[serde(default)]
    variants: BTreeMap<String, String>,
}

fn enabled_default() -> bool {
    true
}

/// Serializes the text snippets; returns the JSON and how many image
/// snippets were left out.
pub fn export_json(snippets: &[store::Snippet]) -> Result<(String, usize, usize), String> {
    let mut skipped_images = 0;
    let exported: Vec<ExportedSnippet> = snippets
        .iter()
        .filter(|s| {
            let image = s.content_type == store::IMAGE;
            skipped_images += image as usize;
            !image
        })
        .map(|s| ExportedSnippet {
            name: s.name.clone(),
            keyword: s.keyword.clone(),
            content: s.content.clone(),
            suffix: Some(s.suffix.clone()),
            enabled: s.enabled,
            parameters: params::parse_parameters(&s.parameters),
            variants: store::parse_variants(&s.variants),
        })
        .collect();
    let count = exported.len();
    let json = serde_json::to_string_pretty(&ExportFile {
        format: FORMAT.to_string(),
        version: VERSION,
        snippets: exported,
    })
    .map_err(|e| e.to_string())?;
    Ok((json, count, skipped_images))
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Skipped {
    /// The keyword as written in the file (may be invalid).
    pub keyword: String,
    pub reason: String,
}

/// A record that passed validation, ready to insert.
#[derive(Debug, PartialEq)]
pub struct Accepted {
    pub name: String,
    pub keyword: String,
    pub content: String,
    pub suffix: String,
    pub enabled: bool,
    pub parameters_json: String,
    pub variants_json: String,
}

/// Validates a whole import file against the snippets that already exist.
/// `Err` only when the file itself isn't a readable Clipet export.
pub fn plan_import(
    json: &str,
    existing_keywords: &HashSet<String>,
    prefix: char,
) -> Result<(Vec<Accepted>, Vec<Skipped>), String> {
    if json.len() > MAX_IMPORT_BYTES {
        return Err("That file is too large to be a Clipet snippet export.".to_string());
    }
    let file: ExportFile = serde_json::from_str(json)
        .map_err(|e| format!("That file isn't a Clipet snippet export ({e})."))?;
    if file.format != FORMAT {
        return Err("That file isn't a Clipet snippet export.".to_string());
    }
    if file.version > VERSION {
        return Err("That export comes from a newer version of Clipet; update Clipet to import it.".to_string());
    }
    let mut taken: HashSet<String> = existing_keywords.iter().map(|k| k.to_lowercase()).collect();
    let mut accepted = Vec::new();
    let mut skipped = Vec::new();
    for record in file.snippets {
        let label = record.keyword.clone();
        match validate_record(record, &taken, prefix) {
            Ok(a) => {
                taken.insert(a.keyword.to_lowercase());
                accepted.push(a);
            }
            Err(reason) => skipped.push(Skipped { keyword: label, reason }),
        }
    }
    Ok((accepted, skipped))
}

fn validate_record(record: ExportedSnippet, taken: &HashSet<String>, prefix: char) -> Result<Accepted, String> {
    let keyword = record.keyword.trim();
    let keyword = keyword.strip_prefix(prefix).unwrap_or(keyword).to_string();
    engine::validate_keyword(&keyword, prefix)?;
    if taken.contains(&keyword.to_lowercase()) {
        return Err(format!("{prefix}{keyword} is already in use"));
    }
    if record.content.is_empty() {
        return Err("its content is empty".to_string());
    }
    if record.content.chars().count() > super::MAX_CONTENT_CHARS {
        return Err("its content is too long".to_string());
    }
    let suffix = record.suffix.unwrap_or_else(|| "exact".to_string());
    if !store::valid_suffix(&suffix) {
        return Err(format!("unknown whitespace setting \"{suffix}\""));
    }
    let parameters_json = super::validate_parameters(&record.content, &record.parameters)?;
    let variants_json = super::validate_variants(&record.variants, &record.parameters)?;
    let name = match record.name.trim() {
        "" => keyword.clone(),
        n => n.to_string(),
    };
    Ok(Accepted {
        name,
        keyword,
        content: record.content,
        suffix,
        enabled: record.enabled,
        parameters_json,
        variants_json,
    })
}

type Service<'a> = tauri::State<'a, Arc<SnippetService>>;

#[derive(Serialize)]
pub struct ExportResult {
    pub path: String,
    pub count: usize,
    pub skipped_images: usize,
}

/// Writes all text snippets to a new dated file in Downloads (never
/// overwriting an existing file) and returns where.
#[tauri::command]
pub fn snippets_export(service: Service) -> Result<ExportResult, String> {
    let snippets = store::list_snippets(&service.conn());
    let (json, count, skipped_images) = export_json(&snippets)?;
    let dir = service
        .app
        .path()
        .download_dir()
        .map_err(|e| format!("couldn't find the Downloads folder: {e}"))?;
    let path = free_path(&dir, &format!("clipet-snippets-{}", chrono::Local::now().format("%Y-%m-%d")));
    std::fs::write(&path, json).map_err(|e| format!("couldn't write the export: {e}"))?;
    Ok(ExportResult {
        path: path.to_string_lossy().into_owned(),
        count,
        skipped_images,
    })
}

fn free_path(dir: &std::path::Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.json"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}.json")))
        .find(|p| !p.exists())
        .expect("an unused file name exists")
}

#[derive(Serialize)]
pub struct ImportReport {
    pub imported: usize,
    pub skipped: Vec<Skipped>,
}

/// Imports the snippets in `json` (the file's text, read by the webview's own
/// file picker). Invalid or clashing records are skipped and reported.
#[tauri::command]
pub fn snippets_import(service: Service, json: String) -> Result<ImportReport, String> {
    let conn = service.conn();
    let prefix = service.live.read().unwrap().config.prefix;
    let existing: HashSet<String> = store::list_snippets(&conn).into_iter().map(|s| s.keyword).collect();
    let (accepted, mut skipped) = plan_import(&json, &existing, prefix)?;
    let now = super::now_ms();
    let mut imported = 0;
    for (i, a) in accepted.into_iter().enumerate() {
        let keyword = a.keyword.clone();
        let snippet = store::Snippet {
            id: format!("{}-{i}", super::new_id()),
            name: a.name,
            keyword: a.keyword,
            content: a.content,
            content_type: store::TEXT.to_string(),
            category: None,
            enabled: a.enabled,
            favorite: false,
            use_count: 0,
            created_at: now,
            updated_at: now,
            last_used_at: None,
            suffix: a.suffix,
            parameters: a.parameters_json,
            variants: a.variants_json,
        };
        match store::insert_snippet(&conn, &snippet) {
            Ok(()) => imported += 1,
            Err(e) => skipped.push(Skipped { keyword, reason: e }),
        }
    }
    drop(conn);
    if imported > 0 {
        service.reload();
    }
    Ok(ImportReport { imported, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(keyword: &str, content: &str, kind: &str, parameters: &str, variants: &str) -> store::Snippet {
        store::Snippet {
            id: keyword.into(),
            name: format!("N {keyword}"),
            keyword: keyword.into(),
            content: content.into(),
            content_type: kind.into(),
            category: None,
            enabled: true,
            favorite: false,
            use_count: 3,
            created_at: 1,
            updated_at: 1,
            last_used_at: None,
            suffix: "space".into(),
            parameters: parameters.into(),
            variants: variants.into(),
        }
    }

    #[test]
    fn export_then_import_round_trips_text_snippets_and_skips_images() {
        let port = r#"[{"name":"port","kind":"integer","min":1,"max":65535,"required":true,"default":null}]"#;
        let snippets = vec![
            snippet("killport", "lsof {{port}}", store::TEXT, port, r#"{"windows":"Stop {{port}}"}"#),
            snippet("sig", "Best,\nKim", store::TEXT, "[]", "{}"),
            snippet("logo", "logo.png", store::IMAGE, "[]", "{}"),
        ];
        let (json, count, images) = export_json(&snippets).unwrap();
        assert_eq!((count, images), (2, 1));

        let (accepted, skipped) = plan_import(&json, &HashSet::new(), ';').unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(accepted.len(), 2);
        let kp = &accepted[0];
        assert_eq!((kp.keyword.as_str(), kp.content.as_str(), kp.suffix.as_str()), ("killport", "lsof {{port}}", "space"));
        assert_eq!(params::parse_parameters(&kp.parameters_json).len(), 1);
        assert_eq!(kp.variants_json, r#"{"windows":"Stop {{port}}"}"#);
    }

    #[test]
    fn import_never_overwrites_and_reports_why() {
        let json = r#"{"format":"clipet-snippets","version":1,"snippets":[
            {"keyword":"sig","content":"new"},
            {"keyword":";fresh","content":"ok"},
            {"keyword":"fresh","content":"duplicate in file"},
            {"keyword":"bad key","content":"x"},
            {"keyword":"empty","content":""},
            {"keyword":"p","content":"Hi {{name}}"},
            {"keyword":"w","content":"x","suffix":"tab"}
        ]}"#;
        let existing: HashSet<String> = ["SIG".to_string()].into_iter().collect();
        let (accepted, skipped) = plan_import(json, &existing, ';').unwrap();
        assert_eq!(accepted.iter().map(|a| a.keyword.as_str()).collect::<Vec<_>>(), vec!["fresh"]);
        assert_eq!(accepted[0].name, "fresh");
        let reasons: Vec<(&str, &str)> = skipped.iter().map(|s| (s.keyword.as_str(), s.reason.as_str())).collect();
        assert_eq!(reasons.len(), 6, "{reasons:?}");
        assert!(reasons[0].1.contains("already in use"));
        assert!(reasons[1].1.contains("already in use"));
        assert!(reasons[2].1.contains("letters"), "{reasons:?}");
        assert!(reasons[3].1.contains("empty"));
        assert!(reasons[4].1.contains("{{name}}"), "{reasons:?}");
        assert!(reasons[5].1.contains("whitespace"));
    }

    #[test]
    fn unreadable_or_foreign_files_are_rejected_whole() {
        let none = HashSet::new();
        assert!(plan_import("not json", &none, ';').is_err());
        assert!(plan_import(r#"{"format":"other","version":1,"snippets":[]}"#, &none, ';').is_err());
        assert!(plan_import(r#"{"format":"clipet-snippets","version":99,"snippets":[]}"#, &none, ';')
            .unwrap_err()
            .contains("newer version"));
    }
}

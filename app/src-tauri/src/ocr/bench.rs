//! OCR accuracy benchmark (macOS). Not part of the normal test run:
//!   cargo test --release ocr_benchmark -- --ignored --nocapture
//! Renders a fixed set of images with known text, optionally adds real
//! screenshots listed in CLIPET_OCR_BENCH (lines of `path|phrase|phrase…`),
//! and scores every engine configuration × preprocessing on the same inputs.

use super::macos::render_text_image;
use super::{OcrOptions, Preprocess, normalize_for_search, prepare, recognize_prepared};
use std::path::PathBuf;
use std::time::Instant;

struct Case {
    name: String,
    image: Vec<u8>,
    expected: Vec<String>,
}

const PHRASES: &[&str] = &[
    "청구서 발행관리", "계약 등록", "세금계산서", "매출관리내역", "서비스기관",
    "NullPointerException", "HTTP 500", "SCR-019",
];

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + (ca != cb) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Character error rate of the best-matching recognized line.
fn cer(expected: &str, lines: &[String]) -> f64 {
    let e: Vec<char> = expected.chars().collect();
    lines
        .iter()
        .map(|l| levenshtein(&e, &l.chars().collect::<Vec<_>>()) as f64 / e.len() as f64)
        .fold(1.0, f64::min)
}

fn synthetic_cases(dir: &PathBuf) -> Vec<Case> {
    let mut cases = Vec::new();
    let mixed = "매출관리내역 SCR-019 HTTP 500 오류";
    let ui_row = ["매출관리내역", "고객정보", "계약 등록", "세금계산서"];
    for &(size, dark) in &[(11.0, false), (13.0, false), (13.0, true), (16.0, false), (24.0, true)] {
        let tag = format!("{}px-{}", size, if dark { "dark" } else { "light" });
        let path = dir.join(format!("phrases-{tag}.png"));
        render_text_image(&path, PHRASES, size, dark);
        cases.push(Case { name: format!("phrases {tag}"), image: std::fs::read(&path).unwrap(), expected: PHRASES.iter().map(|s| s.to_string()).collect() });

        let path = dir.join(format!("mixed-{tag}.png"));
        render_text_image(&path, &[mixed], size, dark);
        cases.push(Case { name: format!("mixed {tag}"), image: std::fs::read(&path).unwrap(), expected: vec![mixed.to_string()] });

        let row = ui_row.join("    ");
        let path = dir.join(format!("ui-row-{tag}.png"));
        render_text_image(&path, &[&row], size, dark);
        cases.push(Case { name: format!("ui row {tag}"), image: std::fs::read(&path).unwrap(), expected: ui_row.iter().map(|s| s.to_string()).collect() });
    }
    // Low resolution: render at 13px, then halve it (blurry, ~6.5px glyphs).
    let path = dir.join("lowres.png");
    render_text_image(&path, PHRASES, 13.0, false);
    let img = image::open(&path).unwrap();
    let small = img.resize(img.width() / 2, img.height() / 2, image::imageops::FilterType::Triangle);
    let mut bytes = Vec::new();
    small.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    cases.push(Case { name: "phrases low-res".into(), image: bytes, expected: PHRASES.iter().map(|s| s.to_string()).collect() });

    // Mostly English with a few Korean words: the case where automatic
    // language detection could pick English for the whole image.
    let english_heavy = [
        "java.lang.NullPointerException: Cannot invoke String.length()",
        "    at com.example.billing.InvoiceService.issue(InvoiceService.java:42)",
        "Caused by: HTTP 500 Internal Server Error (SCR-019) 매출관리내역",
        "    at com.example.billing.ContractController.register(ContractController.java:88)",
        "Retry the request or contact 서비스기관 support.",
    ];
    for &(size, dark) in &[(12.0, false), (12.0, true)] {
        let tag = format!("{}px-{}", size, if dark { "dark" } else { "light" });
        let path = dir.join(format!("english-heavy-{tag}.png"));
        render_text_image(&path, &english_heavy, size, dark);
        cases.push(Case {
            name: format!("english-heavy {tag}"),
            image: std::fs::read(&path).unwrap(),
            expected: ["NullPointerException", "HTTP 500", "SCR-019", "매출관리내역", "서비스기관", "InvoiceService.java:42"].iter().map(|s| s.to_string()).collect(),
        });
    }
    // Very small UI labels.
    let path = dir.join("tiny.png");
    render_text_image(&path, &["매출관리내역   청구서 발행관리   SCR-019"], 9.0, false);
    cases.push(Case { name: "tiny 9px".into(), image: std::fs::read(&path).unwrap(), expected: vec!["매출관리내역".into(), "청구서 발행관리".into(), "SCR-019".into()] });
    cases
}

fn real_cases() -> Vec<Case> {
    let Ok(spec) = std::env::var("CLIPET_OCR_BENCH") else { return Vec::new() };
    spec.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut parts = l.split('|');
            let path = PathBuf::from(parts.next()?);
            let image = std::fs::read(&path).ok()?;
            Some(Case {
                name: format!("real {}", path.file_name()?.to_string_lossy()),
                image,
                expected: parts.map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

#[test]
#[ignore]
fn ocr_benchmark() {
    let dir = std::env::temp_dir().join(format!("clipet-ocr-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cases = if std::env::var("CLIPET_OCR_BENCH_ONLY").as_deref() == Ok("real") { Vec::new() } else { synthetic_cases(&dir) };
    cases.extend(real_cases());

    let configs: Vec<(&str, OcrOptions)> = vec![
        ("v1 auto-detect + correction", OcrOptions { languages: vec![], auto_detect: true, language_correction: true, accurate: true }),
        ("ko+en + correction", OcrOptions { languages: vec!["ko-KR".into(), "en-US".into()], auto_detect: false, language_correction: true, accurate: true }),
        ("ko+en, no correction", OcrOptions { languages: vec!["ko-KR".into(), "en-US".into()], auto_detect: false, language_correction: false, accurate: true }),
        ("en only + correction", OcrOptions { languages: vec!["en-US".into()], auto_detect: false, language_correction: true, accurate: true }),
        ("fast (Latin only)", OcrOptions { languages: vec!["en-US".into()], auto_detect: false, language_correction: false, accurate: false }),
    ];
    let preprocessing = [Preprocess::None, Preprocess::Upscale2x, Preprocess::GrayContrast, Preprocess::Auto];

    println!("\n{:<32} {:<13} {:>8} {:>8} {:>8} {:>9}", "config", "preprocess", "exact%", "found%", "CER%", "ms/img");
    for (name, options) in &configs {
        for pre in preprocessing {
            if !options.accurate && pre != Preprocess::None {
                continue;
            }
            let (mut exact, mut found, mut total, mut cer_sum, mut ms) = (0, 0, 0, 0.0, 0u128);
            let mut misses: Vec<String> = Vec::new();
            for case in &cases {
                let started = Instant::now();
                let prepared = prepare(&case.image, pre).unwrap();
                let lines = recognize_prepared(&prepared, options).unwrap_or_default();
                ms += started.elapsed().as_millis();
                let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
                let all = texts.join("\n");
                let all_norm = normalize_for_search(&all);
                let all_compact: String = all_norm.chars().filter(|c| !c.is_whitespace()).collect();
                for e in &case.expected {
                    total += 1;
                    if all.contains(e.as_str()) { exact += 1; }
                    let compact: String = normalize_for_search(e).chars().filter(|c| !c.is_whitespace()).collect();
                    if all_compact.contains(&compact) { found += 1; } else if misses.len() < 12 { misses.push(format!("{}: {e}", case.name)); }
                    cer_sum += cer(e, &texts);
                }
            }
            println!(
                "{:<32} {:<13} {:>7.1}% {:>7.1}% {:>7.1}% {:>9}",
                name, format!("{pre:?}"),
                exact as f64 * 100.0 / total as f64,
                found as f64 * 100.0 / total as f64,
                cer_sum * 100.0 / total as f64,
                ms / cases.len() as u128
            );
            if std::env::var("CLIPET_OCR_BENCH_MISSES").is_ok() {
                for m in &misses { println!("      miss  {m}"); }
            }
        }
    }
    println!("\n{} images, {} expected phrases", cases.len(), cases.iter().map(|c| c.expected.len()).sum::<usize>());
    std::fs::remove_dir_all(&dir).ok();
}

/// Production pipeline only, on files from CLIPET_OCR_PROBE (one path per
/// line). Run under `/usr/bin/time -l` to see CPU time and peak memory.
#[test]
#[ignore]
fn ocr_resource_probe() {
    let paths = std::env::var("CLIPET_OCR_PROBE").unwrap_or_default();
    for path in paths.lines().filter(|l| !l.is_empty()) {
        let started = Instant::now();
        let lines = super::recognize(std::path::Path::new(path), super::DEFAULT_LANGUAGE).unwrap();
        println!("{} lines in {} ms", lines.len(), started.elapsed().as_millis());
    }
}

//! Apple Vision text recognition (built into macOS 10.15+, fully on-device).

use super::{OcrLine, OcrOptions};
use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

/// Recognizes text in encoded image bytes (PNG/JPEG/TIFF…). Working from
/// bytes lets callers OCR a temporary preprocessed copy without ever writing
/// over the original file.
pub fn recognize_data(image: &[u8], options: &OcrOptions) -> Result<Vec<OcrLine>, String> {
    autoreleasepool(|_| {
        let data = NSData::with_bytes(image);
        let handler = VNImageRequestHandler::initWithData_options(
            VNImageRequestHandler::alloc(),
            &data,
            &NSDictionary::new(),
        );
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(if options.accurate {
            VNRequestTextRecognitionLevel::Accurate
        } else {
            VNRequestTextRecognitionLevel::Fast
        });
        request.setUsesLanguageCorrection(options.language_correction);
        request.setAutomaticallyDetectsLanguage(options.auto_detect);
        if !options.languages.is_empty() {
            let languages: Vec<_> = options
                .languages
                .iter()
                .map(|l| NSString::from_str(l))
                .collect();
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&languages));
        }

        let as_request: &VNRequest = &request;
        handler
            .performRequests_error(&NSArray::from_slice(&[as_request]))
            .map_err(|e| format!("Vision could not read the image: {}", e.localizedDescription()))?;

        let mut lines = Vec::new();
        if let Some(results) = request.results() {
            for observation in results.iter() {
                let candidates = observation.topCandidates(1);
                let Some(best) = candidates.firstObject() else {
                    continue;
                };
                let text = best.string().to_string();
                if text.trim().is_empty() {
                    continue;
                }
                // Vision boxes are normalized with a bottom-left origin.
                let b = unsafe { observation.boundingBox() };
                lines.push(OcrLine {
                    text,
                    x: b.origin.x.clamp(0.0, 1.0),
                    y: (1.0 - b.origin.y - b.size.height).clamp(0.0, 1.0),
                    w: b.size.width.clamp(0.0, 1.0),
                    h: b.size.height.clamp(0.0, 1.0),
                });
            }
        }
        Ok(lines)
    })
}

/// Renders lines of text into a PNG with AppKit (via JavaScript for
/// Automation, so tests and the benchmark need no font-rendering dependency).
/// Hangul falls back to Apple SD Gothic Neo automatically.
#[cfg(test)]
pub fn render_text_image(path: &std::path::Path, lines: &[&str], font_size: f64, dark: bool) {
    let width = 60.0 + lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f64 * font_size;
    let line_h = font_size * 1.8;
    let height = 30.0 + lines.len() as f64 * line_h;
    let (bg, fg) = if dark {
        ("$.NSColor.colorWithSRGBRedGreenBlueAlpha(0.12, 0.13, 0.15, 1)", "$.NSColor.colorWithSRGBRedGreenBlueAlpha(0.88, 0.9, 0.93, 1)")
    } else {
        ("$.NSColor.whiteColor", "$.NSColor.colorWithSRGBRedGreenBlueAlpha(0.15, 0.15, 0.17, 1)")
    };
    let draws: String = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            format!(
                "$({line:?}).drawAtPointWithAttributes($.NSMakePoint(20, {y}), attrs);\n",
                y = height - 15.0 - (i as f64 + 1.0) * line_h
            )
        })
        .collect();
    let script = format!(
        r#"ObjC.import('AppKit');
var img = $.NSImage.alloc.initWithSize($.NSMakeSize({width}, {height}));
img.lockFocus;
{bg}.set;
$.NSRectFill($.NSMakeRect(0, 0, {width}, {height}));
var attrs = $.NSMutableDictionary.dictionary;
attrs.setObjectForKey($.NSFont.systemFontOfSize({font_size}), $.NSFontAttributeName);
attrs.setObjectForKey({fg}, $.NSForegroundColorAttributeName);
{draws}img.unlockFocus;
var rep = $.NSBitmapImageRep.imageRepWithData(img.TIFFRepresentation);
rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $()).writeToFileAtomically({path:?}, true);"#,
        path = path.to_str().unwrap()
    );
    let status = std::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .status()
        .expect("osascript");
    assert!(status.success() && path.exists(), "test image was not rendered");
}

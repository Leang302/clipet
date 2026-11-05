//! Windows.Media.Ocr text recognition (built into Windows 10+, on-device).
//! Uses the OCR languages installed for the user's profile languages.

use super::{OcrLanguage, OcrLine};
use windows::Globalization::Language;
use std::path::Path;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
    ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::{FileAccessMode, StorageFile};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::HSTRING;

pub fn recognize(path: &Path, language: OcrLanguage) -> Result<Vec<OcrLine>, String> {
    if !path.is_file() {
        return Err(format!("{} does not exist", path.display()));
    }
    // Idempotent per thread; "already initialized" is fine.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let engine = engine_for(language)?;
    run(path, &engine).map_err(|e| format!("Windows OCR failed: {}", e.message()))
}

fn run(path: &Path, engine: &OcrEngine) -> windows::core::Result<Vec<OcrLine>> {
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
    let stream = file.OpenAsync(FileAccessMode::Read)?.join()?;
    let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
    let (width, height) = (decoder.PixelWidth()?, decoder.PixelHeight()?);

    // The engine rejects images above MaxImageDimension; scale large
    // screenshots down (boxes are normalized, so they stay correct).
    let max = OcrEngine::MaxImageDimension()?.max(1);
    let scale = (f64::from(max) / f64::from(width.max(height).max(1))).min(1.0);
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(((f64::from(width) * scale) as u32).max(1))?;
    transform.SetScaledHeight(((f64::from(height) * scale) as u32).max(1))?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    let bitmap = decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )?
        .join()?;
    let (bw, bh) = (
        f64::from(bitmap.PixelWidth()?.max(1)),
        f64::from(bitmap.PixelHeight()?.max(1)),
    );

    let result = engine.RecognizeAsync(&bitmap)?.join()?;

    let mut lines = Vec::new();
    for line in result.Lines()? {
        let text = line.Text()?.to_string();
        if text.trim().is_empty() {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, 0.0f64, 0.0f64);
        for word in line.Words()? {
            let r = word.BoundingRect()?;
            x0 = x0.min(f64::from(r.X));
            y0 = y0.min(f64::from(r.Y));
            x1 = x1.max(f64::from(r.X + r.Width));
            y1 = y1.max(f64::from(r.Y + r.Height));
        }
        if x0 > x1 {
            (x0, y0, x1, y1) = (0.0, 0.0, 0.0, 0.0);
        }
        lines.push(OcrLine {
            text,
            x: (x0 / bw).clamp(0.0, 1.0),
            y: (y0 / bh).clamp(0.0, 1.0),
            w: ((x1 - x0) / bw).clamp(0.0, 1.0),
            h: ((y1 - y0) / bh).clamp(0.0, 1.0),
        });
    }
    Ok(lines)
}

/// Windows runs one OCR language per engine; each needs its OCR pack
/// installed (Settings → Time & language → Language & region).
fn engine_for(language: OcrLanguage) -> Result<OcrEngine, String> {
    let installed = |tag: &str| -> Option<OcrEngine> {
        let lang = Language::CreateLanguage(&HSTRING::from(tag)).ok()?;
        if OcrEngine::IsLanguageSupported(&lang).ok()? {
            OcrEngine::TryCreateFromLanguage(&lang).ok()
        } else {
            None
        }
    };
    let profile = || OcrEngine::TryCreateFromUserProfileLanguages().ok();
    let engine = match language {
        OcrLanguage::Auto => profile(),
        // The Korean engine also reads Latin letters and digits.
        OcrLanguage::KoreanEnglish => installed("ko").or_else(profile),
        OcrLanguage::English => installed("en-US").or_else(|| installed("en")),
        OcrLanguage::Korean => installed("ko"),
    };
    engine.ok_or_else(|| match language {
        OcrLanguage::Korean => "Korean text recognition isn't installed. Add Korean in Windows Settings → Time & language → Language & region, then Re-scan.".to_string(),
        OcrLanguage::English => "English text recognition isn't installed in Windows.".to_string(),
        _ => "No text recognition language is installed in Windows. Add one in Settings → Time & language → Language & region.".to_string(),
    })
}

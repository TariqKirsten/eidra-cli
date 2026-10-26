//! Images and PDFs for the model (docs/DESIGN.md §10). An image goes as it is if it fits in
//! 2048 × 2048, the largest the server takes, and as a smaller copy if not. A PDF goes page by
//! page: each page's text layer, for copying exact values, then the page as an image, for what the
//! text loses: headings, tables, figures, layout, and everything on a scanned page.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader};
use tokio::process::Command;

use crate::text::base64;

/// The longest side the server takes.
const LARGEST: u32 = 2048;
/// The long side a PDF page is rendered at: 10-point text stays legible.
const PAGE_PIXELS: u32 = 1600;
/// PDF pages read at once, unless the call asks for fewer, and the most it may ask for.
pub const PAGES: u64 = 5;
const MOST_PAGES: u64 = 10;
/// How long one of poppler's tools may take.
const POPPLER_TIMEOUT: Duration = Duration::from_secs(60);

/// An image ready for the model.
#[derive(Debug, Clone)]
pub struct Image {
    /// A `data:` URL.
    pub url: String,
    pub width: u32,
    pub height: u32,
    /// Its size before it was made smaller, if it was.
    pub original: Option<(u32, u32)>,
    pub format: &'static str,
}

impl Image {
    /// `a 480 × 320 PNG image`, and the size it is shown at if it was made smaller.
    pub fn describe(&self) -> String {
        match self.original {
            Some((w, h)) => format!(
                "a {w} × {h} {} image, shown at {} × {}",
                self.format, self.width, self.height
            ),
            None => format!("a {} × {} {} image", self.width, self.height, self.format),
        }
    }
}

/// Whether `path` names an image the model can be shown, by its extension.
pub fn is_image(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp"))
}

pub fn is_pdf(path: &Path) -> bool {
    extension(path).as_deref() == Some("pdf")
}

fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

/// `bytes` as an image the server takes: as it is, or smaller if it does not fit.
pub fn image(bytes: &[u8]) -> Result<Image, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("reading the image: {e}"))?;
    let format = reader.format().ok_or("not an image format eidra can read")?;
    let name = match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WebP",
        _ => return Err("not an image format eidra can read".into()),
    };
    let (width, height) =
        reader.into_dimensions().map_err(|e| format!("reading the image: {e}"))?;
    let fits = width <= LARGEST && height <= LARGEST;
    if fits && format != ImageFormat::Gif {
        let mime = format.to_mime_type();
        let url = format!("data:{mime};base64,{}", base64(bytes));
        return Ok(Image { url, width, height, original: None, format: name });
    }
    let decoded = image::load_from_memory(bytes).map_err(|e| format!("reading the image: {e}"))?;
    let resized =
        if fits { decoded } else { decoded.resize(LARGEST, LARGEST, FilterType::Lanczos3) };
    let mut out = Vec::new();
    // A photo stays a JPEG; anything else becomes a PNG, which keeps text and lines sharp.
    let mime = if format == ImageFormat::Jpeg {
        let encoder = JpegEncoder::new_with_quality(&mut out, 90);
        resized.to_rgb8().write_with_encoder(encoder).map_err(|e| e.to_string())?;
        "image/jpeg"
    } else {
        resized
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        "image/png"
    };
    let (shown_width, shown_height) = (resized.width(), resized.height());
    Ok(Image {
        url: format!("data:{mime};base64,{}", base64(&out)),
        width: shown_width,
        height: shown_height,
        original: (!fits).then_some((width, height)),
        format: name,
    })
}

/// Some of a PDF's pages: the text before the first page's image, then each image with the text
/// that follows it.
pub struct Pages {
    pub text: String,
    pub images: Vec<(Image, String)>,
}

/// Pages `first` to `first + count - 1` of a PDF (1-based; five unless asked), each as its text
/// layer then its image. `shown` is the path as the model gave it.
pub async fn pdf(
    path: &Path,
    shown: &str,
    first: Option<u64>,
    count: Option<u64>,
) -> Result<Pages, String> {
    let pages = page_count(path).await?;
    let first = first.unwrap_or(1).max(1);
    if first > pages {
        return Err(format!("{shown} has {pages} pages; offset {first} is past the end"));
    }
    let last = (first + count.unwrap_or(PAGES).clamp(1, MOST_PAGES) - 1).min(pages);
    let texts = page_texts(path, first, last).await;
    let images = page_images(path, first, last).await;

    let plural = if pages == 1 { "" } else { "s" };
    let mut head = format!("{shown}: a PDF of {pages} page{plural}; pages {first}–{last}");
    match (&texts, &images) {
        (Some(_), Ok(_)) => head.push_str(
            " follow, each as its text layer and then its image. Copy exact values from the \
             text layer; read headings, tables and figures from the image.",
        ),
        (Some(_), Err(why)) => head.push_str(&format!(" follow as text only: {why}.")),
        (None, Ok(_)) => head.push_str(" follow as images only: their text could not be read."),
        (None, Err(why)) => return Err(format!("reading {shown}: {why}")),
    }
    let page_text = |n: u64| -> String {
        let text = texts.as_ref().and_then(|t| t.get((n - first) as usize)).map(|t| t.trim_end());
        match text {
            Some(text) if !text.trim().is_empty() => format!("\n\n--- page {n} ---\n{text}"),
            Some(_) => format!(
                "\n\n--- page {n} ---\n(no text layer: a scan or a drawing, so read it from the \
                 image)"
            ),
            None => format!("\n\n--- page {n} ---"),
        }
    };
    let footer = if last < pages {
        format!("\n\n({} more pages; continue with offset {})", pages - last, last + 1)
    } else {
        String::new()
    };
    let mut result = Pages { text: head, images: Vec::new() };
    match images {
        Ok(images) => {
            result.text.push_str(&page_text(first));
            let count = images.len() as u64;
            for (i, image) in images.into_iter().enumerate() {
                let n = first + i as u64;
                let after = if n < last && i as u64 + 1 < count {
                    page_text(n + 1)
                } else {
                    footer.clone()
                };
                result.images.push((image, after));
            }
        }
        Err(_) => {
            for n in first..=last {
                result.text.push_str(&page_text(n));
            }
            result.text.push_str(&footer);
        }
    }
    Ok(result)
}

/// The PDF's number of pages, from poppler's `pdfinfo`.
async fn page_count(path: &Path) -> Result<u64, String> {
    let info = poppler(Command::new("pdfinfo").arg(path))
        .await
        .map_err(|why| format!("reading the PDF needs poppler's pdfinfo: {why}"))?;
    String::from_utf8_lossy(&info)
        .lines()
        .find_map(|line| line.strip_prefix("Pages:")?.trim().parse().ok())
        .ok_or_else(|| "pdfinfo did not say how many pages the PDF has".into())
}

/// Each page's text layer, laid out as on the page; None if it could not be read.
async fn page_texts(path: &Path, first: u64, last: u64) -> Option<Vec<String>> {
    let mut command = Command::new("pdftotext");
    command.args(["-layout", "-f", &first.to_string(), "-l", &last.to_string()]).arg(path);
    let text = poppler(command.arg("-")).await.ok()?;
    // Each page ends with a form feed.
    let text = String::from_utf8_lossy(&text);
    Some(text.split('\u{c}').take((last - first + 1) as usize).map(str::to_owned).collect())
}

/// Each page rendered as an image, by poppler's `pdftoppm`.
async fn page_images(path: &Path, first: u64, last: u64) -> Result<Vec<Image>, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eidra-pdf-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = render(path, first, last, &dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

async fn render(path: &Path, first: u64, last: u64, dir: &Path) -> Result<Vec<Image>, String> {
    let mut command = Command::new("pdftoppm");
    command.args(["-png", "-scale-to", &PAGE_PIXELS.to_string()]);
    command.args(["-f", &first.to_string(), "-l", &last.to_string()]);
    poppler(command.arg(path).arg(dir.join("page")))
        .await
        .map_err(|why| format!("page images need poppler's pdftoppm: {why}"))?;
    // page-1.png, or page-01.png and so on: the number is padded to the document's page count.
    let mut files: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let number = path.file_stem()?.to_str()?.rsplit('-').next()?.parse().ok()?;
            Some((number, path))
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|(_, file)| image(&std::fs::read(&file).map_err(|e| e.to_string())?))
        .collect()
}

/// Runs one of poppler's tools and returns what it wrote.
async fn poppler(command: &mut Command) -> Result<Vec<u8>, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "it is not installed here".to_owned(),
        _ => e.to_string(),
    })?;
    let done = tokio::time::timeout(POPPLER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "it took too long".to_owned())?
        .map_err(|e| e.to_string())?;
    if !done.status.success() {
        let why = String::from_utf8_lossy(&done.stderr);
        return Err(why.lines().next().unwrap_or("it failed").to_owned());
    }
    Ok(done.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]))
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn an_image_that_fits_goes_as_it_is() {
        let bytes = png(480, 320);
        let image = image(&bytes).unwrap();
        assert_eq!((image.width, image.height, image.original), (480, 320, None));
        assert_eq!(image.url, format!("data:image/png;base64,{}", base64(&bytes)));
        assert_eq!(image.describe(), "a 480 × 320 PNG image");
    }

    #[test]
    fn a_large_image_is_made_to_fit() {
        let image = image(&png(4096, 1024)).unwrap();
        assert_eq!((image.width, image.height), (2048, 512));
        assert_eq!(image.original, Some((4096, 1024)));
        assert_eq!(image.describe(), "a 4096 × 1024 PNG image, shown at 2048 × 512");
    }

    #[test]
    fn what_is_not_an_image_says_so() {
        assert!(image(b"%PDF-1.7 not an image").is_err());
        assert!(is_image(Path::new("a/Shot.PNG")) && is_pdf(Path::new("spec.pdf")));
        assert!(!is_image(Path::new("notes.txt")));
    }
}

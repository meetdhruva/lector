//! File kind detection (text / binary / image / directory).

use std::path::Path;

/// How many leading bytes are inspected for binary detection.
pub const SNIFF_LEN: usize = 8192;

/// Image formats Lector can preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// Portable Network Graphics.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF.
    Gif,
    /// WebP.
    Webp,
    /// Scalable Vector Graphics (text-based).
    Svg,
    /// Windows bitmap.
    Bmp,
    /// Windows icon.
    Ico,
}

impl ImageFormat {
    /// Guesses the format from a file extension (case-insensitive).
    pub fn from_extension(extension: &str) -> Option<ImageFormat> {
        Some(match extension.to_ascii_lowercase().as_str() {
            "png" => ImageFormat::Png,
            "jpg" | "jpeg" | "jpe" | "jfif" => ImageFormat::Jpeg,
            "gif" => ImageFormat::Gif,
            "webp" => ImageFormat::Webp,
            "svg" => ImageFormat::Svg,
            "bmp" | "dib" => ImageFormat::Bmp,
            "ico" | "cur" => ImageFormat::Ico,
            _ => return None,
        })
    }

    /// Detects the format from leading magic bytes (SVG is not detected here).
    pub fn from_magic(head: &[u8]) -> Option<ImageFormat> {
        if head.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(ImageFormat::Png)
        } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(ImageFormat::Jpeg)
        } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
            Some(ImageFormat::Gif)
        } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
            Some(ImageFormat::Webp)
        } else if head.starts_with(&[0x00, 0x00, 0x01, 0x00])
            || head.starts_with(&[0x00, 0x00, 0x02, 0x00])
        {
            Some(ImageFormat::Ico)
        } else if head.len() >= 14 && head.starts_with(b"BM") && is_binary(head) {
            Some(ImageFormat::Bmp)
        } else {
            None
        }
    }

    /// The conventional MIME type.
    pub fn mime_type(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Svg => "image/svg+xml",
            ImageFormat::Bmp => "image/bmp",
            ImageFormat::Ico => "image/x-icon",
        }
    }
}

/// What a path holds, for choosing a viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// Text (any supported encoding).
    Text,
    /// Non-text data.
    Binary,
    /// A previewable image.
    Image(ImageFormat),
    /// A directory.
    Directory,
}

/// Whether `bytes` starts with a UTF-16 byte order mark.
pub fn has_utf16_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])
}

/// Heuristic binary check on (up to) the first [`SNIFF_LEN`] bytes: UTF-16
/// with a BOM is text; otherwise any NUL byte, or more than 10% control
/// bytes (other than tab, newlines, form feed, backspace and escape),
/// means binary.
pub fn is_binary(head: &[u8]) -> bool {
    let head = &head[..head.len().min(SNIFF_LEN)];
    if has_utf16_bom(head) {
        return false;
    }
    if memchr_nul(head) {
        return true;
    }
    let control = head
        .iter()
        .filter(|&&b| {
            (b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x08 | 0x1B)) || b == 0x7F
        })
        .count();
    control * 10 > head.len()
}

fn memchr_nul(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn looks_like_svg(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&head[..head.len().min(1024)]);
    let trimmed = text.trim_start_matches('\u{FEFF}').trim_start();
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"))
}

/// Classifies `path` given its first bytes (`head`, ideally
/// [`SNIFF_LEN`] bytes). Directories are detected via the filesystem; images
/// by magic bytes or extension (an image extension on binary content is an
/// image; `.svg`, or text content starting with `<svg`, is an SVG image).
pub fn detect_kind(path: &Path, head: &[u8]) -> FileKind {
    if path.is_dir() {
        return FileKind::Directory;
    }
    if let Some(format) = ImageFormat::from_magic(head) {
        return FileKind::Image(format);
    }
    let by_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(ImageFormat::from_extension);
    let binary = is_binary(head);
    match by_extension {
        Some(ImageFormat::Svg) if !binary => FileKind::Image(ImageFormat::Svg),
        Some(format) if binary || head.is_empty() => FileKind::Image(format),
        _ if binary => FileKind::Binary,
        _ if looks_like_svg(head) && by_extension.is_none() && path.extension().is_none() => {
            FileKind::Image(ImageFormat::Svg)
        }
        _ => FileKind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_images_by_magic_and_extension() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        assert_eq!(
            detect_kind(Path::new("x.bin"), png),
            FileKind::Image(ImageFormat::Png)
        );
        assert_eq!(
            detect_kind(Path::new("a.JPG"), &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            FileKind::Image(ImageFormat::Jpeg)
        );
        assert_eq!(
            detect_kind(Path::new("a"), b"GIF89a\x01\0"),
            FileKind::Image(ImageFormat::Gif)
        );
        assert_eq!(
            detect_kind(Path::new("a"), b"RIFF\x10\0\0\0WEBPVP8 "),
            FileKind::Image(ImageFormat::Webp)
        );
        assert_eq!(
            detect_kind(Path::new("favicon.ico"), &[0, 0, 1, 0, 1, 0, 16, 16]),
            FileKind::Image(ImageFormat::Ico)
        );
        let bmp = [
            b'B', b'M', 0x36, 0, 0, 0, 0, 0, 0, 0, 0x36, 0, 0, 0, 0x28, 0,
        ];
        assert_eq!(
            detect_kind(Path::new("a.bmp"), &bmp),
            FileKind::Image(ImageFormat::Bmp)
        );
        let svg = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        assert_eq!(
            detect_kind(Path::new("icon.svg"), svg),
            FileKind::Image(ImageFormat::Svg)
        );
        // A text file that merely starts with "BM" is not a bitmap.
        assert_eq!(
            detect_kind(Path::new("notes.txt"), b"BM is a fine prefix for text\n"),
            FileKind::Text
        );
        // An empty .png is still shown as an image (the viewer reports the error).
        assert_eq!(
            detect_kind(Path::new("empty.png"), b""),
            FileKind::Image(ImageFormat::Png)
        );
    }

    #[test]
    fn detects_binary_and_text() {
        assert_eq!(
            detect_kind(Path::new("a.rs"), b"fn main() {}\n"),
            FileKind::Text
        );
        assert_eq!(
            detect_kind(Path::new("a.o"), b"\x7fELF\x02\x01\x01\0\0\0"),
            FileKind::Binary
        );
        assert_eq!(
            detect_kind(Path::new("a"), &[1, 2, 3, 4, 5, 6, 7, b'a']),
            FileKind::Binary
        );
        assert_eq!(
            detect_kind(Path::new("log"), b"\x1b[31mred\x1b[0m\n\tok\r\n"),
            FileKind::Text
        );
        // UTF-16 text contains NULs but has a BOM.
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("hi\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(detect_kind(Path::new("a.txt"), &utf16), FileKind::Text);
        assert_eq!(
            detect_kind(Path::new("latin1.txt"), b"caf\xe9 cr\xe8me\n"),
            FileKind::Text
        );
        assert_eq!(detect_kind(Path::new("empty"), b""), FileKind::Text);
        // NUL beyond the sniff window is not considered.
        let mut late_nul = vec![b'a'; SNIFF_LEN];
        late_nul.push(0);
        assert!(!is_binary(&late_nul));
    }

    #[test]
    fn detects_directories() {
        let dir = std::env::temp_dir();
        assert_eq!(detect_kind(&dir, b""), FileKind::Directory);
    }
}

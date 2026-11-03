//! Loading files into [`Text`].

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use lector_text::Text;

use crate::decode::{decode, UTF_8};
use crate::kind::{detect_kind, has_utf16_bom, FileKind, ImageFormat, SNIFF_LEN};

/// Encoding name reported for files without text.
pub const BINARY: &str = "binary";

/// A loaded file.
#[derive(Clone, Debug)]
pub struct LoadedFile {
    /// What the file holds.
    pub kind: FileKind,
    /// The text, for [`FileKind::Text`] and SVG images (whose source can be
    /// shown); `None` for other images, binary files and directories.
    pub text: Option<Text>,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Encoding name (`"UTF-8"`, `"UTF-16LE"`, `"UTF-16BE"`,
    /// `"windows-1252"`), [`BINARY`] for non-text, `""` for directories.
    pub encoding: &'static str,
    /// Whether decoding replaced invalid sequences. For memory-mapped files
    /// this only reflects the first [`SNIFF_LEN`] bytes.
    pub lossy: bool,
}

fn read_head(path: &Path) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(SNIFF_LEN);
    File::open(path)?
        .take(SNIFF_LEN as u64)
        .read_to_end(&mut head)?;
    Ok(head)
}

/// Whether `head` (a prefix of a file) is valid UTF-8, ignoring a char cut
/// off at the end of the prefix.
fn head_is_utf8(head: &[u8]) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

/// Loads `path`, sniffing its kind and encoding.
///
/// Text files of at least `mmap_threshold` bytes are memory-mapped
/// ([`Text::Mapped`], read as UTF-8 with lossy decoding) unless they start
/// with a UTF-16 BOM, which forces a full decode. Smaller text files are
/// read, decoded with [`decode`] and stored as a rope.
pub fn load_file(path: &Path, mmap_threshold: u64) -> io::Result<LoadedFile> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        return Ok(LoadedFile {
            kind: FileKind::Directory,
            text: None,
            size: 0,
            encoding: "",
            lossy: false,
        });
    }
    if !metadata.is_file() {
        // FIFOs, sockets and devices: reading could block forever or never end.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let size = metadata.len();

    if size >= mmap_threshold {
        let head = read_head(path)?;
        let kind = detect_kind(path, &head);
        let is_textual = matches!(kind, FileKind::Text | FileKind::Image(ImageFormat::Svg));
        if is_textual && !has_utf16_bom(&head) {
            let text = Text::open_mapped(path)?;
            return Ok(LoadedFile {
                kind,
                size: text.len() as u64,
                text: Some(text),
                encoding: UTF_8,
                lossy: !head_is_utf8(&head),
            });
        }
        if !is_textual {
            return Ok(binary(kind, size));
        }
    }

    let bytes = std::fs::read(path)?;
    let kind = detect_kind(path, &bytes[..bytes.len().min(SNIFF_LEN)]);
    match kind {
        FileKind::Text | FileKind::Image(ImageFormat::Svg) => {
            let (text, encoding, lossy) = decode(&bytes);
            Ok(LoadedFile {
                kind,
                text: Some(Text::from_str(&text)),
                size: bytes.len() as u64,
                encoding,
                lossy,
            })
        }
        _ => Ok(binary(kind, bytes.len() as u64)),
    }
}

fn binary(kind: FileKind, size: u64) -> LoadedFile {
    LoadedFile {
        kind,
        text: None,
        size,
        encoding: BINARY,
        lossy: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn special_files_are_rejected_without_blocking() {
        let err = load_file(Path::new("/dev/zero"), u64::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            let err = load_file(&fifo, 1024).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    #[test]
    fn loads_small_text_as_rope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "a.rs", b"fn main() {}\r\n// done");
        let file = load_file(&path, 1 << 20).expect("load");
        assert_eq!(file.kind, FileKind::Text);
        assert_eq!((file.encoding, file.lossy, file.size), ("UTF-8", false, 21));
        let text = file.text.expect("text");
        assert!(!text.is_mapped());
        assert_eq!(text.line(0), "fn main() {}");
        assert_eq!(text.line_count(), 2);
    }

    #[test]
    fn loads_large_text_mapped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let content = "line\n".repeat(1000);
        let path = write(dir.path(), "big.log", content.as_bytes());
        let file = load_file(&path, 100).expect("load");
        let text = file.text.expect("text");
        assert!(text.is_mapped());
        assert_eq!(text.line_count(), 1001);
        assert_eq!(file.size, content.len() as u64);
    }

    #[test]
    fn utf16_is_decoded_even_above_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("héllo\nwörld".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let path = write(dir.path(), "u16.txt", &bytes);
        let file = load_file(&path, 4).expect("load");
        assert_eq!(file.encoding, "UTF-16LE");
        let text = file.text.expect("text");
        assert!(!text.is_mapped());
        assert_eq!(text.line(1), "wörld");
    }

    #[test]
    fn latin1_and_binary_and_images() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "latin1.txt", b"caf\xe9\n");
        let file = load_file(&path, 1 << 20).expect("load");
        assert_eq!(file.encoding, "windows-1252");
        assert_eq!(file.text.expect("text").line(0), "café");

        let path = write(dir.path(), "blob.bin", b"\0\x01\x02binary");
        let file = load_file(&path, 1 << 20).expect("load");
        assert_eq!((file.kind, file.encoding), (FileKind::Binary, BINARY));
        assert!(file.text.is_none());
        let file = load_file(&path, 1).expect("load mapped-size binary");
        assert_eq!(file.kind, FileKind::Binary);
        assert!(file.text.is_none());

        let path = write(dir.path(), "pic.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        let file = load_file(&path, 1 << 20).expect("load");
        assert_eq!(file.kind, FileKind::Image(ImageFormat::Png));
        assert!(file.text.is_none());

        let path = write(
            dir.path(),
            "icon.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        );
        let file = load_file(&path, 1 << 20).expect("load");
        assert_eq!(file.kind, FileKind::Image(ImageFormat::Svg));
        assert!(file.text.is_some());

        let file = load_file(dir.path(), 1 << 20).expect("load dir");
        assert_eq!(file.kind, FileKind::Directory);
        assert!(load_file(&dir.path().join("missing"), 1).is_err());
    }

    #[test]
    fn empty_file_is_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "empty", b"");
        for threshold in [0, 1 << 20] {
            let file = load_file(&path, threshold).expect("load");
            assert_eq!(file.kind, FileKind::Text);
            assert_eq!(file.text.expect("text").line_count(), 1);
        }
    }
}

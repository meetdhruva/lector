//! Byte → text decoding with encoding detection.

use encoding_rs::{UTF_16BE, UTF_16LE, WINDOWS_1252};

/// Name reported for UTF-8 content.
pub const UTF_8: &str = "UTF-8";

/// Decodes file bytes to a `String`, returning `(text, encoding name,
/// lossy)`.
///
/// * UTF-8 (a BOM is stripped) → `"UTF-8"`.
/// * UTF-16 LE/BE **with BOM** → `"UTF-16LE"` / `"UTF-16BE"` (lossy if it
///   contains unpaired surrogates or an odd trailing byte).
/// * Otherwise, if the bytes are *mostly* invalid UTF-8 (more invalid bytes
///   than valid non-ASCII chars, typical of Latin-1 text) →
///   `"windows-1252"` (never lossy: every byte maps to a char).
/// * Otherwise UTF-8 with replacement characters, `lossy = true`.
///
/// Encoding names are the WHATWG/`encoding_rs` names.
pub fn decode(bytes: &[u8]) -> (String, &'static str, bool) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return decode_utf8(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, lossy) = UTF_16LE.decode_without_bom_handling(rest);
        return (text.into_owned(), UTF_16LE.name(), lossy);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, lossy) = UTF_16BE.decode_without_bom_handling(rest);
        return (text.into_owned(), UTF_16BE.name(), lossy);
    }
    decode_utf8(bytes)
}

fn decode_utf8(bytes: &[u8]) -> (String, &'static str, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), UTF_8, false),
        Err(_) => {
            let mut invalid_bytes = 0usize;
            let mut valid_non_ascii = 0usize;
            for chunk in bytes.utf8_chunks() {
                invalid_bytes += chunk.invalid().len();
                valid_non_ascii += chunk.valid().chars().filter(|c| !c.is_ascii()).count();
            }
            if invalid_bytes > valid_non_ascii {
                let (text, _) = WINDOWS_1252.decode_without_bom_handling(bytes);
                (text.into_owned(), WINDOWS_1252.name(), false)
            } else {
                (String::from_utf8_lossy(bytes).into_owned(), UTF_8, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_with_and_without_bom() {
        assert_eq!(
            decode(b"hello \xc3\xa9"),
            ("hello é".to_string(), "UTF-8", false)
        );
        assert_eq!(
            decode(b"\xef\xbb\xbfbom"),
            ("bom".to_string(), "UTF-8", false)
        );
        assert_eq!(decode(b""), (String::new(), "UTF-8", false));
    }

    #[test]
    fn utf16_with_bom() {
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("héllo 😀".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode(&le), ("héllo 😀".to_string(), "UTF-16LE", false));
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("abc".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode(&be), ("abc".to_string(), "UTF-16BE", false));
        let (_, _, lossy) = decode(&[0xFF, 0xFE, b'a']);
        assert!(lossy);
    }

    #[test]
    fn latin1_falls_back_to_windows_1252() {
        assert_eq!(
            decode(b"caf\xe9 cr\xe8me \x80"),
            ("café crème €".to_string(), "windows-1252", false)
        );
    }

    #[test]
    fn mostly_valid_utf8_is_lossy() {
        let (text, encoding, lossy) = decode(b"\xc3\xa9\xc3\xa9\xc3\xa9 ok \xff");
        assert_eq!(
            (text.as_str(), encoding, lossy),
            ("ééé ok \u{FFFD}", "UTF-8", true)
        );
    }
}

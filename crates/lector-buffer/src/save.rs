//! Encoding a buffer's text back to bytes for saving.

use lector_text::Rope;

/// Character encoding of the file on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8 without a byte order mark.
    Utf8,
    /// UTF-8 with a BOM.
    Utf8Bom,
    /// UTF-16 little endian, with BOM.
    Utf16Le,
    /// UTF-16 big endian, with BOM.
    Utf16Be,
}

/// Line terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n`
    Lf,
    /// `\r\n`
    CrLf,
}

impl LineEnding {
    /// The terminator as a string.
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
}

/// How [`encode`] transforms the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveOptions {
    /// Output encoding.
    pub encoding: Encoding,
    /// Convert every line break to this ending; `None` keeps the text as is.
    pub line_ending: Option<LineEnding>,
    /// Strip spaces and tabs at the end of every line.
    pub trim_trailing_whitespace: bool,
    /// Append a final line break when the text lacks one (never to empty text).
    pub ensure_final_newline: bool,
}

impl Default for SaveOptions {
    fn default() -> Self {
        SaveOptions {
            encoding: Encoding::Utf8,
            line_ending: None,
            trim_trailing_whitespace: false,
            ensure_final_newline: false,
        }
    }
}

/// Encodes `text` for writing to disk according to `opts`.
///
/// Line breaks are `\n` and `\r\n`; a lone `\r` is ordinary content. When a
/// final newline is added without an explicit `line_ending`, the first line
/// break of the text decides its style (default `\n`).
pub fn encode(text: &Rope, opts: &SaveOptions) -> Vec<u8> {
    let transform =
        opts.line_ending.is_some() || opts.trim_trailing_whitespace || opts.ensure_final_newline;
    let utf8: Vec<u8> = if transform {
        transform_text(text, opts).into_bytes()
    } else {
        let mut v = Vec::with_capacity(text.len());
        for chunk in text.chunks() {
            v.extend_from_slice(chunk.as_bytes());
        }
        v
    };
    match opts.encoding {
        Encoding::Utf8 => utf8,
        Encoding::Utf8Bom => {
            let mut v = Vec::with_capacity(utf8.len() + 3);
            v.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            v.extend_from_slice(&utf8);
            v
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let le = opts.encoding == Encoding::Utf16Le;
            let s = String::from_utf8_lossy(&utf8);
            let mut v = Vec::with_capacity(2 + s.len() * 2);
            let push = |v: &mut Vec<u8>, u: u16| {
                v.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
            };
            push(&mut v, 0xFEFF);
            for u in s.encode_utf16() {
                push(&mut v, u);
            }
            v
        }
    }
}

fn transform_text(text: &Rope, opts: &SaveOptions) -> String {
    let src = text.to_string();
    let detected = if src
        .find('\n')
        .is_some_and(|i| i > 0 && src.as_bytes()[i - 1] == b'\r')
    {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let eol = opts.line_ending.unwrap_or(detected);
    let mut out = String::with_capacity(src.len() + 1);
    let mut ends_with_break = false;
    let mut rest = src.as_str();
    while !rest.is_empty() {
        let (line, term, tail) = match rest.find('\n') {
            Some(i) => {
                let crlf = i > 0 && rest.as_bytes()[i - 1] == b'\r';
                let line_end = if crlf { i - 1 } else { i };
                (
                    &rest[..line_end],
                    Some(if crlf {
                        LineEnding::CrLf
                    } else {
                        LineEnding::Lf
                    }),
                    &rest[i + 1..],
                )
            }
            None => (rest, None, ""),
        };
        out.push_str(if opts.trim_trailing_whitespace {
            line.trim_end_matches([' ', '\t'])
        } else {
            line
        });
        match term {
            Some(t) => {
                out.push_str(opts.line_ending.unwrap_or(t).as_str());
                ends_with_break = true;
            }
            None => ends_with_break = false,
        }
        rest = tail;
    }
    if opts.ensure_final_newline && !out.is_empty() && !ends_with_break {
        out.push_str(eol.as_str());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(s: &str, o: SaveOptions) -> Vec<u8> {
        encode(&Rope::from_str(s), &o)
    }

    #[test]
    fn plain_utf8_is_identity() {
        assert_eq!(enc("a\r\nb\n", SaveOptions::default()), b"a\r\nb\n");
        assert_eq!(enc("", SaveOptions::default()), b"");
    }

    #[test]
    fn boms_and_utf16() {
        let o = |e| SaveOptions {
            encoding: e,
            ..Default::default()
        };
        assert_eq!(
            enc("hi", o(Encoding::Utf8Bom)),
            [0xEF, 0xBB, 0xBF, b'h', b'i']
        );
        assert_eq!(
            enc("h😀", o(Encoding::Utf16Le)),
            [0xFF, 0xFE, b'h', 0, 0x3D, 0xD8, 0x00, 0xDE]
        );
        assert_eq!(enc("h", o(Encoding::Utf16Be)), [0xFE, 0xFF, 0, b'h']);
    }

    #[test]
    fn line_endings() {
        let crlf = SaveOptions {
            line_ending: Some(LineEnding::CrLf),
            ..Default::default()
        };
        assert_eq!(enc("a\nb\r\nc", crlf), b"a\r\nb\r\nc");
        let lf = SaveOptions {
            line_ending: Some(LineEnding::Lf),
            ..Default::default()
        };
        assert_eq!(enc("a\r\nb\rc\n", lf), b"a\nb\rc\n");
    }

    #[test]
    fn trim_and_final_newline() {
        let o = SaveOptions {
            trim_trailing_whitespace: true,
            ensure_final_newline: true,
            ..Default::default()
        };
        assert_eq!(enc("a  \n\t\nb \t", o), b"a\n\nb\n");
        assert_eq!(enc("a \r\nb", o), b"a\r\nb\r\n");
        assert_eq!(enc("", o), b"");
        assert_eq!(enc("x\n", o), b"x\n");
        let both = SaveOptions {
            line_ending: Some(LineEnding::CrLf),
            ensure_final_newline: true,
            ..Default::default()
        };
        assert_eq!(enc("a\nb", both), b"a\r\nb\r\n");
    }
}

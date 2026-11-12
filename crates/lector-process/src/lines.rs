use std::borrow::Cow;

/// Which output stream a [`Line`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// How a [`Line`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineEnd {
    /// `\n` or `\r\n`: a finished line.
    Newline,
    /// A lone `\r`: a progress update that the next line overwrites (git's `--progress`).
    CarriageReturn,
    /// No terminator: end of stream, or a line longer than the limit was split.
    None,
}

/// One line of output, delivered while the process runs.
#[derive(Clone, PartialEq, Eq)]
pub struct Line {
    /// Its stream.
    pub stream: Stream,
    /// The bytes without the terminator.
    pub bytes: Vec<u8>,
    /// The terminator.
    pub end: LineEnd,
}

impl Line {
    /// The text (lossy UTF-8).
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    /// True for `\r`-terminated progress updates.
    pub fn is_progress_update(&self) -> bool {
        self.end == LineEnd::CarriageReturn
    }
}

impl std::fmt::Debug for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}({:?}, {:?})", self.stream, self.text(), self.end)
    }
}

/// Longest line delivered in one piece; longer lines are split ([`LineEnd::None`]).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Splits a byte stream into lines on `\n`, `\r\n` and lone `\r`, with bounded memory.
///
/// A `\r` is reported at once as [`LineEnd::CarriageReturn`] (progress must not wait for
/// the next chunk); if the next byte turns out to be `\n`, that `\n` is swallowed, so a
/// `\r\n` split across two reads is still one line (reported as a carriage return).
#[derive(Debug)]
pub struct LineSplitter {
    buf: Vec<u8>,
    max: usize,
    swallow_lf: bool,
}

impl Default for LineSplitter {
    fn default() -> Self {
        Self::new(MAX_LINE_BYTES)
    }
}

impl LineSplitter {
    /// A splitter that splits lines longer than `max_line` bytes (at least 1).
    pub fn new(max_line: usize) -> Self {
        LineSplitter {
            buf: Vec::new(),
            max: max_line.max(1),
            swallow_lf: false,
        }
    }

    /// Feeds a chunk; calls `emit` for every completed line.
    pub fn feed(&mut self, mut chunk: &[u8], emit: &mut dyn FnMut(Vec<u8>, LineEnd)) {
        if self.swallow_lf {
            self.swallow_lf = false;
            if let Some(rest) = chunk.strip_prefix(b"\n") {
                chunk = rest;
            }
        }
        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            match b {
                b'\n' => {
                    emit(std::mem::take(&mut self.buf), LineEnd::Newline);
                    i += 1;
                }
                b'\r' => match chunk.get(i + 1) {
                    Some(b'\n') => {
                        emit(std::mem::take(&mut self.buf), LineEnd::Newline);
                        i += 2;
                    }
                    Some(_) => {
                        emit(std::mem::take(&mut self.buf), LineEnd::CarriageReturn);
                        i += 1;
                    }
                    None => {
                        emit(std::mem::take(&mut self.buf), LineEnd::CarriageReturn);
                        self.swallow_lf = true;
                        i += 1;
                    }
                },
                _ => {
                    // Copy the run up to the next terminator in one go.
                    let end = chunk[i..]
                        .iter()
                        .position(|c| *c == b'\n' || *c == b'\r')
                        .map_or(chunk.len(), |p| i + p);
                    let mut run = &chunk[i..end];
                    while !run.is_empty() {
                        let room = self.max - self.buf.len();
                        let take = room.min(run.len());
                        self.buf.extend_from_slice(&run[..take]);
                        run = &run[take..];
                        if self.buf.len() >= self.max {
                            emit(std::mem::take(&mut self.buf), LineEnd::None);
                        }
                    }
                    i = end;
                }
            }
        }
    }

    /// End of stream: emits a final unterminated line, if any.
    pub fn finish(&mut self, emit: &mut dyn FnMut(Vec<u8>, LineEnd)) {
        if !self.buf.is_empty() {
            emit(std::mem::take(&mut self.buf), LineEnd::None);
        }
        self.swallow_lf = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]], max: usize) -> Vec<(String, LineEnd)> {
        let mut s = LineSplitter::new(max);
        let mut out = Vec::new();
        let mut emit = |b: Vec<u8>, e: LineEnd| out.push((String::from_utf8(b).unwrap(), e));
        for c in chunks {
            s.feed(c, &mut emit);
        }
        s.finish(&mut emit);
        out
    }

    #[test]
    fn splits_newlines_crlf_and_progress() {
        let out = split(
            &[b"a\nb\r\nReceiving  10%\rReceiving 100%, done.\ntail"],
            100,
        );
        assert_eq!(
            out,
            vec![
                ("a".into(), LineEnd::Newline),
                ("b".into(), LineEnd::Newline),
                ("Receiving  10%".into(), LineEnd::CarriageReturn),
                ("Receiving 100%, done.".into(), LineEnd::Newline),
                ("tail".into(), LineEnd::None),
            ]
        );
    }

    #[test]
    fn carriage_return_at_chunk_end_is_emitted_at_once() {
        let mut s = LineSplitter::new(100);
        let mut out = Vec::new();
        s.feed(b"50%\r", &mut |b, e| out.push((b, e)));
        assert_eq!(out, vec![(b"50%".to_vec(), LineEnd::CarriageReturn)]);
        // The `\n` of a split `\r\n` does not produce an empty line.
        s.feed(b"\nnext\n", &mut |b, e| out.push((b, e)));
        assert_eq!(out[1], (b"next".to_vec(), LineEnd::Newline));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn long_lines_are_split_with_bounded_memory() {
        let long = [b'x'; 25];
        let out = split(&[&long[..10], &long[10..], b"\n"], 8);
        let lens: Vec<_> = out.iter().map(|(t, e)| (t.len(), *e)).collect();
        assert_eq!(
            lens,
            vec![
                (8, LineEnd::None),
                (8, LineEnd::None),
                (8, LineEnd::None),
                (1, LineEnd::Newline)
            ]
        );
    }

    #[test]
    fn chunking_does_not_change_lines_without_cr() {
        let text = b"one\ntwo\n\nthree\nfour";
        let whole = split(&[text], 100);
        for cut in 0..text.len() {
            assert_eq!(
                split(&[&text[..cut], &text[cut..]], 100),
                whole,
                "cut {cut}"
            );
        }
    }
}

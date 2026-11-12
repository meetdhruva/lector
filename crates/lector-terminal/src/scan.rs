//! A tiny escape-sequence scanner that runs alongside the VT parser.
//!
//! `alacritty_terminal` ignores OSC 7 (cwd) and OSC 133 (command marks) and does
//! not track a few modes Lector needs (modifyOtherKeys, X10 mouse). The scanner
//! sees the same bytes as the parser, keeps its state across chunk boundaries and
//! reports each interesting sequence together with the offset of its *final*
//! byte, so the emulator can feed the parser up to that byte and observe the
//! terminal state exactly where the sequence takes effect.
//!
//! Memory is bounded: OSC payloads longer than [`MAX_OSC`] are dropped, CSI
//! parameter strings longer than [`MAX_CSI`] are ignored.

/// Longest OSC payload that is captured.
pub(crate) const MAX_OSC: usize = 4096;
/// Longest CSI parameter string that is captured.
const MAX_CSI: usize = 64;

/// A sequence the emulator cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanEvent {
    /// OSC 7 / 133 / 633 payload (everything between `ESC ]` and the terminator).
    Osc(Vec<u8>),
    /// `CSI 2 J` (erase whole display; pushes the screen into scrollback).
    EraseDisplay,
    /// `CSI 3 J` (erase scrollback).
    EraseScrollback,
    /// A private mode switch that swaps or clears screens (`?47`, `?1047`, `?1049`, `?3`).
    ScreenSwitch,
    /// `ESC c` (full reset).
    FullReset,
    /// `CSI > 4 ; n m` (xterm modifyOtherKeys level).
    ModifyOtherKeys(u8),
    /// `CSI ? 9 h/l` (X10 mouse reporting on/off).
    X10Mouse(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    /// DCS, SOS, PM, APC string: skipped until ESC.
    Str,
}

/// Incremental scanner; feed it every byte the parser receives.
#[derive(Debug)]
pub(crate) struct Scanner {
    state: State,
    osc: Vec<u8>,
    osc_overflow: bool,
    csi: Vec<u8>,
    csi_overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

/// OSC prefixes the scanner captures.
const WANTED_OSC: [&[u8]; 3] = [b"7;", b"133;", b"633;"];

fn wanted_osc(payload: &[u8]) -> bool {
    WANTED_OSC.iter().any(|w| payload.starts_with(w))
}

impl Scanner {
    /// A scanner in the ground state.
    pub(crate) fn new() -> Self {
        Self {
            state: State::Ground,
            osc: Vec::new(),
            osc_overflow: false,
            csi: Vec::new(),
            csi_overflow: false,
        }
    }

    /// Scans `bytes`, appending `(offset_of_final_byte, event)` pairs to `out`.
    pub(crate) fn scan(&mut self, bytes: &[u8], out: &mut Vec<(usize, ScanEvent)>) {
        let mut i = 0;
        while i < bytes.len() {
            // Fast path: in ground state skip to the next ESC.
            if self.state == State::Ground {
                match memchr_esc(&bytes[i..]) {
                    Some(n) => {
                        i += n + 1;
                        self.state = State::Escape;
                        continue;
                    }
                    None => return,
                }
            }
            let b = bytes[i];
            match self.state {
                State::Ground => {}
                State::Escape => self.escape(b, i, out),
                State::Csi => self.csi_byte(b, i, out),
                State::Osc => match b {
                    0x07 => self.finish_osc(i, out),
                    0x1b => self.state = State::OscEscape,
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => self.push_osc(b),
                },
                State::OscEscape => {
                    // vte dispatches the OSC on the ESC itself; report it at the ESC.
                    self.finish_osc(i.saturating_sub(1), out);
                    // The byte after ESC starts an escape sequence (`\` for ST).
                    self.escape(b, i, out);
                }
                State::Str => match b {
                    0x1b => self.state = State::Escape,
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {}
                },
            }
            i += 1;
        }
    }

    fn escape(&mut self, b: u8, i: usize, out: &mut Vec<(usize, ScanEvent)>) {
        self.state = match b {
            b'[' => {
                self.csi.clear();
                self.csi_overflow = false;
                State::Csi
            }
            b']' => {
                self.osc.clear();
                self.osc_overflow = false;
                State::Osc
            }
            b'P' | b'X' | b'^' | b'_' => State::Str,
            b'c' => {
                out.push((i, ScanEvent::FullReset));
                State::Ground
            }
            0x1b => State::Escape,
            // Intermediates of other escape sequences (`ESC ( B`, ...).
            0x20..=0x2f => State::Escape,
            _ => State::Ground,
        };
    }

    fn csi_byte(&mut self, b: u8, i: usize, out: &mut Vec<(usize, ScanEvent)>) {
        match b {
            0x1b => self.state = State::Escape,
            0x18 | 0x1a => self.state = State::Ground,
            0x20..=0x3f => {
                if self.csi.len() < MAX_CSI {
                    self.csi.push(b);
                } else {
                    self.csi_overflow = true;
                }
            }
            0x40..=0x7e => {
                if !self.csi_overflow {
                    if let Some(ev) = classify_csi(&self.csi, b) {
                        out.push((i, ev));
                    }
                }
                self.state = State::Ground;
            }
            // C0 controls inside CSI are executed by the parser and do not abort it.
            _ => {}
        }
    }

    fn push_osc(&mut self, b: u8) {
        if self.osc_overflow {
            return;
        }
        if self.osc.len() >= MAX_OSC {
            self.osc_overflow = true;
            self.osc = Vec::new();
            return;
        }
        self.osc.push(b);
        // Stop buffering early for OSCs we do not care about (OSC 52 can be large).
        let osc = &self.osc;
        let compatible = WANTED_OSC.iter().any(|w| {
            if osc.len() <= w.len() {
                w.starts_with(osc)
            } else {
                osc.starts_with(w)
            }
        });
        if !compatible {
            self.osc_overflow = true;
            self.osc = Vec::new();
        }
    }

    fn finish_osc(&mut self, i: usize, out: &mut Vec<(usize, ScanEvent)>) {
        if !self.osc_overflow && wanted_osc(&self.osc) {
            out.push((i, ScanEvent::Osc(std::mem::take(&mut self.osc))));
        }
        self.osc.clear();
        self.osc_overflow = false;
        self.state = State::Ground;
    }
}

fn memchr_esc(bytes: &[u8]) -> Option<usize> {
    memchr::memchr(0x1b, bytes)
}

fn parse_params(params: &[u8]) -> impl Iterator<Item = Option<u32>> + '_ {
    params.split(|&b| b == b';').map(|p| {
        if p.is_empty() {
            return None;
        }
        std::str::from_utf8(p).ok()?.parse::<u32>().ok()
    })
}

fn classify_csi(params: &[u8], final_byte: u8) -> Option<ScanEvent> {
    // Intermediates (0x20..=0x2f) make it a different sequence.
    if params.iter().any(|b| (0x20..=0x2f).contains(b)) {
        return None;
    }
    match (params.first(), final_byte) {
        (Some(b'?'), b'h' | b'l') => {
            let set = final_byte == b'h';
            let mut event = None;
            for p in parse_params(&params[1..]).flatten() {
                match p {
                    3 | 47 | 1047 | 1049 => event = Some(ScanEvent::ScreenSwitch),
                    9 if event.is_none() => event = Some(ScanEvent::X10Mouse(set)),
                    _ => {}
                }
            }
            event
        }
        (Some(b'>'), b'm') => {
            let mut it = parse_params(&params[1..]);
            match it.next() {
                Some(Some(4)) => {
                    let level = it.next().flatten().unwrap_or(0).min(2) as u8;
                    Some(ScanEvent::ModifyOtherKeys(level))
                }
                _ => None,
            }
        }
        (Some(b'0'..=b'9') | None, b'J') => match parse_params(params).next().flatten() {
            Some(2) => Some(ScanEvent::EraseDisplay),
            Some(3) => Some(ScanEvent::EraseScrollback),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<ScanEvent> {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.scan(c, &mut out);
        }
        out.into_iter().map(|(_, e)| e).collect()
    }

    #[test]
    fn osc7_with_bel_and_st() {
        assert_eq!(
            scan_all(&[b"a\x1b]7;file:///tmp\x07b"]),
            vec![ScanEvent::Osc(b"7;file:///tmp".to_vec())]
        );
        assert_eq!(
            scan_all(&[b"\x1b]133;A\x1b\\"]),
            vec![ScanEvent::Osc(b"133;A".to_vec())]
        );
    }

    #[test]
    fn split_across_every_boundary() {
        let seq: &[u8] = b"xx\x1b]133;D;1\x1b\\yy\x1b[2J\x1b[?1049h\x1b[>4;2m\x1bc";
        let whole = scan_all(&[seq]);
        assert_eq!(whole.len(), 5);
        for cut in 0..seq.len() {
            let (a, b) = seq.split_at(cut);
            assert_eq!(scan_all(&[a, b]), whole, "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = seq.chunks(1).collect();
        assert_eq!(scan_all(&bytes), whole);
    }

    #[test]
    fn offsets_point_at_final_byte() {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        let seq = b"ab\x1b[2Jc\x1b]7;x\x07";
        s.scan(seq, &mut out);
        assert_eq!(out[0].0, 5);
        assert_eq!(seq[out[0].0], b'J');
        assert_eq!(seq[out[1].0], 0x07);
    }

    #[test]
    fn ignores_other_oscs_and_bounds_memory() {
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', 100_000));
        big.push(0x07);
        assert!(scan_all(&[&big]).is_empty());
        let mut long7 = b"\x1b]7;file://".to_vec();
        long7.extend(std::iter::repeat_n(b'a', MAX_OSC + 10));
        long7.push(0x07);
        assert!(scan_all(&[&long7]).is_empty());
        assert_eq!(scan_all(&[b"\x1b]0;title\x07"]), vec![]);
    }

    #[test]
    fn modes() {
        assert_eq!(scan_all(&[b"\x1b[?9h"]), vec![ScanEvent::X10Mouse(true)]);
        assert_eq!(
            scan_all(&[b"\x1b[?1000;9l"]),
            vec![ScanEvent::X10Mouse(false)]
        );
        assert_eq!(
            scan_all(&[b"\x1b[>4m"]),
            vec![ScanEvent::ModifyOtherKeys(0)]
        );
        assert_eq!(scan_all(&[b"\x1b[3J"]), vec![ScanEvent::EraseScrollback]);
        assert_eq!(scan_all(&[b"\x1b[J\x1b[1J"]), vec![]);
        assert_eq!(scan_all(&[b"\x1b[2 J"]), vec![]);
    }

    #[test]
    fn dcs_strings_are_skipped() {
        assert_eq!(scan_all(&[b"\x1bP]7;x\x07\x1b\\"]), vec![]);
    }
}

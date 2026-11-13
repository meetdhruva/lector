//! Golden and behaviour tests of the headless emulator (no PTY).

use std::fmt::Write as _;

use lector_terminal::{
    Attrs, CellWidth, Damage, Direction, Emulator, EmulatorConfig, GridPoint, LinkSource,
    LinkTarget, ScrollAmount, SearchQuery, SelectionKind, Side, Snapshot, TermColor, TermSize,
    TerminalEvent, UnderlineStyle,
};

fn emu(cols: u16, rows: u16) -> Emulator {
    Emulator::new(EmulatorConfig {
        size: TermSize::new(cols, rows),
        ..EmulatorConfig::default()
    })
}

fn emu_scrollback(cols: u16, rows: u16, scrollback: usize) -> Emulator {
    Emulator::new(EmulatorConfig {
        size: TermSize::new(cols, rows),
        scrollback,
        ..EmulatorConfig::default()
    })
}

/// Text plus a compact description of each styled run.
fn dump(s: &Snapshot) -> String {
    let mut out = String::new();
    for (i, row) in s.rows.iter().enumerate() {
        let text = row.text();
        let _ = writeln!(
            out,
            "{i:02}|{}|{}",
            text.trim_end(),
            if row.wrapped { " (wrap)" } else { "" }
        );
        for run in row.runs() {
            let st = &run.style;
            let plain = st.fg == TermColor::Foreground
                && st.bg == TermColor::Background
                && st.attrs == Attrs::empty()
                && st.underline == UnderlineStyle::None
                && st.hyperlink.is_none();
            if plain {
                continue;
            }
            let _ = writeln!(
                out,
                "   col {} +{} {:?} fg={:?} bg={:?} attrs={:#04x} ul={:?}{}",
                run.col,
                run.cols,
                run.text.trim_end(),
                st.fg,
                st.bg,
                st.attrs.bits(),
                st.underline,
                st.hyperlink
                    .as_ref()
                    .map(|h| format!(" link={}", h.uri))
                    .unwrap_or_default()
            );
        }
    }
    let _ = writeln!(
        out,
        "cursor {},{} visible={}",
        s.cursor.row, s.cursor.col, s.cursor.visible
    );
    out
}

#[test]
fn golden_sgr_colors_and_attributes() {
    let mut e = emu(40, 6);
    e.feed(b"\x1b[31mred\x1b[0m \x1b[1;4mbold-ul\x1b[0m \x1b[38;5;208m256\x1b[0m\r\n");
    e.feed(b"\x1b[38;2;10;20;30;48;2;200;100;0mtrue\x1b[0m \x1b[3;9;2mital\x1b[0m \x1b[7minv\x1b[27m\r\n");
    e.feed(b"\x1b[4:3mcurl\x1b[0m \x1b[4:2mdbl\x1b[0m \x1b[8mhid\x1b[0m \x1b[94;101mbright\x1b[0m");
    insta::assert_snapshot!("sgr", dump(&e.snapshot()));
}

#[test]
fn golden_cursor_movement_and_erase() {
    let mut e = emu(10, 4);
    e.feed(b"0123456789abc\r\n");
    e.feed(b"\x1b[1;1HX\x1b[3;5HY\x1b[2;1H\x1b[K\x1b[1;4H\x1b[1K\x1b[4;1Hlast\x1b[2D\x1b[0K");
    insta::assert_snapshot!("cursor_erase", dump(&e.snapshot()));
}

#[test]
fn golden_scroll_region() {
    let mut e = emu(10, 5);
    e.feed(b"top\r\na\r\nb\r\nc\r\nbottom");
    // Region rows 2..4; scroll it up twice from the region's bottom.
    e.feed(b"\x1b[2;4r\x1b[4;1H\n\nnew\x1b[r");
    let s = e.snapshot();
    assert_eq!(s.text(), "top\nc\n\nnew\nbottom");
    // Lines scrolled inside a region that does not start at the top never enter scrollback.
    assert_eq!(s.history_size, 0);
}

#[test]
fn golden_alternate_screen() {
    let mut e = emu(10, 3);
    e.feed(b"shell$ ");
    e.feed(b"\x1b[?1049h\x1b[Hfull\r\nscreen");
    let s = e.snapshot();
    assert!(s.modes.alt_screen);
    assert_eq!(s.text(), "full\nscreen");
    e.feed(b"\x1b[?1049l");
    let s = e.snapshot();
    assert!(!s.modes.alt_screen);
    assert_eq!(s.text(), "shell$");
    assert_eq!((s.cursor.row, s.cursor.col), (0, 7));
}

#[test]
fn golden_wide_and_combining() {
    let mut e = emu(8, 4);
    e.feed("a\u{4e2d}\u{6587}b\r\ne\u{301}x\r\n".as_bytes());
    // A wide char that does not fit at the end of the line wraps, leaving a spacer.
    e.feed("1234567\u{4e2d}".as_bytes());
    let s = e.snapshot();
    let row0 = &s.rows[0];
    assert_eq!(row0.cells[1].width, CellWidth::Wide);
    assert_eq!(row0.cells[2].width, CellWidth::WideSpacer);
    assert_eq!(row0.text().trim_end(), "a\u{4e2d}\u{6587}b");
    let runs = row0.runs();
    assert_eq!(runs.iter().map(|r| r.cols).sum::<u32>(), 8);
    assert_eq!(
        s.rows[1].cells[0].zerowidth.as_deref(),
        Some(&['\u{301}'][..])
    );
    assert_eq!(s.rows[1].text().trim_end(), "e\u{301}x");
    assert_eq!(s.rows[2].cells[7].width, CellWidth::LeadingSpacer);
    assert!(s.rows[2].wrapped);
    insta::assert_snapshot!("wide", dump(&s));
}

#[test]
fn tabs_and_reflow() {
    let mut e = emu(20, 4);
    e.feed(b"a\tb\tc\r\n");
    // Copying keeps tabs; cells render them as blanks up to the tab stop.
    assert_eq!(e.line_text(0), "a\tb\tc");
    assert_eq!(e.snapshot().rows[0].text().trim_end(), "a       b       c");
    e.feed(b"0123456789ABCDEFGHIJKLMNOP");
    assert!(e.snapshot().rows[1].wrapped);
    e.resize(TermSize::new(10, 4));
    let s = e.snapshot();
    assert_eq!(s.damage, Damage::Full);
    // The long line reflows into three rows of 10, 10 and 6.
    let text: Vec<String> = (0..4)
        .map(|r| s.rows[r].text().trim_end().to_owned())
        .collect();
    assert!(text.contains(&"0123456789".to_owned()), "{text:?}");
    assert!(text.contains(&"KLMNOP".to_owned()), "{text:?}");
    e.resize(TermSize::new(30, 4));
    let s = e.snapshot();
    assert!(
        s.text().contains("0123456789ABCDEFGHIJKLMNOP"),
        "{}",
        s.text()
    );
}

#[test]
fn damage_tracks_single_rows() {
    let mut e = emu(20, 5);
    e.feed(b"one\r\ntwo\r\nthree");
    assert_eq!(e.snapshot().damage, Damage::Full);
    let before = e.snapshot();
    assert!(before.damage.is_empty() || before.damage == Damage::Rows(vec![2]));
    e.feed(b"\x1b[2;1HT");
    let s = e.snapshot();
    // Only the changed row (and the row the cursor left) are damaged.
    let Damage::Rows(rows) = &s.damage else {
        panic!("expected partial damage, got {:?}", s.damage)
    };
    assert!(rows.contains(&1), "{rows:?}");
    assert!(rows.iter().all(|&r| r == 1 || r == 2), "{rows:?}");
    // Undamaged rows are shared with the previous snapshot.
    assert!(std::sync::Arc::ptr_eq(&s.rows[0], &before.rows[0]));
    assert!(!std::sync::Arc::ptr_eq(&s.rows[1], &before.rows[1]));
    e.resize(TermSize::new(21, 5));
    assert_eq!(e.snapshot().damage, Damage::Full);
    e.feed(b"\r\n\r\n\r\n\r\n\r\n");
    e.snapshot();
    e.scroll(ScrollAmount::Lines(1));
    assert_eq!(e.snapshot().damage, Damage::Full);
    e.start_selection(SelectionKind::Simple, GridPoint::new(0, 0), Side::Left);
    assert_eq!(e.snapshot().damage, Damage::Full);
}

#[test]
fn osc7_cwd_and_split_chunks() {
    let mut e = emu(20, 3);
    let seq = b"\x1b]7;file:///tmp/a%20dir\x1b\\";
    for b in seq.iter() {
        e.feed(&[*b]);
    }
    assert_eq!(
        e.take_events(),
        vec![TerminalEvent::CwdChanged("/tmp/a dir".into())]
    );
    #[cfg(unix)]
    assert_eq!(e.cwd(), Some(std::path::Path::new("/tmp/a dir")));
    e.feed(b"\x1b]7;file://some-other-host.example/etc\x07");
    assert!(e.take_events().is_empty());
}

#[test]
fn title_is_sanitised() {
    let mut e = emu(20, 3);
    e.feed("\x1b]0;  hi\u{202e}there\u{2066}!\x07".as_bytes());
    let events = e.take_events();
    assert_eq!(
        events,
        vec![TerminalEvent::TitleChanged(Some("hithere!".into()))]
    );
    let long = format!("\x1b]2;{}\x07", "t".repeat(5000));
    e.feed(long.as_bytes());
    assert_eq!(
        e.title().map(str::len),
        Some(lector_terminal::MAX_TITLE_CHARS)
    );
}

#[test]
fn osc52_copy_allowed_read_denied() {
    let mut e = emu(20, 3);
    e.feed(b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(
        e.take_events(),
        vec![TerminalEvent::ClipboardCopy {
            text: "hello".into(),
            primary_selection: false
        }]
    );
    e.feed(b"\x1b]52;c;?\x07");
    assert!(
        e.take_responses().is_empty(),
        "clipboard contents must never be sent to the program"
    );
}

#[test]
fn osc8_hyperlinks() {
    let mut e = emu(30, 3);
    e.feed(b"see \x1b]8;id=x;https://example.com/a\x1b\\the docs\x1b]8;;\x1b\\ now");
    let s = e.snapshot();
    let link = s.rows[0].cells[4].style.hyperlink.clone();
    assert_eq!(
        link.as_ref().map(|h| h.uri.as_str()),
        Some("https://example.com/a")
    );
    assert!(s.rows[0].cells[3].style.hyperlink.is_none());
    let c = e
        .link_at(GridPoint::new(0, 6))
        .unwrap_or_else(|| panic!("no link"));
    assert_eq!(c.source, LinkSource::Hyperlink);
    assert_eq!(
        (c.start, c.end),
        (GridPoint::new(0, 4), GridPoint::new(0, 11))
    );
    assert_eq!(c.target, LinkTarget::Url("https://example.com/a".into()));
    assert_eq!(c.text, "the docs");
    assert!(e.link_at(GridPoint::new(0, 1)).is_none());
}

#[test]
fn detected_links_span_wrapped_lines() {
    let mut e = emu(12, 4);
    e.feed(b"err: src/lib/mod.rs:10:5 x");
    let c = e
        .link_at(GridPoint::new(1, 2))
        .unwrap_or_else(|| panic!("no link"));
    assert_eq!(c.text, "src/lib/mod.rs:10:5");
    assert_eq!(c.start, GridPoint::new(0, 5));
    assert_eq!(c.end, GridPoint::new(1, 11));
    assert_eq!(
        c.target,
        LinkTarget::File {
            path: "src/lib/mod.rs".into(),
            line: Some(9),
            col: Some(4)
        }
    );
    let all = e.visible_links();
    assert_eq!(all.len(), 1);
}

#[test]
fn shell_marks_and_command_blocks() {
    let mut e = emu(30, 5);
    e.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a b\r\n\x1b]133;D;0\x07");
    e.feed(
        b"\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;1\x07\x1b]133;A\x07$ ",
    );
    let blocks = e.command_blocks();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].prompt_line, 0);
    assert_eq!(blocks[0].command_start, Some(GridPoint::new(0, 2)));
    assert_eq!(blocks[0].output_line, Some(1));
    assert_eq!(blocks[0].end_line, Some(2));
    assert_eq!(blocks[0].exit_code, Some(0));
    assert!(!blocks[0].failed());
    assert!(blocks[1].failed());
    assert_eq!(e.command_text(&blocks[1]).as_deref(), Some("false"));
    assert!(!blocks[2].finished());
    let events = e.take_events();
    assert!(events.contains(&TerminalEvent::CommandFinished { exit_code: Some(1) }));
}

#[test]
fn shell_marks_split_across_chunks() {
    let whole: &[u8] =
        b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;3\x1b\\";
    let mut reference = emu(20, 5);
    reference.feed(whole);
    let want = reference.command_blocks();
    for cut in 1..whole.len() {
        let mut e = emu(20, 5);
        e.feed(&whole[..cut]);
        e.feed(&whole[cut..]);
        let got = e.command_blocks();
        assert_eq!(got.len(), want.len(), "cut {cut}");
        for (a, b) in got.iter().zip(&want) {
            assert_eq!(
                (
                    a.prompt_line,
                    a.command_start,
                    a.output_line,
                    a.end_line,
                    a.exit_code
                ),
                (
                    b.prompt_line,
                    b.command_start,
                    b.output_line,
                    b.end_line,
                    b.exit_code
                ),
                "cut {cut}"
            );
        }
    }
}

fn run_commands(e: &mut Emulator, n: usize, output_lines: usize) {
    for i in 0..n {
        let mut s = format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\r\n\x1b]133;C\x07");
        for j in 0..output_lines {
            let _ = write!(s, "out {i}.{j}\r\n");
        }
        let _ = write!(s, "\x1b]133;D;{}\x07", i % 2);
        e.feed(s.as_bytes());
    }
}

#[test]
fn marks_follow_scrolling_and_trimming() {
    let mut e = emu_scrollback(20, 5, 50);
    run_commands(&mut e, 40, 3);
    let blocks = e.command_blocks();
    let oldest = -(e.history_size() as i32);
    assert!(!blocks.is_empty());
    assert!(blocks.len() < 40, "trimmed blocks must be dropped");
    for b in &blocks {
        assert!(b.prompt_line >= oldest);
        if !b.truncated {
            // The prompt line still shows this block's command.
            let text = e.line_text(b.prompt_line);
            let id = b.command_start.map(|p| e.line_text(p.line));
            assert!(text.starts_with("$ cmd"), "{text:?} {id:?}");
            let output = b.output_line.map(|l| e.line_text(l)).unwrap_or_default();
            let n: String = text.trim_start_matches("$ cmd").to_owned();
            assert!(
                output.starts_with(&format!("out {n}.0")),
                "{output:?} vs {text:?}"
            );
        }
    }
    assert_eq!(blocks.last().map(|b| b.failed()), Some(true));
}

#[test]
fn marks_survive_clear_and_alt_screen() {
    let mut e = emu_scrollback(20, 5, 100);
    run_commands(&mut e, 2, 1);
    e.feed(b"\x1b[H\x1b[2J");
    e.feed(b"\x1b[?1049hTUI\r\n\r\n\r\n\r\n\r\n\r\nmore\x1b[?1049l");
    run_commands(&mut e, 1, 1);
    let blocks = e.command_blocks();
    assert_eq!(blocks.len(), 3);
    for b in &blocks {
        let text = e.line_text(b.prompt_line);
        assert!(text.starts_with("$ cmd"), "{text:?} at {}", b.prompt_line);
    }
    // CSI 3 J drops scrollback and the marks in it.
    e.feed(b"\x1b[3J");
    let left = e.command_blocks();
    assert!(left.iter().all(|b| b.prompt_line >= 0));
}

#[test]
fn marks_survive_reflow() {
    let mut e = emu_scrollback(20, 6, 1000);
    run_commands(&mut e, 5, 2);
    e.feed(b"a very long output line that wraps around\r\n");
    run_commands(&mut e, 2, 1);
    for cols in [10u16, 33, 7, 20] {
        e.resize(TermSize::new(cols, 6));
        for b in e.command_blocks() {
            let text = e.line_text(b.prompt_line);
            assert!(
                text.starts_with("$ cm") || text.starts_with("$ "),
                "cols {cols}: {text:?}"
            );
        }
    }
}

#[test]
fn scroll_to_command_jumps() {
    let mut e = emu_scrollback(20, 4, 100);
    run_commands(&mut e, 6, 2);
    let first = e
        .scroll_to_command(Direction::Previous)
        .unwrap_or_else(|| panic!("no previous"));
    assert_eq!(-(e.display_offset() as i32), first.prompt_line);
    let prev = e
        .scroll_to_command(Direction::Previous)
        .unwrap_or_else(|| panic!("no previous"));
    assert!(prev.prompt_line < first.prompt_line);
    let next = e
        .scroll_to_command(Direction::Next)
        .unwrap_or_else(|| panic!("no next"));
    assert_eq!(next.id, first.id);
}

#[test]
fn selection_kinds_and_text() {
    let mut e = emu(10, 8);
    e.feed("hello wor\u{4e2d}d\r\nfoo/bar.rs baz\r\nline3".as_bytes());
    // Simple selection across the soft wrap: joined without a newline, wide char once.
    e.start_selection(SelectionKind::Simple, GridPoint::new(0, 6), Side::Left);
    e.update_selection(GridPoint::new(1, 1), Side::Right);
    assert_eq!(e.selection_text().as_deref(), Some("wor\u{4e2d}"));
    // Word selection keeps paths whole.
    e.start_selection(SelectionKind::Word, GridPoint::new(2, 4), Side::Left);
    assert_eq!(e.selection_text().as_deref(), Some("foo/bar.rs"));
    // Line selection includes the trailing newline.
    e.start_selection(SelectionKind::Line, GridPoint::new(4, 0), Side::Left);
    assert_eq!(e.selection_text().as_deref(), Some("line3\n"));
    // Block selection.
    e.start_selection(SelectionKind::Block, GridPoint::new(0, 0), Side::Left);
    e.update_selection(GridPoint::new(2, 2), Side::Right);
    assert_eq!(e.selection_text().as_deref(), Some("hel\n\u{4e2d}d\nfoo"));
    let span = e
        .snapshot()
        .selection
        .unwrap_or_else(|| panic!("no selection"));
    assert!(span.block);
    e.clear_selection();
    assert_eq!(e.selection_text(), None);
}

#[test]
fn selection_follows_scrolling_output() {
    let mut e = emu(10, 3);
    e.feed(b"keep me\r\n");
    e.start_selection(SelectionKind::Line, GridPoint::new(0, 0), Side::Left);
    e.feed(b"x\r\ny\r\nz\r\n");
    assert_eq!(e.selection_text().as_deref(), Some("keep me\n"));
    let span = e.selection_span().unwrap_or_else(|| panic!("no span"));
    assert_eq!(span.start.line, -2);
}

#[test]
fn search_plain_regex_and_case() {
    let mut e = emu(20, 3);
    for i in 0..10 {
        e.feed(format!("line {i} Foo\r\n").as_bytes());
    }
    e.set_search(&SearchQuery {
        pattern: "foo".into(),
        regex: false,
        case_sensitive: false,
    })
    .unwrap_or_else(|e| panic!("{e}"));
    let first = e
        .search_next(Direction::Previous)
        .unwrap_or_else(|| panic!("no match"));
    assert_eq!(e.line_text(first.start.line), "line 9 Foo");
    let second = e
        .search_next(Direction::Previous)
        .unwrap_or_else(|| panic!("no match"));
    assert_eq!(second.start.line, first.start.line - 1);
    assert_eq!(e.line_text(second.start.line), "line 8 Foo");
    let back = e
        .search_next(Direction::Next)
        .unwrap_or_else(|| panic!("no match"));
    assert_eq!(back.start.line, first.start.line);
    assert!(!e.visible_matches().is_empty());

    e.set_search(&SearchQuery {
        pattern: "foo".into(),
        regex: false,
        case_sensitive: true,
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(e.search_next(Direction::Previous), None);

    e.set_search(&SearchQuery {
        pattern: r"line [3-4] F".into(),
        regex: true,
        case_sensitive: true,
    })
    .unwrap_or_else(|e| panic!("{e}"));
    let m = e
        .search_next(Direction::Previous)
        .unwrap_or_else(|| panic!("no match"));
    assert_eq!(e.line_text(m.start.line), "line 4 Foo");
    assert_eq!(m.end.col - m.start.col + 1, 8);
    // A search reaching into scrollback scrolls the view to it.
    assert!(e.display_offset() > 0);

    // Plain search escapes regex syntax.
    e.feed(b"a.b(c)\r\n");
    e.set_search(&SearchQuery {
        pattern: "b(c".into(),
        regex: false,
        case_sensitive: true,
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(e.search_next(Direction::Previous).is_some());
    assert!(e
        .set_search(&SearchQuery {
            pattern: "(".into(),
            regex: true,
            case_sensitive: true
        })
        .is_err());
}

#[test]
fn answers_terminal_queries() {
    let mut e = emu(20, 5);
    e.feed(b"ab\x1b[6n");
    assert_eq!(e.take_responses(), b"\x1b[1;3R".to_vec());
    e.feed(b"\x1b[c");
    let da = e.take_responses();
    assert!(da.starts_with(b"\x1b[?"), "{da:?}");
    e.feed(b"\x1b]11;?\x07");
    let bg = String::from_utf8_lossy(&e.take_responses()).into_owned();
    assert!(bg.starts_with("\x1b]11;rgb:"), "{bg:?}");
    e.feed(b"\x1b[18t");
    assert_eq!(e.take_responses(), b"\x1b[8;5;20t".to_vec());
}

#[test]
fn modes_from_program() {
    let mut e = emu(20, 5);
    e.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[>4;2m");
    let m = e.snapshot().modes;
    assert!(m.app_cursor && m.bracketed_paste && m.sgr_mouse && m.focus_reporting);
    assert_eq!(m.mouse, lector_terminal::MouseMode::Drag);
    assert_eq!(m.modify_other_keys, 2);
    assert!(m.wants_mouse());
    assert_eq!(
        e.encode_paste("x\x1b[201~y"),
        b"\x1b[200~x[201~y\x1b[201~".to_vec()
    );
    e.feed(b"\x1b[?1002l\x1b[?9h");
    assert_eq!(e.modes().mouse, lector_terminal::MouseMode::X10);
    e.feed(b"\x1bc");
    assert_eq!(e.modes().mouse, lector_terminal::MouseMode::None);
}

#[test]
fn cursor_hidden_and_scrolled_out() {
    let mut e = emu(10, 3);
    e.feed(b"\x1b[?25l");
    assert!(!e.snapshot().cursor.visible);
    e.feed(b"\x1b[?25h\x1b[4 q");
    let c = e.snapshot().cursor;
    assert!(c.visible);
    assert_eq!(c.shape, lector_terminal::CursorShape::Underline);
    for _ in 0..10 {
        e.feed(b"x\r\n");
    }
    e.scroll(ScrollAmount::Top);
    assert!(!e.snapshot().cursor.visible);
    e.scroll(ScrollAmount::Bottom);
    assert!(e.snapshot().cursor.visible);
}

#[test]
fn scrollback_is_bounded() {
    let mut e = emu_scrollback(20, 5, 100);
    for i in 0..1000 {
        e.feed(format!("{i}\r\n").as_bytes());
    }
    assert_eq!(e.history_size(), 100);
    e.scroll(ScrollAmount::Top);
    assert_eq!(e.display_offset(), 100);
    assert_eq!(e.snapshot().rows[0].text().trim_end(), "896");
}

mod props {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        #[test]
        fn arbitrary_bytes_never_panic(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..512), 0..16)) {
            let mut e = emu_scrollback(17, 7, 30);
            for c in &chunks {
                e.feed(c);
                let s = e.snapshot();
                prop_assert_eq!(s.rows.len(), 7);
                prop_assert!(s.rows.iter().all(|r| r.cells.len() == 17));
            }
            prop_assert!(e.history_size() <= 30);
            prop_assert!(e.take_events().len() <= 1024);
            let _ = e.selection_text();
            let _ = e.visible_links();
            let _ = e.command_blocks();
            e.resize(TermSize::new(9, 4));
            let _ = e.snapshot();
        }

        #[test]
        fn escape_heavy_input_never_panics(parts in proptest::collection::vec(prop_oneof![
            Just(b"\x1b[".to_vec()), Just(b"\x1b]".to_vec()), Just(b"133;".to_vec()), Just(b"7;file:///".to_vec()),
            Just(b"\x07".to_vec()), Just(b"\x1b\\".to_vec()), Just(b"2J".to_vec()), Just(b"?1049h".to_vec()),
            Just(b"?1049l".to_vec()), Just(b"\r\n".to_vec()), Just(b"A".to_vec()), Just(b"D;1".to_vec()),
            Just("\u{4e2d}".as_bytes().to_vec()), Just(b"\x1bc".to_vec()), Just(b"3J".to_vec()), Just(b"8;;".to_vec()),
        ], 0..200)) {
            let mut e = emu_scrollback(11, 5, 20);
            for p in &parts {
                e.feed(p);
            }
            let _ = e.snapshot();
            for b in e.command_blocks() {
                prop_assert!(b.prompt_line >= -(e.history_size() as i32));
            }
            prop_assert!(e.command_blocks().len() <= 4096);
        }
    }
}

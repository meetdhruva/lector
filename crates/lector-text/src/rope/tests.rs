//! Unit tests of the rope against a `String` model: chunk boundaries, slices, replace and `apply_edits`.

use super::*;
use proptest::prelude::*;

/// Reference implementation of the line/point semantics on a `String`.
struct Model<'a>(&'a str);

impl Model<'_> {
    fn floor(&self, offset: usize) -> usize {
        floor_char_boundary(self.0, offset.min(self.0.len()))
    }
    fn line_starts(&self) -> Vec<usize> {
        std::iter::once(0)
            .chain(self.0.match_indices('\n').map(|(i, _)| i + 1))
            .collect()
    }
    fn offset_to_point(&self, offset: usize) -> Point {
        let offset = self.floor(offset);
        let prefix = &self.0[..offset];
        let row = prefix.matches('\n').count() as u32;
        let column = prefix.rfind('\n').map_or(offset, |i| offset - i - 1) as u32;
        Point::new(row, column)
    }
    fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        let offset = self.floor(offset);
        let prefix = &self.0[..offset];
        let row = prefix.matches('\n').count() as u32;
        let line = &prefix[prefix.rfind('\n').map_or(0, |i| i + 1)..];
        PointUtf16::new(row, line.encode_utf16().count() as u32)
    }
    fn raw_line(&self, row: u32) -> Option<Range<usize>> {
        let starts = self.line_starts();
        let start = *starts.get(row as usize)?;
        let end = starts.get(row as usize + 1).map_or(self.0.len(), |s| s - 1);
        Some(start..end)
    }
    fn point_to_offset(&self, point: Point) -> usize {
        match self.raw_line(point.row) {
            None => self.0.len(),
            Some(line) => self.floor((line.start + point.column as usize).min(line.end)),
        }
    }
    fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        match self.raw_line(point.row) {
            None => self.0.len(),
            Some(line) => {
                let mut units = 0;
                for (i, ch) in self.0[line.clone()].char_indices() {
                    if units + ch.len_utf16() > point.column as usize {
                        return line.start + i;
                    }
                    units += ch.len_utf16();
                }
                line.end
            }
        }
    }
    fn line_range(&self, row: u32) -> Range<usize> {
        let starts = self.line_starts();
        match self.raw_line(row) {
            None => self.0.len()..self.0.len(),
            Some(mut line) => {
                let has_newline = (row as usize + 1) < starts.len();
                if has_newline && line.end > line.start && self.0.as_bytes()[line.end - 1] == b'\r'
                {
                    line.end -= 1;
                }
                line
            }
        }
    }
    fn summary(&self) -> (usize, usize, usize, u32, u32) {
        let mut longest = (0u32, 0u32);
        for (row, line) in self.0.split('\n').enumerate() {
            let chars = line.chars().count() as u32;
            if chars > longest.1 {
                longest = (row as u32, chars);
            }
        }
        (
            self.0.len(),
            self.0.chars().count(),
            self.0.encode_utf16().count(),
            longest.0,
            longest.1,
        )
    }
}

/// Cheap consistency checks for big texts: content, summary, line index.
fn check_light(rope: &Rope, text: &str) {
    assert_eq!(rope.to_string(), text);
    assert_eq!(rope.summary(), TextSummary::from_text(text));
    assert_eq!(rope.line_count() as usize, text.matches('\n').count() + 1);
    let model = Model(text);
    for offset in (0..=text.len()).step_by(97) {
        assert_eq!(rope.offset_to_point(offset), model.offset_to_point(offset));
    }
    for row in (0..rope.line_count()).step_by(31) {
        assert_eq!(
            rope.point_to_offset(Point::new(row, 0)),
            model.line_starts()[row as usize]
        );
    }
}

fn check_rope(rope: &Rope, text: &str) {
    let model = Model(text);
    assert_eq!(rope.to_string(), text);
    assert_eq!(rope.len(), text.len());
    assert_eq!(rope.line_count() as usize, text.matches('\n').count() + 1);
    let summary = rope.summary();
    assert_eq!(
        (
            summary.len,
            summary.chars,
            summary.len_utf16,
            summary.longest_row,
            summary.longest_row_chars
        ),
        model.summary()
    );
    assert_eq!(rope.max_point(), model.offset_to_point(text.len()));
    for offset in 0..=text.len() + 1 {
        let point = rope.offset_to_point(offset);
        assert_eq!(
            point,
            model.offset_to_point(offset),
            "offset_to_point({offset})"
        );
        assert_eq!(
            rope.offset_to_point_utf16(offset),
            model.offset_to_point_utf16(offset)
        );
        let clipped = model.floor(offset);
        assert_eq!(rope.clip_offset(offset, Bias::Left), clipped);
        assert_eq!(rope.point_to_offset(point), clipped);
        assert_eq!(
            rope.point_utf16_to_offset(rope.offset_to_point_utf16(offset)),
            clipped
        );
        assert_eq!(
            rope.offset_to_char_offset(offset).0,
            text[..clipped].chars().count(),
        );
        assert_eq!(
            rope.char_offset_to_offset(rope.offset_to_char_offset(offset)),
            clipped
        );
        let right = rope.clip_offset(offset, Bias::Right);
        assert!(right >= offset.min(text.len()) && text.is_char_boundary(right));
        assert_eq!(rope.chars_at(offset).collect::<String>(), text[clipped..]);
        assert_eq!(
            rope.reversed_chars_at(offset).collect::<String>(),
            text[..clipped].chars().rev().collect::<String>()
        );
    }
    let rows = rope.line_count() + 1;
    for row in 0..rows {
        assert_eq!(
            rope.line_range(row),
            model.line_range(row),
            "line_range({row})"
        );
        for column in [0, 1, 2, 3, 5, 100] {
            let point = Point::new(row, column);
            assert_eq!(
                rope.point_to_offset(point),
                model.point_to_offset(point),
                "{point:?}"
            );
            let point = PointUtf16::new(row, column);
            assert_eq!(
                rope.point_utf16_to_offset(point),
                model.point_utf16_to_offset(point)
            );
        }
    }
}

#[test]
fn basics() {
    assert_eq!(Rope::new().line_count(), 1);
    assert_eq!(Rope::from("a\nb").line_count(), 2);
    assert_eq!(Rope::from("a\n").line_count(), 2);
    let rope = Rope::from("ab\r\ncd\r\n\r\nx\r");
    assert_eq!(rope.line_range(0), 0..2);
    assert_eq!(rope.line_range(1), 4..6);
    assert_eq!(rope.line_range(2), 8..8);
    assert_eq!(rope.line_range(3), 10..12);
    assert_eq!(rope.line_len(3), 2);
    for text in [
        "",
        "a",
        "\n",
        "hello\nworld\n",
        "αβγ\n😀x\r\nyz\n\n€€€€€€€€€€€€€€€€€€€€€€\r\n",
        "line one is fairly long, to span chunks\r\nshort\n\u{10FFFF}\u{10FFFF}\n",
    ] {
        check_rope(&Rope::from(text), text);
    }
}

#[test]
fn chunks_respect_char_and_crlf_boundaries() {
    let text = "ab\r\n".repeat(50) + &"é😀".repeat(40);
    let rope = Rope::from(text.as_str());
    let mut previous_ended_with_cr = false;
    for chunk in rope.chunk_tree().iter() {
        let chunk = chunk.as_str();
        assert!(chunk.len() <= MAX_CHUNK);
        assert!(!(previous_ended_with_cr && chunk.starts_with('\n')));
        previous_ended_with_cr = chunk.ends_with('\r');
    }
    check_rope(&rope, &text);
}

#[test]
fn large_parallel_build() {
    let line = "fn main() { println!(\"héllo wörld\"); }\r\n";
    let text = line.repeat(PARALLEL_THRESHOLD / line.len() + 100);
    let rope = Rope::from(text.as_str());
    assert_eq!(rope.len(), text.len());
    assert_eq!(rope.summary(), TextSummary::from_text(&text));
    let row = 12345;
    let range = rope.line_range(row);
    assert_eq!(&text[range], line.trim_end());
    assert_eq!(rope.slice(100..5000).to_string(), text[100..5000]);
}

#[test]
fn versions() {
    let mut rope = Rope::from("abc");
    let clone = rope.clone();
    assert_eq!(rope.version(), clone.version());
    rope.replace(1..2, "x");
    assert_ne!(rope.version(), clone.version());
    assert_eq!(rope.to_string(), "axc");
    assert_eq!(clone.to_string(), "abc");
}

fn text_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("a".to_string()),
            Just("xyz".to_string()),
            Just("\n".to_string()),
            Just("\r\n".to_string()),
            Just("\r".to_string()),
            Just("é".to_string()),
            Just("€".to_string()),
            Just("😀".to_string()),
            "[a-z ]{0,20}",
        ],
        0..40,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn rope_matches_string_model(text in text_strategy()) {
        let rope = Rope::from(text.as_str());
        check_rope(&rope, &text);
        // Building by pushes must give the same text and summary.
        let mut pushed = Rope::new();
        for piece in text.split_inclusive(['\n', 'z']) {
            pushed.push(piece);
        }
        prop_assert_eq!(pushed.summary(), rope.summary());
        prop_assert_eq!(&pushed, &rope);
    }

    #[test]
    fn slices_and_ranges(text in text_strategy(), a in 0usize..300, b in 0usize..300) {
        let rope = Rope::from(text.as_str());
        let (start, end) = (a.min(b).min(text.len()), a.max(b).min(text.len()));
        let model = Model(&text);
        let (s, e) = (model.floor(start), model.floor(end));
        prop_assert_eq!(rope.slice(start..end).to_string(), &text[s..e]);
        prop_assert_eq!(rope.text_for_range(start..end), &text[s..e]);
        let reversed: Vec<&str> = rope.reversed_chunks_in_range(start..end).collect();
        prop_assert_eq!(reversed.into_iter().rev().collect::<String>(), &text[s..e]);
        prop_assert_eq!(rope.text_summary_for_range(start..end), TextSummary::from_text(&text[s..e]));
    }

    #[test]
    fn replace_matches_model(
        text in text_strategy(),
        edits in proptest::collection::vec((0usize..300, 0usize..300, text_strategy()), 1..6),
    ) {
        let mut rope = Rope::from(text.as_str());
        let mut model = text.clone();
        for (a, b, insert) in edits {
            let (start, end) = (a.min(b).min(model.len()), a.max(b).min(model.len()));
            let (s, e) = (floor_char_boundary(&model, start), floor_char_boundary(&model, end));
            rope.replace(start..end, &insert);
            model.replace_range(s..e, &insert);
            prop_assert_eq!(rope.to_string(), model.as_str());
            prop_assert_eq!(rope.summary(), TextSummary::from_text(&model));
        }
        check_rope(&rope, &model);
    }

    /// Batched edits equal the same edits applied to a `String` (both the
    /// copy path for short gaps and the shared-subtree path for long ones).
    #[test]
    fn apply_edits_matches_model(
        text in text_strategy(),
        repeat in 1usize..14,
        cuts in proptest::collection::vec(0usize..8000, 0..24),
        inserts in proptest::collection::vec(text_strategy(), 12),
    ) {
        let text = text.repeat(repeat);
        let mut cuts = cuts;
        cuts.sort_unstable();
        let edits: Vec<(Range<usize>, &str)> = cuts
            .chunks_exact(2)
            .zip(&inserts)
            .map(|(c, t)| (c[0]..c[1], t.as_str()))
            .collect();
        let mut rope = Rope::from(text.as_str());
        let before = rope.clone();
        // The public entry point and both strategies on their own.
        let by_splicing = Rope { chunks: rope.apply_edits_by_splicing(&edits), version: 0 };
        let by_leaves = if text.is_empty() {
            None
        } else {
            Some(Rope { chunks: rope.apply_edits_by_leaves(&edits), version: 0 })
        };
        rope.apply_edits(&edits);
        let mut model = text.clone();
        // offsets always refer to the original text
        for (r, t) in edits.iter().rev() {
            let s = floor_char_boundary(&text, r.start.min(text.len()));
            let e = floor_char_boundary(&text, r.end.min(text.len()));
            model.replace_range(s..e, t);
        }
        for candidate in [Some(&rope), Some(&by_splicing), by_leaves.as_ref()].into_iter().flatten() {
            prop_assert_eq!(candidate.to_string(), model.as_str());
            prop_assert_eq!(candidate.summary(), TextSummary::from_text(&model));
            check_light(candidate, &model);
            // no tiny chunk is left behind by a rebuilt tail
            let sizes: Vec<usize> = candidate.chunk_tree().iter().map(|c| c.as_str().len()).collect();
            prop_assert!(sizes.iter().all(|&n| n > 0), "empty chunk in {sizes:?}");
        }
        // the old snapshot is untouched
        prop_assert_eq!(before.to_string(), text);
    }
}

#[test]
fn apply_edits_basics() {
    let mut r = Rope::from("hello world");
    let v = r.version();
    r.apply_edits::<&str>(&[]);
    assert_eq!(r.version(), v, "no edits, no new version");
    r.apply_edits(&[(0..5, "HELLO"), (6..11, "there"), (11..11, "!")]);
    assert_eq!(r.to_string(), "HELLO there!");
    assert_ne!(r.version(), v);
    // inserts at the same offset keep their order; pure deletion; clamping
    let mut r = Rope::from("abc");
    r.apply_edits(&[(1..1, "x"), (1..1, "y"), (2..99, "")]);
    assert_eq!(r.to_string(), "axyb");
    let mut r = Rope::from("");
    r.apply_edits(&[(0..0, "new")]);
    assert_eq!(r.to_string(), "new");
    // malformed (overlapping / unsorted) input never panics and keeps text sane
    let mut r = Rope::from("0123456789");
    r.apply_edits(&[(4..8, "A"), (2..6, "B")]);
    assert_eq!(r.to_string(), "0123AB89");
    // char boundaries: an edit inside a multi-byte char is clipped down
    let mut r = Rope::from("a\u{e9}b");
    r.apply_edits(&[(2..3, "X")]);
    assert_eq!(r.to_string(), "aXb");
}

#[test]
fn apply_edits_shares_long_gaps_and_stays_balanced() {
    let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    let mut r = Rope::from(text.as_str());
    let edits: Vec<(Range<usize>, &str)> = (0..500)
        .map(|i| {
            let at = i * 700;
            (at..at, "x")
        })
        .collect();
    r.apply_edits(&edits);
    let mut model = text.clone();
    for (rg, t) in edits.iter().rev() {
        let e = floor_char_boundary(&model, rg.start);
        model.insert_str(e, t);
    }
    check_light(&r, &model);
    assert_eq!(r.to_string(), model);
    // later edits still work on the result
    r.apply_edits(&[(0..1, "L"), (r.len()..r.len(), "end")]);
    assert!(r.to_string().starts_with("Lline 0") && r.to_string().ends_with("end"));
}

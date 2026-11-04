//! Property tests at production chunk sizes (unit tests use tiny chunks).

use std::io::Write;

use lector_text::{Bias, MappedText, Point, PointUtf16, Rope, Text, TextSummary};
use proptest::prelude::*;

fn floor(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn model_point(text: &str, offset: usize) -> Point {
    let prefix = &text[..floor(text, offset)];
    let row = prefix.matches('\n').count() as u32;
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |i| prefix.len() - i - 1) as u32;
    Point::new(row, column)
}

fn model_point_utf16(text: &str, offset: usize) -> PointUtf16 {
    let prefix = &text[..floor(text, offset)];
    let row = prefix.matches('\n').count() as u32;
    let line = &prefix[prefix.rfind('\n').map_or(0, |i| i + 1)..];
    PointUtf16::new(row, line.encode_utf16().count() as u32)
}

fn model_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .enumerate()
        .map(|(row, line)| {
            let is_last = row == text.matches('\n').count();
            if is_last {
                line
            } else {
                line.strip_suffix('\r').unwrap_or(line)
            }
        })
        .collect()
}

fn text_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            "[a-z ]{0,200}",
            Just("\n".to_string()),
            Just("\r\n".to_string()),
            Just("é€😀".to_string()),
            "[ -~]{0,60}",
        ],
        0..80,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn rope_conversions(text in text_strategy(), offsets in proptest::collection::vec(0usize..20_000, 20)) {
        let rope = Rope::from(text.as_str());
        prop_assert_eq!(rope.summary(), TextSummary::from_text(&text));
        for offset in offsets {
            let point = rope.offset_to_point(offset);
            prop_assert_eq!(point, model_point(&text, offset));
            prop_assert_eq!(rope.point_to_offset(point), floor(&text, offset));
            let point = rope.offset_to_point_utf16(offset);
            prop_assert_eq!(point, model_point_utf16(&text, offset));
            prop_assert_eq!(rope.point_utf16_to_offset(point), floor(&text, offset));
            prop_assert_eq!(rope.clip_offset(offset, Bias::Left), floor(&text, offset));
        }
        let lines = model_lines(&text);
        prop_assert_eq!(rope.line_count() as usize, lines.len());
        for (row, line) in lines.iter().enumerate() {
            prop_assert_eq!(&rope.text_for_range(rope.line_range(row as u32)), line);
        }
    }

    #[test]
    fn rope_replace(
        text in text_strategy(),
        edits in proptest::collection::vec((0usize..20_000, 0usize..20_000, text_strategy()), 1..8),
    ) {
        let mut rope = Rope::from(text.as_str());
        let mut model = text;
        for (a, b, insert) in edits {
            let (start, end) = (floor(&model, a.min(b)), floor(&model, a.max(b)));
            rope.replace(start..end, &insert);
            model.replace_range(start..end, &insert);
            prop_assert_eq!(rope.len(), model.len());
        }
        prop_assert_eq!(rope.to_string(), model.clone());
        prop_assert_eq!(rope.summary(), TextSummary::from_text(&model));
    }

    #[test]
    fn rope_apply_edits_matches_sequential_replace(
        text in text_strategy(),
        cuts in proptest::collection::vec(0usize..20_000, 0..40),
        inserts in proptest::collection::vec(text_strategy(), 20),
    ) {
        let mut cuts = cuts;
        cuts.sort_unstable();
        let edits: Vec<(std::ops::Range<usize>, &str)> = cuts
            .chunks_exact(2)
            .zip(&inserts)
            .map(|(c, t)| (c[0]..c[1], t.as_str()))
            .collect();
        let mut batched = Rope::from(text.as_str());
        batched.apply_edits(&edits);
        // Reference: the same edits one by one, back to front.
        let mut single = Rope::from(text.as_str());
        let mut model = text.clone();
        for (r, t) in edits.iter().rev() {
            let (s, e) = (floor(&text, r.start), floor(&text, r.end));
            single.replace(s..e, t);
            model.replace_range(s..e, t);
        }
        prop_assert_eq!(batched.to_string(), model.clone());
        prop_assert_eq!(&batched, &single);
        prop_assert_eq!(batched.summary(), TextSummary::from_text(&model));
        prop_assert_eq!(batched.line_count() as usize, model.matches('\n').count() + 1);
    }

    #[test]
    fn mapped_matches_rope(text in text_strategy(), repeat in 1usize..40) {
        let text = text.repeat(repeat);
        let mut file = tempfile::NamedTempFile::new().map_err(|e| TestCaseError::fail(e.to_string()))?;
        file.write_all(text.as_bytes()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        file.flush().map_err(|e| TestCaseError::fail(e.to_string()))?;
        let mapped = MappedText::open(file.path()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let rope = Rope::from(text.as_str());
        prop_assert_eq!(mapped.line_count(), rope.line_count());
        for row in 0..rope.line_count() + 1 {
            prop_assert_eq!(mapped.line_range(row), rope.line_range(row));
            let point = Point::new(row, 7);
            prop_assert_eq!(mapped.point_to_offset(point), rope.point_to_offset(point));
        }
        for offset in (0..=text.len()).step_by(97) {
            prop_assert_eq!(mapped.offset_to_point(offset), rope.offset_to_point(offset));
            prop_assert_eq!(mapped.offset_to_point_utf16(offset), rope.offset_to_point_utf16(offset));
        }
    }

    #[test]
    fn mapped_handles_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
        let mut file = tempfile::NamedTempFile::new().map_err(|e| TestCaseError::fail(e.to_string()))?;
        file.write_all(&bytes).map_err(|e| TestCaseError::fail(e.to_string()))?;
        file.flush().map_err(|e| TestCaseError::fail(e.to_string()))?;
        let text = Text::open_mapped(file.path()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
        prop_assert_eq!(text.line_count() as usize, newlines + 1);
        let joined: String = text.chunks().collect();
        prop_assert_eq!(joined, String::from_utf8_lossy(&bytes).into_owned());
        for offset in 0..=bytes.len() {
            let point = text.offset_to_point(offset);
            let clipped = text.clip_offset(offset, Bias::Left);
            prop_assert_eq!(text.point_to_offset(point), clipped);
            let utf16 = text.offset_to_point_utf16(offset);
            prop_assert_eq!(text.point_utf16_to_offset(utf16), clipped);
            let _ = text.text_for_range(offset..bytes.len());
        }
        for row in 0..text.line_count() + 1 {
            let _ = text.line(row);
        }
    }
}

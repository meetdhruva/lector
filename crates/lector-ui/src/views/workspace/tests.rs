//! Tests of the workspace module.

use super::detect_file_meta;

#[test]
fn detects_indentation_and_line_endings() {
    let m = detect_file_meta("fn a() {\n    if x {\n        y();\n    }\n}\n");
    assert_eq!(m.indentation.as_ref(), "Spaces: 4");
    assert_eq!(m.line_ending.as_ref(), "LF");
    let m = detect_file_meta("a:\r\n  b:\r\n    c\r\n");
    assert_eq!(m.indentation.as_ref(), "Spaces: 2");
    assert_eq!(m.line_ending.as_ref(), "CRLF");
    assert_eq!(
        detect_file_meta("a\n\tb\n\tc\n").indentation.as_ref(),
        "Tabs"
    );
}

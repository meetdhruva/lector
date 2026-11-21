//! The real check against this repository: `cargo test -p lector-archcheck` protects the
//! committed baseline, layering, allow files and the generated crate graph.

use std::path::Path;

#[test]
fn the_repository_passes_its_own_ratchet() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let args = vec![
        "all".to_string(),
        "--root".to_string(),
        root.to_string_lossy().into_owned(),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = lector_archcheck::run(&args, &mut out, &mut err);
    assert_eq!(
        code,
        0,
        "arch-check all failed (lower the baseline with scripts/arch-check --update, never raise it):\n{}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
}

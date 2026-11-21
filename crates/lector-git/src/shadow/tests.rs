//! Unit tests of the shadow repository's manifest and helpers.

use super::util::quote_path;
use super::*;

#[test]
fn rel_paths_are_checked() {
    assert_eq!(check_rel("a/b.txt").unwrap(), "a/b.txt");
    assert_eq!(check_rel("./a//b").unwrap(), "a/b");
    for bad in [
        "",
        "..",
        "../x",
        "/etc/passwd",
        "a/../../x",
        ".git/config",
        "sub/.GIT/x",
    ] {
        assert!(check_rel(bad).is_err(), "{bad}");
    }
}

#[test]
fn within() {
    assert!(is_within("a/b", "a"));
    assert!(is_within("a", "a"));
    assert!(!is_within("ab", "a"));
    assert!(!is_within("a", "a/b"));
}

#[test]
fn quoting() {
    assert_eq!(quote_path("plain name.txt"), "plain name.txt");
    assert_eq!(quote_path("a\nb"), "\"a\\nb\"");
    assert_eq!(quote_path("q\"x\\"), "\"q\\\"x\\\\\"");
    assert_eq!(quote_path("tab\there"), "\"tab\\there\"");
}

#[test]
fn manifest_round_trips() {
    let mut plan = Plan::new(SnapshotKind::Index);
    let oid = Oid::from_hex("1111111111111111111111111111111111111111").unwrap();
    plan.index.push(IndexRecord {
        path: "weird\nname".into(),
        mode: "100644".into(),
        oid,
        stage: 2,
    });
    plan.no_index.push("gone".into());
    plan.intent_to_add.push("ita.txt".into());
    plan.absent.push("missing".into());
    let parsed = Manifest::parse(&plan.manifest()).unwrap();
    assert_eq!(parsed.kind, SnapshotKind::Index);
    assert_eq!(parsed.index.len(), 1);
    assert_eq!(parsed.index[0].path, "weird\nname");
    assert_eq!(parsed.index[0].stage, 2);
    assert_eq!(parsed.no_index, ["gone"]);
    assert_eq!(parsed.intent_to_add, ["ita.txt"]);
    assert_eq!(parsed.absent, ["missing"]);
    assert!(Manifest::parse(b"nope\0").is_err());
}

#[test]
fn ref_names_carry_the_time() {
    assert_eq!(
        ref_time("refs/lector/snapshots/1700000000-0001-9"),
        Some(1_700_000_000)
    );
    assert_eq!(ref_time("refs/heads/main"), None);
}

#[test]
fn hash_is_stable_and_separates_parts() {
    assert_eq!(fnv128(["abc"]), fnv128(["abc"]));
    assert_ne!(fnv128(["ab", "c"]), fnv128(["a", "bc"]));
    assert_eq!(fnv128(["x"]).len(), 32);
}

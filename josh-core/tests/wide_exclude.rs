use std::fmt::Write as _;

use josh_core::filter;

fn make_exclude_spec(n: usize, seed: u32) -> String {
    // Keep this URL-compatible: no whitespace/newlines.
    let mut spec = String::from(":exclude[");
    for i in 0..n {
        if i != 0 {
            spec.push(',');
        }
        let dir = (i % 1024) as u32;
        write!(&mut spec, "::dir{dir:04}/file_{seed:08}_{i:08}.txt").expect("write spec");
    }
    spec.push(']');
    spec
}

#[test]
fn wide_exclude_roundtrip_spec() {
    let n = std::env::var("JOSH_WIDE_EXCLUDE_N")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2_000);

    let spec_str = make_exclude_spec(n, 0);
    let parsed = filter::parse(&spec_str).expect("parse wide exclude");
    let roundtrip = filter::spec(parsed);
    let reparsed = filter::parse(&roundtrip).expect("reparse wide exclude");
    assert_eq!(parsed, reparsed);
}

#[test]
fn wide_exclude_spec_is_deterministic() {
    let spec_str = make_exclude_spec(1_000, 1);
    let parsed = filter::parse(&spec_str).expect("parse wide exclude");
    let first = filter::spec(parsed);
    let second = filter::spec(parsed);
    assert_eq!(first, second);
}

#[test]
#[ignore]
fn wide_exclude_roundtrip_spec_50k() {
    let spec_str = make_exclude_spec(50_000, 2);
    let parsed = filter::parse(&spec_str).expect("parse wide exclude");
    let roundtrip = filter::spec(parsed);
    let reparsed = filter::parse(&roundtrip).expect("reparse wide exclude");
    assert_eq!(parsed, reparsed);
}

use super::*;

fn section(contents: Option<&str>) -> String {
    let mut out = String::new();
    write_section(&mut out, contents.map(str::as_bytes)).unwrap();
    out
}

#[test]
fn a_missing_or_unreadable_report_says_so() {
    assert_eq!(section(None), "\n## Last Shutdown\n(none recorded)\n");
    for contents in ["", "{", "[1,2]", "\"text\""] {
        assert_eq!(
            section(Some(contents)),
            "\n## Last Shutdown\n(last shutdown report unreadable)\n",
            "{contents}"
        );
    }
}

#[test]
fn the_recorded_stages_follow_the_total_and_the_recording_time() {
    let contents = r#"{"version":1,"recorded_at_ms":1700000000123,"total_ms":42,"stages":[{"name":"terminal_released","step_ms":3,"total_ms":3},{"name":"complete","step_ms":39,"total_ms":42}]}"#;
    assert_eq!(
        section(Some(contents)),
        "\n## Last Shutdown\ntotal_ms=42 recorded=2023-11-14T22:13:20Z\n- terminal_released step_ms=3\n- complete step_ms=39\n"
    );
}

#[test]
fn missing_fields_fall_back_as_upstream_reports_them() {
    let contents = r#"{"recorded_at_ms":-5,"stages":[{"step_ms":"slow"},{"name":"sk-abcdefghijklmnop\nfake"}]}"#;
    assert_eq!(
        section(Some(contents)),
        "\n## Last Shutdown\ntotal_ms=-1\n- ? step_ms=-1\n- [redacted] fake step_ms=-1\n"
    );
    assert_eq!(
        section(Some(r#"{"total_ms":7,"stages":"none"}"#)),
        "\n## Last Shutdown\ntotal_ms=7\n"
    );
}

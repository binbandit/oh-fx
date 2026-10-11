use super::*;
use crate::app_session_runtime::{LastTitle, RunningTitle};

fn section(facts: &SessionTitleFacts) -> String {
    let mut out = String::new();
    write_section(&mut out, facts).unwrap();
    out
}

fn problems(facts: &SessionTitleFacts) -> (usize, String) {
    let mut out = String::new();
    let count = write_problems(&mut out, facts).unwrap();
    (count, out)
}

#[test]
fn the_section_reports_the_retained_generation_outcome() {
    let mut facts = SessionTitleFacts {
        setting: true,
        ..SessionTitleFacts::default()
    };
    assert_eq!(
        section(&facts),
        "\n## Session Title\nsetting: true\ntitle: (none)\ngeneration: status=none\n"
    );
    assert_eq!(problems(&facts), (0, String::new()));

    facts.generation.last = LastTitle {
        status: TitleStatus::Failed,
        reason: Some("transport_error"),
        detail: "ConnectionRefused".to_owned(),
        elapsed_ms: Some(15_001),
        session: "abc123sess".to_owned(),
        model: "openai/gpt-5.6-luna".to_owned(),
    };
    assert!(
        section(&facts).ends_with("generation: status=failed session=abc123sess model=openai/gpt-5.6-luna reason=transport_error detail=ConnectionRefused elapsed=15001ms\n"),
        "{}",
        section(&facts)
    );
    assert_eq!(
        problems(&facts),
        (
            1,
            "- session title generation failed reason=transport_error detail=ConnectionRefused\n"
                .to_owned()
        )
    );

    facts.generation.last.status = TitleStatus::Installed;
    facts.generation.last.reason = None;
    facts.generation.last.detail.clear();
    facts.title = Some("Fix renderer lag".to_owned());
    let installed = section(&facts);
    assert!(
        installed.contains("title: Fix renderer lag\n"),
        "{installed}"
    );
    assert!(
        installed.contains("generation: status=installed"),
        "{installed}"
    );
    assert_eq!(problems(&facts).0, 0);
}

#[test]
fn a_running_generation_names_its_session_model_and_elapsed_time() {
    let facts = SessionTitleFacts {
        setting: false,
        title: Some("token sk-live-0123456789abcdefghij".to_owned()),
        generation: TitleSnapshot {
            running: Some(RunningTitle {
                session: "s1".to_owned(),
                model: "m".to_owned(),
                elapsed_ms: 42,
            }),
            last: LastTitle::default(),
        },
    };
    let written = section(&facts);
    assert!(written.contains("setting: false\n"), "{written}");
    assert!(
        !written.contains("sk-live-0123456789abcdefghij"),
        "{written}"
    );
    assert!(
        written.ends_with("generation: status=running session=s1 model=m elapsed=42ms\n"),
        "{written}"
    );
}

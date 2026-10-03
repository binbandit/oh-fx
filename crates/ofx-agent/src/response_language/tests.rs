use super::*;

#[test]
fn response_language_evidence_distinguishes_clear_scripts() {
    assert_eq!(
        evidence("I will inspect the lockfile next.").script,
        Some(Script::Latin)
    );
    assert_eq!(
        evidence("我会先检查锁文件和依赖清单。").script,
        Some(Script::Han)
    );
    assert_eq!(
        evidence("Сначала я проверю файл блокировки.").script,
        Some(Script::Cyrillic)
    );
    assert_eq!(
        evidence("次にロックファイルを確認します。").script,
        Some(Script::Japanese)
    );
    assert_eq!(
        evidence(
            "接下来我将检查项目中的锁文件（如 package-lock.json、Cargo.lock、Pipfile.lock 等）及其对应的清单文件。"
        )
        .script,
        Some(Script::Han)
    );
    assert_eq!(
        evidence("我将检查 lockfile 和 dependency manifest，查找损坏问题。").script,
        Some(Script::Han)
    );
    assert_eq!(
        evidence(&"English transcript payload with sparse 界 markers. ".repeat(16)).script,
        Some(Script::Latin)
    );
}

#[test]
fn response_language_expectation_follows_the_current_human_unless_a_switch_may_be_explicit() {
    assert_eq!(
        infer_expectation("The lockfile is broken again."),
        Some(Script::Latin)
    );
    assert_eq!(infer_expectation("请再次检查锁文件。"), None);
    assert_eq!(
        infer_expectation("Answer in Japanese and keep it short."),
        None
    );
    assert_eq!(infer_expectation("Translate the error into Russian."), None);
    assert_eq!(
        infer_expectation("Why did you answer in Chinese? Reply in English."),
        None
    );
    assert_eq!(infer_expectation("Rispondi in giapponese."), None);
    assert_eq!(infer_expectation("Antworte auf Japanisch."), None);
    assert_eq!(infer_expectation("fix lockfile"), None);
}

#[test]
fn response_language_decision_is_pure_conservative_and_bounded() {
    let expected = infer_expectation("The lockfile is broken again.");
    let matching = evidence("I will inspect the lockfile next.");
    let mismatching = evidence("我会先检查锁文件和依赖清单。");
    let mixed = evidence("abcd锁文件坏");
    let input = |candidate| DecisionInput {
        expected,
        candidate,
        correction_attempted: false,
        has_tool_calls: false,
    };

    assert_eq!(decide(input(matching)), Decision::Accept);
    assert_eq!(decide(input(mismatching)), Decision::RetryOnce);
    assert_eq!(
        decide(DecisionInput {
            correction_attempted: true,
            ..input(mismatching)
        }),
        Decision::FailWithoutCommit
    );
    assert_eq!(
        decide(DecisionInput {
            has_tool_calls: true,
            ..input(mismatching)
        }),
        Decision::AcceptWithoutProse
    );
    assert_eq!(decide(input(mixed)), Decision::Undecidable);
    assert_eq!(
        decide(DecisionInput {
            expected: None,
            ..input(matching)
        }),
        Decision::Undecidable
    );
}

#[test]
fn response_language_evidence_needs_enough_letters() {
    assert_eq!(evidence("ok").script, None);
    assert_eq!(evidence("\u{fffd} broken").script, Some(Script::Latin));
}

#[test]
fn clear_evidence_needs_three_fifths_of_the_letters() {
    assert_eq!(evidence("abcd锁文件坏").script, None);
    assert_eq!(evidence("abc中文").script, Some(Script::Latin));
    assert_eq!(evidence("ab中文").script, None);
}

#[test]
fn a_clear_non_latin_minority_outranks_latin_prose() {
    assert_eq!(
        evidence("Please 我将检查锁文件和依赖清单查找问题 check this lockfile and the manifest file again today").script,
        Some(Script::Han)
    );
    assert_eq!(
        evidence("Please 我将检查锁 check this lockfile and the manifest file again today now")
            .script,
        Some(Script::Latin)
    );
}

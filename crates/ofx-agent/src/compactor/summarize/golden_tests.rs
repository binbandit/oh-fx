#[test]
fn summary_model_system_instruction_matches_upstream_golden() {
    assert_eq!(
        super::SYSTEM_PROMPT.as_bytes(),
        include_bytes!("../../../../../parity/goldens/compaction_system_prompt.txt")
    );
}

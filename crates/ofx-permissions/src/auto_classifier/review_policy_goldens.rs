#[test]
fn runtime_review_policy_matches_the_upstream_golden_byte_for_byte() {
    let golden = include_bytes!("../../../../parity/goldens/review-policy/review_policy.xml");
    assert_eq!(super::REVIEW_POLICY_TEMPLATE.as_bytes(), golden);
    assert_eq!(golden.len(), 3195);
    assert_eq!(
        super::REVIEW_POLICY_TEMPLATE
            .matches(super::REVIEW_DATA_MARKER)
            .count(),
        1
    );
    assert!(golden.ends_with(b"</permission_review>\n"));
}

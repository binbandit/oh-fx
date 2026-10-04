use crate::context::GATEWAY_SYSTEM_PROMPT;

fn renamed_prompt(source: &str) -> Result<String, &'static str> {
    let substitutions = [
        ("You are fx,", "You are oh-fx,"),
        ("For questions about fx,", "For questions about oh-fx,"),
        ("which fx renders", "which oh-fx renders"),
        ("since fx already bolds", "since oh-fx already bolds"),
    ];
    let mut output = source.to_owned();
    for (before, after) in substitutions {
        if output.matches(before).count() != 1 {
            return Err("unexpected branding substitution count");
        }
        output = output.replacen(before, after, 1);
    }
    Ok(output)
}

#[test]
fn shipped_system_prompt_matches_upstream_golden() {
    let source = include_str!("../../../parity/goldens/system_prompt.md");
    let expected = renamed_prompt(source).unwrap();
    assert_eq!(GATEWAY_SYSTEM_PROMPT.as_bytes(), expected.as_bytes());
    assert!(expected.contains("https://fx.sh/llms.txt"));
}

#[test]
fn branding_substitutions_reject_missing_or_duplicated_context() {
    let source = include_str!("../../../parity/goldens/system_prompt.md");
    assert!(renamed_prompt(&source.replace("You are fx,", "You are changed,")).is_err());
    assert!(renamed_prompt(&format!("{source}You are fx,")).is_err());
}

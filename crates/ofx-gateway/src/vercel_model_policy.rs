pub(crate) fn parallel_tool_calls(model: &str) -> Option<bool> {
    model.starts_with("xai/").then_some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_xai_models_ask_for_parallel_tool_calls() {
        assert_eq!(parallel_tool_calls("xai/grok-4"), Some(true));
        assert_eq!(parallel_tool_calls("spacexai/grok-4.7"), None);
        assert_eq!(parallel_tool_calls("openai/gpt-5.6-sol"), None);
    }
}

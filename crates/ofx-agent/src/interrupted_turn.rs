use ofx_contract::ToolCall;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterruptedTurn {
    pub in_flight: Option<ToolCall>,
    pub completed_tools: Vec<String>,
}

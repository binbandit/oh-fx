use ofx_contract::{ActiveMode, ToolOutput, ToolSet, ToolSpec};

pub(super) struct Offer {
    pub(super) specs: Vec<ToolSpec>,
    pub(super) guidance: String,
}

pub(super) fn offer(
    specs: &[ToolSpec],
    provider_executed: &[bool],
    mode: Option<&ActiveMode>,
) -> Offer {
    let (remote, offered): (Vec<_>, Vec<_>) = specs
        .iter()
        .zip(provider_executed)
        .filter(|(spec, _)| mode.is_none_or(|mode| allows(mode, specs, &spec.name)))
        .partition(|(_, remote)| **remote);
    Offer {
        specs: offered.into_iter().map(|(spec, _)| spec.clone()).collect(),
        guidance: remote
            .iter()
            .map(|(spec, _)| spec.description.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

pub(super) fn denial(mode: &ActiveMode, specs: &[ToolSpec], tool_name: &str) -> Option<ToolOutput> {
    mode.registry
        .tool_policy_denied_json(&tool_set(mode, specs), mode.id, tool_name)
        .map(ToolOutput::failure)
}

fn allows(mode: &ActiveMode, specs: &[ToolSpec], tool_name: &str) -> bool {
    mode.registry
        .tool_allowed(&tool_set(mode, specs), mode.id, tool_name)
}

fn tool_set<'a>(mode: &ActiveMode, specs: &'a [ToolSpec]) -> ToolSet<'a> {
    ToolSet {
        specs,
        read_only_tool_names: mode.read_only_tool_names,
    }
}

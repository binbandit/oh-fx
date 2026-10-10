use std::sync::Arc;

use ofx_contract::{DynamicTools, Tool, ToolOutput};

pub(super) struct DynamicToolSet {
    source: Arc<dyn DynamicTools>,
    generation: Option<u64>,
    published: Vec<Arc<dyn Tool>>,
    advertised: Vec<Arc<dyn Tool>>,
}

impl DynamicToolSet {
    pub(super) fn new(source: Arc<dyn DynamicTools>) -> Self {
        Self {
            source,
            generation: None,
            published: Vec::new(),
            advertised: Vec::new(),
        }
    }

    pub(super) fn forget_advertised(&mut self) {
        self.generation = None;
        self.advertised.clear();
    }

    pub(super) fn lists(&self, name: &str) -> bool {
        self.source.lists(name)
    }

    pub(super) fn advertised(&self) -> &[Arc<dyn Tool>] {
        &self.advertised
    }

    pub(super) fn advertise(&mut self, selected: &SelectedTools) -> bool {
        let generation = self.source.generation();
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.published = self.source.tools();
        }
        let advertised: Vec<Arc<dyn Tool>> = selected
            .0
            .iter()
            .filter_map(|name| {
                self.published
                    .iter()
                    .find(|tool| tool.spec().name == *name)
                    .cloned()
            })
            .collect();
        let unchanged = advertised.len() == self.advertised.len()
            && advertised
                .iter()
                .zip(&self.advertised)
                .all(|(left, right)| Arc::ptr_eq(left, right));
        self.advertised = advertised;
        !unchanged
    }
}

#[derive(Debug, Default)]
pub(super) struct SelectedTools(Vec<String>);

impl SelectedTools {
    pub(super) fn record(&mut self, output: &ToolOutput) {
        self.0.retain(|name| !output.retired_tools().contains(name));
        for name in output.selected_tools() {
            if !self.0.contains(name) {
                self.0.push(name.clone());
            }
        }
    }
}

pub(super) fn not_selected(name: &str) -> String {
    format!(
        "Dynamic MCP tool not selected for this model step: {name}. Use capability_search or mcp_select_tool to load its definition; the selected tool can be called on the next model step after its schema is advertised."
    )
}

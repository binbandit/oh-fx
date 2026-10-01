use std::path::PathBuf;

use ofx_contract::ApplicableTarget;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    pub content: Option<String>,
    pub delivered_sources: Vec<PathBuf>,
    pub evaluated_endpoints: Vec<PathBuf>,
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryState {
    pub delivered_sources: Vec<PathBuf>,
    pub evaluated_endpoints: Vec<PathBuf>,
}

impl DeliveryState {
    pub(crate) fn from_snapshot(snapshot: &ProjectContext) -> Self {
        Self {
            delivered_sources: snapshot.delivered_sources.clone(),
            evaluated_endpoints: snapshot.evaluated_endpoints.clone(),
        }
    }

    pub(crate) fn commit(&mut self, selected: &ProjectContext) {
        self.delivered_sources
            .extend(selected.delivered_sources.iter().cloned());
        self.evaluated_endpoints
            .extend(selected.evaluated_endpoints.iter().cloned());
    }
}

pub trait ProjectContextProvider: Send + Sync {
    fn select(&self, targets: &[ApplicableTarget], delivery: &DeliveryState) -> ProjectContext;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_state_starts_from_the_snapshot_and_appends_commits() {
        let snapshot = ProjectContext {
            content: Some("rules".to_owned()),
            delivered_sources: vec![PathBuf::from("/work/AGENTS.md")],
            evaluated_endpoints: vec![PathBuf::from("/work")],
            notices: vec!["notice".to_owned()],
        };
        let mut state = DeliveryState::from_snapshot(&snapshot);
        state.commit(&ProjectContext {
            delivered_sources: vec![PathBuf::from("/work/src/AGENTS.md")],
            evaluated_endpoints: vec![PathBuf::from("/work/src")],
            ..ProjectContext::default()
        });
        assert_eq!(
            state,
            DeliveryState {
                delivered_sources: vec![
                    PathBuf::from("/work/AGENTS.md"),
                    PathBuf::from("/work/src/AGENTS.md")
                ],
                evaluated_endpoints: vec![PathBuf::from("/work"), PathBuf::from("/work/src")],
            }
        );
    }
}

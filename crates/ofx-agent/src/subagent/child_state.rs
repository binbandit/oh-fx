use std::sync::Arc;

use ofx_contract::{
    ChildPhase, ModelFailureDiagnostic, PermissionMode, RootUserRequests, valid_agent_name,
    valid_instructions,
};

const MAX_CHILDREN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    OneOff,
    Persistent { agent: String, instructions: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryError {
    CapacityExceeded,
    ChildAlreadyExists,
    AgentAlreadyExists,
    InvalidState,
    ChildNotFound,
    ChildBusy,
    StaleWork,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveWork {
    pub(crate) id: String,
    pub(crate) request_fingerprint: [u8; 32],
    pub(crate) message: String,
    pub(crate) root_user_requests: Arc<RootUserRequests>,
    pub(crate) root_user_context: String,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Child {
    pub(crate) id: String,
    pub(crate) kind: Kind,
    pub(crate) phase: ChildPhase,
    pub(crate) work_generation: u64,
    pub(crate) active: Option<ActiveWork>,
    pub(crate) last_work_id: Option<String>,
    pub(crate) last_request_fingerprint: Option<[u8; 32]>,
    pub(crate) last_outcome: Option<Outcome>,
    pub(crate) last_failure: Option<ModelFailureDiagnostic>,
}

impl Child {
    pub(crate) fn agent_name(&self) -> Option<&str> {
        match &self.kind {
            Kind::OneOff => None,
            Kind::Persistent { agent, .. } => Some(agent),
        }
    }

    pub(crate) fn instructions(&self) -> &str {
        match &self.kind {
            Kind::OneOff => "",
            Kind::Persistent { instructions, .. } => instructions,
        }
    }

    pub(crate) fn operation_fingerprint(&self, operation_id: &str) -> Option<[u8; 32]> {
        if let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.id == operation_id)
        {
            return Some(active.request_fingerprint);
        }
        if self.last_work_id.as_deref() == Some(operation_id) {
            return self.last_request_fingerprint;
        }
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Registry {
    generation: u64,
    children: Vec<Child>,
}

impl Registry {
    pub(crate) fn render(&self, parent_id: &str) -> Vec<u8> {
        registry_file::render(self, parent_id)
    }

    pub(crate) fn find_by_id(&self, child_id: &str) -> Option<&Child> {
        self.children.iter().find(|child| child.id == child_id)
    }

    pub(crate) fn find_persistent(&self, agent: &str) -> Option<&Child> {
        self.children
            .iter()
            .find(|child| child.agent_name() == Some(agent))
    }

    pub(crate) fn find_by_operation(&self, operation_id: &str) -> Option<&Child> {
        self.children.iter().find(|child| {
            child
                .active
                .as_ref()
                .is_some_and(|active| active.id == operation_id)
                || child.last_work_id.as_deref() == Some(operation_id)
        })
    }

    pub(crate) fn append_one_off(
        &mut self,
        child_id: &str,
        active: ActiveWork,
    ) -> Result<(), RegistryError> {
        self.append_child(child_id, Kind::OneOff, active)
    }

    pub(crate) fn append_persistent(
        &mut self,
        child_id: &str,
        agent: &str,
        instructions: &str,
        active: ActiveWork,
    ) -> Result<(), RegistryError> {
        if !valid_agent_name(agent) || !valid_instructions(instructions) {
            return Err(RegistryError::InvalidState);
        }
        if self.find_persistent(agent).is_some() {
            return Err(RegistryError::AgentAlreadyExists);
        }
        let kind = Kind::Persistent {
            agent: agent.to_owned(),
            instructions: instructions.to_owned(),
        };
        self.append_child(child_id, kind, active)
    }

    fn append_child(
        &mut self,
        child_id: &str,
        kind: Kind,
        active: ActiveWork,
    ) -> Result<(), RegistryError> {
        if self.children.len() >= MAX_CHILDREN {
            return Err(RegistryError::CapacityExceeded);
        }
        if self.find_by_id(child_id).is_some() {
            return Err(RegistryError::ChildAlreadyExists);
        }
        self.children.push(Child {
            id: child_id.to_owned(),
            kind,
            phase: ChildPhase::Running,
            work_generation: 1,
            active: Some(active),
            last_work_id: None,
            last_request_fingerprint: None,
            last_outcome: None,
            last_failure: None,
        });
        self.advance();
        Ok(())
    }

    fn advance(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    pub(crate) fn start_persistent_work(
        &mut self,
        agent: &str,
        instructions: Option<&str>,
        active: ActiveWork,
    ) -> Result<&Child, RegistryError> {
        if instructions.is_some_and(|value| value.is_empty() || !valid_instructions(value)) {
            return Err(RegistryError::InvalidState);
        }
        let index = self
            .children
            .iter()
            .position(|child| child.agent_name() == Some(agent))
            .ok_or(RegistryError::ChildNotFound)?;
        let generation = self.generation.saturating_add(1);
        let child = &mut self.children[index];
        match child.phase {
            ChildPhase::Idle | ChildPhase::Interrupted => {}
            ChildPhase::Running | ChildPhase::AwaitingApproval => {
                return Err(RegistryError::ChildBusy);
            }
            ChildPhase::Finished => return Err(RegistryError::ChildNotFound),
        }
        if let (Some(next), Kind::Persistent { instructions, .. }) = (instructions, &mut child.kind)
        {
            next.clone_into(instructions);
        }
        child.active = Some(active);
        child.phase = ChildPhase::Running;
        child.work_generation = child.work_generation.saturating_add(1);
        self.generation = generation;
        Ok(&self.children[index])
    }

    pub(crate) fn finish(
        &mut self,
        child_id: &str,
        work_id: &str,
        outcome: Outcome,
        failure: Option<ModelFailureDiagnostic>,
    ) -> Result<&Child, RegistryError> {
        let index = self
            .children
            .iter()
            .position(|child| child.id == child_id)
            .ok_or(RegistryError::ChildNotFound)?;
        let child = &mut self.children[index];
        if child
            .active
            .as_ref()
            .is_none_or(|active| active.id != work_id)
        {
            return Err(RegistryError::StaleWork);
        }
        if failure.is_some() && outcome != Outcome::Failed {
            return Err(RegistryError::InvalidState);
        }
        let Some(active) = child.active.take() else {
            return Err(RegistryError::StaleWork);
        };
        child.last_work_id = Some(active.id);
        child.last_request_fingerprint = Some(active.request_fingerprint);
        child.last_outcome = Some(outcome);
        child.last_failure = failure;
        child.phase = match child.kind {
            Kind::OneOff => ChildPhase::Finished,
            Kind::Persistent { .. } => ChildPhase::Idle,
        };
        self.advance();
        Ok(&self.children[index])
    }
}

mod registry_file;
#[cfg(test)]
mod tests;

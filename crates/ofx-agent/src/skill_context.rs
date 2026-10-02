use ofx_contract::{BoxFuture, Notice};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillContext {
    pub catalog: String,
    pub explicit: String,
    pub context_notices: Vec<String>,
    pub load_notice: Option<Notice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillContextFailure {
    Cancelled,
    Failed {
        code: String,
        context_notices: Vec<String>,
    },
}

pub trait SkillContextProvider: Send + Sync {
    fn uses_context_window(&self) -> bool;

    fn prepare<'a>(
        &'a self,
        prompt: &'a str,
        context_window: Option<u32>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<SkillContext, SkillContextFailure>>;
}

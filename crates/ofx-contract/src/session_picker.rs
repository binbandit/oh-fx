#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    CurrentWorkspace,
    AllWorkspaces,
}

impl SessionScope {
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::CurrentWorkspace => Self::AllWorkspaces,
            Self::AllWorkspaces => Self::CurrentWorkspace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub title: Option<String>,
    pub workspace_root: String,
    pub updated_at_ms: i64,
    pub turns: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCursor {
    pub updated_at_ms: i64,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPage {
    pub scope: SessionScope,
    pub after: Option<SessionCursor>,
    pub rows: Vec<SessionRow>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRefusal {
    OpenElsewhere,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_toggle_between_this_workspace_and_every_workspace() {
        assert_eq!(
            SessionScope::CurrentWorkspace.toggled(),
            SessionScope::AllWorkspaces
        );
        assert_eq!(
            SessionScope::AllWorkspaces.toggled(),
            SessionScope::CurrentWorkspace
        );
    }
}

use std::error::Error;
use std::fmt;

use crate::ids::TurnId;
use crate::types::TurnPresentationOutcome;

pub(super) const HANDLER_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookScope {
    Interactive,
    Ask,
    Acp,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookInvocation {
    pub scope: HookScope,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostTurnEndInput {
    pub invocation: HookInvocation,
    pub outcome: TurnPresentationOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    Permission,
    Question,
    RouteRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionRequiredInput {
    pub invocation: HookInvocation,
    pub kind: AttentionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRegistrationError {
    EmptyHandlerName,
    HandlerNameTooLong,
    InvalidHandlerName,
    DuplicateHandlerName,
}

impl fmt::Display for HookRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyHandlerName => "EmptyHandlerName",
            Self::HandlerNameTooLong => "HandlerNameTooLong",
            Self::InvalidHandlerName => "InvalidHandlerName",
            Self::DuplicateHandlerName => "DuplicateHandlerName",
        })
    }
}

impl Error for HookRegistrationError {}

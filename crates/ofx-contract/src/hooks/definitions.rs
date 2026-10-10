use std::error::Error;
use std::fmt;

use crate::ids::TurnId;
use crate::types::TurnPresentationOutcome;

pub(super) const HANDLER_NAME_BYTES: usize = 128;
pub(super) const REASON_BYTES: usize = 4 * 1024;
pub(super) const ARGUMENTS_JSON_BYTES: usize = 1024 * 1024;

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
pub enum HookHandlerError {
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDispatchError {
    HandlerFailed,
    Cancelled,
    InvalidHandlerOutput,
    HandlerOutputTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreToolUseInput<'a> {
    pub invocation: HookInvocation,
    pub step_index: usize,
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub arguments_json: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolUseAction {
    Continue,
    RewriteArguments(String),
    Block(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolUseOutcome {
    Unchanged,
    Rewritten(String),
    Blocked(String),
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

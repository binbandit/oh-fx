use std::path::{Path, PathBuf};

use ofx_contract::{
    CallDescription, CallPresentation, Concurrency, FileMutation, FileMutationState, PathAccess,
    PreparedCall, ToolContext, ToolEffect, ToolOutput, format_plain_action,
    format_tool_execution_error_json,
};
use ofx_permissions::{FileMutationKind, FileMutationTargets, prepare_file_mutation_targets};
use ofx_workspace::{TargetMode, resolve_file_mutation_target};

use crate::file_mutation::{MutationInput, PrepareFailure, PreparedMutation};
use crate::tool_admission::file_target_failure;
use crate::tool_runtime::BlockingCall;

const TARGET_MISMATCH: &str =
    "file mutation preparation failed: approved target no longer matches the call";

pub(crate) struct MutationRequest {
    pub(crate) tool_name: &'static str,
    pub(crate) presentation: CallPresentation,
    pub(crate) workspace_root: PathBuf,
}

impl MutationRequest {
    pub(crate) fn prepare(
        &self,
        arguments: &str,
        decoded: Result<(String, MutationInput), ToolOutput>,
    ) -> Box<dyn PreparedCall> {
        let description = |effect| CallDescription {
            title: format_plain_action(self.tool_name, &self.presentation, arguments),
            activity: self.presentation.activity,
            effect,
            concurrency: Concurrency::Serial,
        };
        match decoded.and_then(|(path, input)| self.plan(path, input)) {
            Ok(plan) => {
                let mutation = plan.file_mutation();
                BlockingCall::mutation(
                    description(ToolEffect::Irreversible),
                    mutation,
                    move |context| plan.execute(&context),
                )
            }
            Err(failure) => BlockingCall::boxed(description(ToolEffect::None), move |_| failure),
        }
    }

    fn plan(&self, requested_path: String, input: MutationInput) -> Result<Plan, ToolOutput> {
        let targets =
            prepare_file_mutation_targets(&self.workspace_root, &requested_path, input.kind())
                .map_err(|failure| {
                    ToolOutput::failure(file_target_failure(self.tool_name, failure))
                })?;
        let stage = if targets.target.anchor_is_external {
            Stage::Deferred(targets)
        } else {
            Stage::Prepared(
                PreparedMutation::prepare(targets, &requested_path, &input)
                    .map_err(|failure| prepare_failure(self.tool_name, failure))?,
            )
        };
        Ok(Plan {
            tool_name: self.tool_name,
            workspace_root: self.workspace_root.clone(),
            requested_path,
            input,
            stage,
        })
    }
}

enum Stage {
    Prepared(PreparedMutation),
    Deferred(FileMutationTargets),
}

struct Plan {
    tool_name: &'static str,
    workspace_root: PathBuf,
    requested_path: String,
    input: MutationInput,
    stage: Stage,
}

impl Plan {
    fn file_mutation(&self) -> FileMutation {
        let (target, state) = match &self.stage {
            Stage::Prepared(prepared) => {
                let state = if prepared.is_noop() {
                    FileMutationState::Unchanged
                } else if prepared.creates_file() {
                    FileMutationState::Creates
                } else {
                    FileMutationState::Changes
                };
                (prepared.targets(), state)
            }
            Stage::Deferred(targets) => {
                let state = if targets.target_identity.is_none() {
                    FileMutationState::Creates
                } else {
                    FileMutationState::Unread
                };
                (targets, state)
            }
        };
        FileMutation {
            target: target.target.path(),
            state,
        }
    }

    fn execute(self, context: &ToolContext) -> ToolOutput {
        let external = match &self.stage {
            Stage::Prepared(prepared) => prepared.targets().target.anchor_is_external,
            Stage::Deferred(targets) => targets.target.anchor_is_external,
        };
        if external && context.path_access == PathAccess::WorkspaceOnly {
            return ToolOutput::failure(
                "file mutation target resolution failed: path_outside_workspace",
            );
        }
        let prepared = match self.stage {
            Stage::Prepared(prepared) => prepared,
            Stage::Deferred(targets) => {
                if !target_still_matches(
                    &self.workspace_root,
                    &self.requested_path,
                    self.input.kind(),
                    &targets,
                ) {
                    return ToolOutput::failure(TARGET_MISMATCH);
                }
                match PreparedMutation::prepare(targets, &self.requested_path, &self.input) {
                    Ok(prepared) => prepared,
                    Err(failure) => return prepare_failure(self.tool_name, failure),
                }
            }
        };
        if prepared.is_noop() {
            return match prepared.confirm_noop() {
                Ok(()) => ToolOutput::success(prepared.noop_message()),
                Err(rejection) => ToolOutput::failure(rejection.message()),
            };
        }
        match prepared.apply(&context.cancellation) {
            Ok(()) => ToolOutput::success(prepared.success_message()),
            Err(rejection) => ToolOutput::failure(rejection.message()),
        }
    }
}

fn target_still_matches(
    workspace_root: &Path,
    requested_path: &str,
    kind: FileMutationKind,
    targets: &FileMutationTargets,
) -> bool {
    let mode = match kind {
        FileMutationKind::Write => TargetMode::Create,
        FileMutationKind::Edit => TargetMode::Existing,
    };
    resolve_file_mutation_target(workspace_root, requested_path, mode)
        .is_ok_and(|target| target == targets.target)
}

fn prepare_failure(tool_name: &str, failure: PrepareFailure) -> ToolOutput {
    ToolOutput::failure(match failure {
        PrepareFailure::Semantic(message) => message,
        PrepareFailure::Operational(error) => {
            format_tool_execution_error_json(tool_name, &error.to_string())
        }
    })
}

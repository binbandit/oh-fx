use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_contract::{
    ApplicableTarget, BoxFuture, CallDescription, CallPresentation, Concurrency, FileChange,
    FileMutation, FileMutationState, LivePermissionMode, PathAccess, PermissionMode, PreparedCall,
    TargetKind, ToolContext, ToolEffect, ToolOutput, format_tool_execution_error_json,
};
use ofx_permissions::{FileMutationKind, FileMutationTargets, prepare_file_mutation_targets};
use ofx_text::{encode_terminal_safe, encode_terminal_safe_path_tail};
use ofx_workspace::{TargetMode, resolve_file_mutation_target};

use crate::file_mutation::{
    MAX_ENCODED_PATH_BYTES, MutationInput, PrepareFailure, PreparedMutation,
};
use crate::tool_admission::file_target_failure;
use crate::tool_runtime::{BlockingCall, run_blocking};

const TARGET_MISMATCH: &str =
    "file mutation preparation failed: approved target no longer matches the call";

pub(crate) struct MutationRequest {
    pub(crate) tool_name: &'static str,
    pub(crate) presentation: CallPresentation,
    pub(crate) workspace_root: PathBuf,
    pub(crate) permission_mode: Option<LivePermissionMode>,
}

impl MutationRequest {
    pub(crate) fn prepare(
        &self,
        decoded: Result<(String, MutationInput), ToolOutput>,
    ) -> Box<dyn PreparedCall> {
        match decoded.and_then(|(path, input)| self.plan(path, input)) {
            Ok(plan) => {
                let mutation = plan.file_mutation();
                Box::new(MutationCall {
                    presentation: self.presentation,
                    target: Some(file_target(mutation.target.clone())),
                    mutation: Some(mutation),
                    plan: Ok(plan),
                })
            }
            Err(failure) => BlockingCall::boxed(
                description(&self.presentation, None, ToolEffect::None),
                move |_| failure,
            ),
        }
    }

    fn plan(&self, requested_path: String, input: MutationInput) -> Result<Plan, ToolOutput> {
        let targets = resolve_targets(
            self.tool_name,
            &self.workspace_root,
            &requested_path,
            input.kind(),
        )?;
        Ok(Plan {
            tool_name: self.tool_name,
            workspace_root: self.workspace_root.clone(),
            requested_path,
            full_access: false,
            permission_mode: self.permission_mode.clone(),
            input,
            stage: Stage::Deferred(targets),
        })
    }
}

struct MutationCall {
    presentation: CallPresentation,
    plan: Result<Plan, ToolOutput>,
    target: Option<ApplicableTarget>,
    mutation: Option<FileMutation>,
}

impl PreparedCall for MutationCall {
    fn describe(&self) -> CallDescription {
        match &self.plan {
            Ok(plan) => description(&self.presentation, plan.label(), ToolEffect::Irreversible),
            Err(_) => description(&self.presentation, None, ToolEffect::None),
        }
    }

    fn untargeted_title(&self) -> String {
        self.presentation.untargeted_title()
    }

    fn complete(&mut self) {
        let placeholder = Err(ToolOutput::failure(String::new()));
        match mem::replace(&mut self.plan, placeholder) {
            Ok(plan) => (self.target, self.plan) = plan.complete(),
            failed => self.plan = failed,
        }
        self.mutation = self.plan.as_ref().ok().map(Plan::file_mutation);
    }

    fn applicable_target(&self) -> Option<ApplicableTarget> {
        self.target.clone()
    }

    fn file_mutation(&self) -> Option<&FileMutation> {
        self.mutation.as_ref()
    }

    fn file_change(&self) -> Option<FileChange<'_>> {
        match &self.plan {
            Ok(Plan {
                stage: Stage::Prepared(prepared),
                ..
            }) => Some(prepared.file_change()),
            _ => None,
        }
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        run_blocking(move || match self.plan {
            Ok(plan) => plan.execute(&context),
            Err(failure) => failure,
        })
    }
}

fn description(
    presentation: &CallPresentation,
    label: Option<String>,
    effect: ToolEffect,
) -> CallDescription {
    CallDescription {
        title: label.map_or_else(
            || presentation.untargeted_title(),
            |label| format!("{} {label}", presentation.action_label),
        ),
        activity: presentation.activity,
        effect,
        concurrency: Concurrency::Serial,
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
    full_access: bool,
    permission_mode: Option<LivePermissionMode>,
    input: MutationInput,
    stage: Stage,
}

impl Plan {
    fn complete(self) -> (Option<ApplicableTarget>, Result<Self, ToolOutput>) {
        let targets = match resolve_targets(
            self.tool_name,
            &self.workspace_root,
            &self.requested_path,
            self.input.kind(),
        ) {
            Ok(targets) => targets,
            Err(failure) => return (None, Err(failure)),
        };
        let target = file_target(targets.target.path());
        let full_access = self
            .permission_mode
            .as_ref()
            .is_some_and(|mode| mode.get() == PermissionMode::Yolo);
        let stage = if targets.target.anchor_is_external || full_access {
            Ok(Stage::Deferred(targets))
        } else {
            PreparedMutation::prepare(targets, &self.requested_path, &self.input)
                .map(Stage::Prepared)
                .map_err(|failure| prepare_failure(self.tool_name, failure))
        };
        (
            Some(target),
            stage.map(|stage| Self {
                full_access,
                stage,
                ..self
            }),
        )
    }

    fn label(&self) -> Option<String> {
        if self.full_access {
            return Some(
                encode_terminal_safe(self.requested_path.as_bytes(), MAX_ENCODED_PATH_BYTES).text,
            );
        }
        match &self.stage {
            Stage::Prepared(prepared) => Some(prepared.display_path().to_owned()),
            Stage::Deferred(targets) => encode_terminal_safe_path_tail(
                targets.target.path().as_os_str().as_bytes(),
                MAX_ENCODED_PATH_BYTES,
            ),
        }
    }

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
            Ok(committed) => ToolOutput::success(committed.annotate(prepared.success_message())),
            Err(rejection) => ToolOutput::failure(rejection.message()),
        }
    }
}

fn file_target(path: PathBuf) -> ApplicableTarget {
    ApplicableTarget {
        path,
        kind: TargetKind::File,
    }
}

fn resolve_targets(
    tool_name: &str,
    workspace_root: &Path,
    requested_path: &str,
    kind: FileMutationKind,
) -> Result<FileMutationTargets, ToolOutput> {
    prepare_file_mutation_targets(workspace_root, requested_path, kind)
        .map_err(|failure| ToolOutput::failure(file_target_failure(tool_name, failure)))
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

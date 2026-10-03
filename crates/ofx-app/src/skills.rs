use std::ffi::OsStr;
use std::fs;
use std::future;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use ofx_agent::{SkillContext, SkillContextFailure, SkillContextProvider};
use ofx_config::{ContextLimitName, ContextLimitSource, ContextLimits, ProfilePaths, Settings};
use ofx_contract::{BoxFuture, Notice, NoticeTone, SkillBinding};
use ofx_skills::{
    ExplicitBinding, LoadNotice, RootPolicy, RootSpec, SkillCatalog, SkillDiscovery,
    SkillDiscoveryContext, SkillError, SkillInventory, SkillLoader, SkillSource,
    SymlinkAuthorities, build_skill_prompt,
};
use ofx_tools::SkillTool;
use tokio_util::sync::CancellationToken;

const MANAGED_DIRECTORY: &str = "skills";

const WORKSPACE_ROOTS: [RootSpec; 7] = [
    RootSpec {
        source: SkillSource::WorkspaceOhFx,
        path: ".oh-fx/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceShared,
        path: "skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceOpencode,
        path: ".opencode/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceCodex,
        path: ".codex/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceClaude,
        path: ".claude/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceAgents,
        path: ".agents/skills",
    },
    RootSpec {
        source: SkillSource::WorkspaceClaw,
        path: ".claw/skills",
    },
];

const GLOBAL_ROOTS: [RootSpec; 5] = [
    RootSpec {
        source: SkillSource::GlobalOpencode,
        path: ".config/opencode/skills",
    },
    RootSpec {
        source: SkillSource::GlobalCodex,
        path: ".codex/skills",
    },
    RootSpec {
        source: SkillSource::GlobalClaude,
        path: ".claude/skills",
    },
    RootSpec {
        source: SkillSource::GlobalAgents,
        path: ".agents/skills",
    },
    RootSpec {
        source: SkillSource::GlobalClaw,
        path: ".claw/skills",
    },
];

pub(crate) const ROOT_POLICY: RootPolicy = RootPolicy {
    workspace_roots: &WORKSPACE_ROOTS,
    managed_root_source: Some(SkillSource::GlobalOhFx),
    global_roots: &GLOBAL_ROOTS,
};

const NO_ROOTS: RootPolicy = RootPolicy {
    workspace_roots: &[],
    managed_root_source: None,
    global_roots: &[],
};

pub(crate) struct HostSkills {
    shared: Arc<Shared>,
}

struct Shared {
    discovery: SkillDiscoveryContext,
    policy: RootPolicy,
    limits: ContextLimits,
    tool: Arc<SkillTool>,
    current: Mutex<Arc<SkillDiscovery>>,
    installations: Arc<Installations>,
}

impl HostSkills {
    pub(crate) fn load(
        workspace_root: &Path,
        home: Option<&OsStr>,
        paths: Option<&ProfilePaths>,
        settings: &Settings,
        limits: &ContextLimits,
    ) -> Self {
        let home = home.map(|home| canonical(Path::new(home)));
        let managed_root = paths.map(|paths| canonical(&paths.config).join(MANAGED_DIRECTORY));
        let policy = match (&home, &managed_root) {
            (Some(_), Some(_)) => ROOT_POLICY,
            _ => NO_ROOTS,
        };
        let discovery = SkillDiscoveryContext {
            workspace_root: Some(canonical(workspace_root)),
            home,
            managed_root: managed_root.unwrap_or_default(),
            symlink_authorities: SymlinkAuthorities::from_environment(
                settings.skill_symlink_authorities(),
            ),
        };
        let found = discovery.load_visible_skills(&policy);
        let tool = Arc::new(SkillTool::new(discovery.clone(), policy, *limits));
        Self {
            shared: Arc::new(Shared {
                discovery,
                policy,
                limits: *limits,
                tool,
                current: Mutex::new(Arc::new(found)),
                installations: Arc::new(Installations::default()),
            }),
        }
    }

    pub(crate) fn installations(&self) -> Arc<Installations> {
        Arc::clone(&self.shared.installations)
    }

    pub(crate) fn tool(&self) -> Arc<SkillTool> {
        Arc::clone(&self.shared.tool)
    }

    pub(crate) fn current(&self) -> Arc<SkillDiscovery> {
        self.shared.snapshot()
    }

    pub(crate) fn managed_root(&self) -> &Path {
        &self.shared.discovery.managed_root
    }

    pub(crate) fn refresh(&self) {
        let found = self
            .shared
            .discovery
            .load_visible_skills(&self.shared.policy);
        *self.shared.lock() = Arc::new(found);
    }
}

#[derive(Default)]
pub(crate) struct Installations {
    pending: Mutex<usize>,
    drained: Condvar,
}

pub(crate) struct SkillInstall {
    installations: Arc<Installations>,
}

impl Installations {
    pub(crate) fn start(self: &Arc<Self>) -> SkillInstall {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        SkillInstall {
            installations: Arc::clone(self),
        }
    }

    pub(crate) fn wait_for_running(&self, grace: Duration) -> bool {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if *pending == 0 {
            return false;
        }
        drop(
            self.drained
                .wait_timeout_while(pending, grace, |pending| *pending > 0)
                .unwrap_or_else(PoisonError::into_inner),
        );
        true
    }
}

impl Drop for SkillInstall {
    fn drop(&mut self) {
        let mut pending = self
            .installations
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *pending -= 1;
        if *pending == 0 {
            self.installations.drained.notify_all();
        }
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Arc<SkillDiscovery>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn snapshot(&self) -> Arc<SkillDiscovery> {
        Arc::clone(&self.lock())
    }

    fn prepare(
        &self,
        prompt: &str,
        bindings: &[SkillBinding],
        context_window: Option<u32>,
        cancel: &CancellationToken,
    ) -> Result<SkillContext, SkillContextFailure> {
        let found = self.snapshot();
        let rendered = renders(&found);
        if !rendered && bindings.is_empty() {
            self.tool.advertise(ofx_skills::Locations::default());
            return Ok(SkillContext::default());
        }
        let catalog = if rendered {
            build_skill_prompt(
                &found.skills,
                &found.diagnostics,
                &self.limits,
                context_window,
            )
        } else {
            SkillCatalog::default()
        };
        self.tool.advertise(catalog.locations);
        let bindings: Vec<ExplicitBinding<'_>> = bindings
            .iter()
            .map(|binding| ExplicitBinding {
                name: &binding.name,
                path: &binding.path,
            })
            .collect();
        let explicit = SkillLoader::new(
            SkillInventory {
                skills: &found.skills,
                diagnostics: &found.diagnostics,
            },
            &self.discovery.symlink_authorities,
            &self.limits,
        )
        .with_cancellation(cancel)
        .build_explicit_prompt_section(prompt, &bindings);
        let explicit = match explicit {
            Ok(explicit) => explicit,
            Err(SkillError::Cancelled) if cancel.is_cancelled() => {
                return Err(SkillContextFailure::Cancelled);
            }
            Err(error) => {
                return Err(SkillContextFailure::Failed {
                    code: error.to_string(),
                    context_notices: [catalog.notice, catalog.diagnostic_notice]
                        .into_iter()
                        .flatten()
                        .collect(),
                });
            }
        };
        let context_notices = [
            catalog.notice,
            catalog.diagnostic_notice,
            explicit.notice,
            explicit.diagnostic_notice,
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(SkillContext {
            catalog: catalog.text,
            explicit: explicit.text,
            context_notices,
            load_notice: explicit.load_notice.map(load_notice),
        })
    }
}

impl SkillContextProvider for HostSkills {
    fn uses_context_window(&self) -> bool {
        renders(&self.shared.snapshot())
            && self
                .shared
                .limits
                .get(ContextLimitName::SkillCatalogBytes)
                .source
                == ContextLimitSource::CompiledDefault
    }

    fn prepare<'a>(
        &'a self,
        prompt: &'a str,
        bindings: &'a [SkillBinding],
        context_window: Option<u32>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<SkillContext, SkillContextFailure>> {
        if bindings.is_empty() && !renders(&self.shared.snapshot()) {
            return Box::pin(future::ready(self.shared.prepare(
                prompt,
                bindings,
                context_window,
                cancel,
            )));
        }
        let shared = Arc::clone(&self.shared);
        let prompt = prompt.to_owned();
        let bindings = bindings.to_vec();
        let cancel = cancel.clone();
        Box::pin(async move {
            let prepared = tokio::task::spawn_blocking(move || {
                shared.prepare(&prompt, &bindings, context_window, &cancel)
            })
            .await;
            match prepared {
                Ok(prepared) => prepared,
                Err(error) => match error.try_into_panic() {
                    Ok(payload) => panic::resume_unwind(payload),
                    Err(_) => Err(SkillContextFailure::Cancelled),
                },
            }
        })
    }
}

fn renders(found: &SkillDiscovery) -> bool {
    !found.skills.is_empty() || !found.diagnostics.is_empty()
}

fn load_notice(notice: LoadNotice) -> Notice {
    let tone = match notice.tone {
        ofx_skills::NoticeTone::Neutral => NoticeTone::Neutral,
        ofx_skills::NoticeTone::Warning => NoticeTone::Warning,
    };
    Notice::new(tone, "", notice.body)
}

#[cfg(test)]
pub(crate) fn rootless_skill_tool() -> Arc<SkillTool> {
    let discovery = SkillDiscoveryContext {
        workspace_root: None,
        home: None,
        managed_root: PathBuf::new(),
        symlink_authorities: SymlinkAuthorities::default(),
    };
    Arc::new(SkillTool::new(
        discovery,
        NO_ROOTS,
        ContextLimits::default(),
    ))
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests;

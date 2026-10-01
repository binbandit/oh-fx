mod omissions;
mod rule_file;

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt::Write;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_config::{ContextLimit, ContextLimitName, ContextLimits};
use ofx_workspace::{basename, dirname, path_inside};

use omissions::{OmissionReason, Omissions, separate, write_path};
use rule_file::{RuleLoad, load_rule};

const RULE_FILE_NAME: &str = "AGENTS.md";
const SCOPED_RULES: usize = 32;
const GUIDANCE: &str = "Direct user instructions take precedence over project instructions. When project instructions conflict, follow the narrowest applicable project scope.";
const FILE_LIMIT_OVERRIDE: &str = "--context-limit project_instruction_file_bytes=BYTES|off";
const TOTAL_LIMIT_OVERRIDE: &str = "--context-limit project_instructions_total_bytes=BYTES|off";

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProjectContext {
    pub(crate) content: Option<String>,
    pub(crate) notices: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InstructionLimits {
    file: ContextLimit,
    total: ContextLimit,
}

impl InstructionLimits {
    pub(crate) fn from_limits(limits: &ContextLimits) -> Self {
        Self {
            file: limits.get(ContextLimitName::ProjectInstructionFileBytes),
            total: limits.get(ContextLimitName::ProjectInstructionsTotalBytes),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProfileLocation<'a> {
    pub(crate) home: Option<&'a OsStr>,
    pub(crate) config_directory: Option<&'a Path>,
}

pub(crate) fn gather_project_context(
    workspace_root: &Path,
    profile: ProfileLocation<'_>,
    limits: InstructionLimits,
) -> ProjectContext {
    let mut selection = Selection::new(workspace_root, limits);
    selection.add_ranking_endpoint(workspace_root);
    let mut global = None;
    let mut global_source = None;
    match profile.home {
        None => selection.omit(Path::new("HOME"), OmissionReason::HomeUnavailable),
        Some(home) => match fs::canonicalize(home) {
            Err(_) => selection.omit(Path::new(home), OmissionReason::HomeUnavailable),
            Ok(home_root) => {
                if let Some(directory) = profile.config_directory {
                    let source = global_rule_source(directory);
                    global = selection.load_direct(&source);
                    global_source = Some(source);
                }
                if path_inside(&home_root, workspace_root) {
                    selection.collect_launch_ancestors(&home_root);
                } else {
                    selection.omit(workspace_root, OmissionReason::HomeOutsideWorkspace);
                }
            }
        },
    }
    let project = if workspace_root.is_absolute() {
        let source = workspace_root.join(RULE_FILE_NAME);
        if global_source.as_ref() == Some(&source) {
            None
        } else {
            selection.load_direct(&source)
        }
    } else {
        selection.omit(workspace_root, OmissionReason::UnsafeTarget);
        None
    };
    selection.finish(global.as_ref(), project.as_ref())
}

fn global_rule_source(config_directory: &Path) -> PathBuf {
    fs::canonicalize(config_directory)
        .or_else(|_| std::path::absolute(config_directory))
        .unwrap_or_else(|_| config_directory.to_path_buf())
        .join(RULE_FILE_NAME)
}

struct Candidate {
    source: PathBuf,
    scope: PathBuf,
}

struct UsableRule {
    candidate: Candidate,
    body: String,
    observed_bytes: usize,
    distance: usize,
    depth: usize,
}

struct LoadedRule {
    source: PathBuf,
    body: String,
    observed_bytes: usize,
}

struct RenderedRule {
    source: PathBuf,
    start: usize,
    end: usize,
    is_global: bool,
}

struct Selection<'a> {
    workspace_root: &'a Path,
    file_limit: ContextLimit,
    total_limit: ContextLimit,
    candidates: Vec<Candidate>,
    ranking_endpoints: Vec<PathBuf>,
    delivered_sources: Vec<PathBuf>,
    omissions: Omissions,
    notices: Vec<String>,
}

impl<'a> Selection<'a> {
    fn new(workspace_root: &'a Path, limits: InstructionLimits) -> Self {
        Self {
            workspace_root,
            file_limit: limits.file,
            total_limit: limits.total,
            candidates: Vec::new(),
            ranking_endpoints: Vec::new(),
            delivered_sources: Vec::new(),
            omissions: Omissions::default(),
            notices: Vec::new(),
        }
    }

    fn omit(&mut self, source: &Path, reason: OmissionReason) {
        self.omissions.add(source, reason, &mut self.notices);
    }

    fn add_delivered(&mut self, source: &Path) {
        push_unique(&mut self.delivered_sources, source);
    }

    fn add_ranking_endpoint(&mut self, endpoint: &Path) {
        push_unique(&mut self.ranking_endpoints, endpoint);
    }

    fn load_direct(&mut self, source: &Path) -> Option<LoadedRule> {
        match load_rule(source, self.file_limit.effective_bytes()) {
            RuleLoad::Body(body) => {
                self.add_delivered(source);
                Some(LoadedRule {
                    source: source.to_path_buf(),
                    body: body.text,
                    observed_bytes: body.observed_bytes,
                })
            }
            RuleLoad::Missing | RuleLoad::Blank => None,
            RuleLoad::Omitted(reason) => {
                self.omit(source, reason);
                None
            }
        }
    }

    fn collect_launch_ancestors(&mut self, home: &Path) {
        let mut current = parent_directory(self.workspace_root);
        while let Some(scope) = current {
            if scope == home || !path_inside(home, scope) {
                break;
            }
            self.append_candidate(scope);
            current = parent_directory(scope);
        }
    }

    fn append_candidate(&mut self, scope: &Path) {
        let source = scope.join(RULE_FILE_NAME);
        if self.delivered_sources.contains(&source)
            || self
                .candidates
                .iter()
                .any(|candidate| candidate.source == source)
        {
            return;
        }
        self.candidates.push(Candidate {
            source,
            scope: scope.to_path_buf(),
        });
    }

    fn finish(
        mut self,
        global: Option<&LoadedRule>,
        project: Option<&LoadedRule>,
    ) -> ProjectContext {
        let selected = self.select_candidates();
        let mut out = String::new();
        let mut rendered = Vec::new();
        if global.is_some() || project.is_some() || !selected.is_empty() {
            append_section(&mut out, "project-instructions-guidance", GUIDANCE);
        }
        if let Some(rule) = global {
            let start = out.len();
            append_section_from(&mut out, "global-rules", &rule.source, &rule.body);
            self.append_file_limit_marker(&mut out, &rule.source, rule.observed_bytes);
            rendered.push(RenderedRule {
                source: rule.source.clone(),
                start,
                end: out.len(),
                is_global: true,
            });
        }
        self.render_scoped(&mut out, &mut rendered, &selected);
        if let Some(rule) = project {
            let start = out.len();
            append_section_from(&mut out, "project-rules", &rule.source, &rule.body);
            self.append_file_limit_marker(&mut out, &rule.source, rule.observed_bytes);
            rendered.push(RenderedRule {
                source: rule.source.clone(),
                start,
                end: out.len(),
                is_global: false,
            });
        }
        let omissions = std::mem::take(&mut self.omissions);
        omissions.render(&mut out, &mut self.notices);
        let content = (!out.is_empty()).then(|| {
            if out.len() > self.total_limit.effective_bytes() {
                cap_total(&out, &rendered, self.total_limit, &mut self.notices)
            } else {
                out
            }
        });
        ProjectContext {
            content,
            notices: self.notices,
        }
    }

    fn select_candidates(&mut self) -> Vec<UsableRule> {
        let candidates = std::mem::take(&mut self.candidates);
        let mut usable = Vec::new();
        for candidate in candidates {
            match load_rule(&candidate.source, self.file_limit.effective_bytes()) {
                RuleLoad::Body(body) => {
                    let distance = minimum_distance(&candidate.scope, &self.ranking_endpoints);
                    let depth = path_depth(&candidate.scope);
                    usable.push(UsableRule {
                        candidate,
                        body: body.text,
                        observed_bytes: body.observed_bytes,
                        distance,
                        depth,
                    });
                }
                RuleLoad::Missing | RuleLoad::Blank => {}
                RuleLoad::Omitted(reason) => self.omit(&candidate.source, reason),
            }
        }
        sort_with(&mut usable, rank_order);
        let capped = usable.split_off(usable.len().min(SCOPED_RULES));
        for rule in &usable {
            self.add_delivered(&rule.candidate.source);
        }
        for rule in &capped {
            self.omit(&rule.candidate.source, OmissionReason::SelectionCap);
        }
        sort_with(&mut usable, render_order);
        usable
    }

    fn render_scoped(
        &mut self,
        out: &mut String,
        rendered: &mut Vec<RenderedRule>,
        selected: &[UsableRule],
    ) {
        for rule in selected {
            let start = out.len();
            append_scoped_section_from(
                out,
                &rule.candidate.source,
                &rule.candidate.scope,
                &rule.body,
            );
            self.append_file_limit_marker(out, &rule.candidate.source, rule.observed_bytes);
            rendered.push(RenderedRule {
                source: rule.candidate.source.clone(),
                start,
                end: out.len(),
                is_global: false,
            });
        }
    }

    fn append_file_limit_marker(&mut self, out: &mut String, source: &Path, observed: usize) {
        let effective = self.file_limit.effective_bytes();
        if observed <= effective {
            return;
        }
        let label = self.file_limit.source.label();
        separate(out);
        out.push_str(
            "<context_limit name=\"project_instruction_file_bytes\" action=\"truncated\" source_file=\"",
        );
        write_path(out, source);
        let _ = write!(
            out,
            "\" observed_bytes=\"{observed}\" effective_bytes=\"{effective}\" source=\"{label}\" override=\"{FILE_LIMIT_OVERRIDE}\" />"
        );
        let mut notice = String::from("[context] project instruction file \"");
        write_path(&mut notice, source);
        let _ = write!(
            notice,
            "\" truncated: observed={observed} bytes effective={effective} bytes source={label}; override with {FILE_LIMIT_OVERRIDE}"
        );
        self.notices.push(notice);
    }
}

fn cap_total(
    full: &str,
    rendered: &[RenderedRule],
    limit: ContextLimit,
    notices: &mut Vec<String>,
) -> String {
    let consequence_start = rendered.last().map_or(0, |rule| rule.end);
    let observed = consequence_start;
    let effective = limit.effective_bytes();
    if observed <= effective {
        return full.to_owned();
    }
    let preamble_end = rendered.first().map_or(0, |rule| rule.start);
    let include_preamble = preamble_end > 0 && preamble_end <= effective;
    let preamble = if include_preamble { preamble_end } else { 0 };
    let mut retained = vec![false; rendered.len()];
    if preamble_end == 0 || include_preamble {
        let mut retained_bytes = 0;
        let mut retain = |index: usize, retained: &mut [bool]| {
            let size = rendered[index].end - rendered[index].start;
            if preamble + retained_bytes + size <= effective {
                retained[index] = true;
                retained_bytes += size;
            }
        };
        if let Some(global) = rendered.iter().position(|rule| rule.is_global) {
            retain(global, &mut retained);
        }
        if let Some(closest) = rendered.len().checked_sub(1)
            && !retained[closest]
        {
            retain(closest, &mut retained);
        }
        for index in 0..rendered.len() {
            if !retained[index] {
                retain(index, &mut retained);
            }
        }
    }
    let mut omitted_names = String::new();
    let mut omitted_count = 0;
    for (rule, _) in rendered.iter().zip(&retained).filter(|(_, keep)| !**keep) {
        if omitted_count > 0 {
            omitted_names.push_str(", ");
        }
        write_path(&mut omitted_names, &rule.source);
        omitted_count += 1;
    }
    let mut capped = String::new();
    if include_preamble {
        capped.push_str(&full[..preamble_end]);
    }
    for (rule, _) in rendered.iter().zip(&retained).filter(|(_, keep)| **keep) {
        capped.push_str(&full[rule.start..rule.end]);
    }
    let prior_consequences = full[consequence_start..].trim_start_matches(['\r', '\n']);
    if !prior_consequences.is_empty() {
        separate(&mut capped);
        capped.push_str(prior_consequences);
    }
    let label = limit.source.label();
    separate(&mut capped);
    let _ = write!(
        capped,
        "<context_limit name=\"project_instructions_total_bytes\" action=\"omitted\" omitted_count=\"{omitted_count}\" observed_bytes=\"{observed}\" effective_bytes=\"{effective}\" source=\"{label}\" override=\"{TOTAL_LIMIT_OVERRIDE}\" />"
    );
    notices.push(format!(
        "[context] project instructions omitted {omitted_count} source(s) ({omitted_names}): observed={observed} bytes effective={effective} bytes source={label}; override with {TOTAL_LIMIT_OVERRIDE}"
    ));
    capped
}

fn append_section(out: &mut String, tag: &str, body: &str) {
    if body.is_empty() {
        return;
    }
    separate(out);
    let _ = write!(out, "<{tag}>\n{body}\n</{tag}>");
}

fn append_section_from(out: &mut String, tag: &str, from: &Path, body: &str) {
    if body.is_empty() {
        return;
    }
    separate(out);
    let _ = write!(out, "<{tag} from=\"");
    write_path(out, from);
    let _ = write!(out, "\">\n{body}\n</{tag}>");
}

fn append_scoped_section_from(out: &mut String, from: &Path, scope: &Path, body: &str) {
    if body.is_empty() {
        return;
    }
    separate(out);
    out.push_str("<scoped-rules from=\"");
    write_path(out, from);
    out.push_str("\" scope=\"");
    write_path(out, scope);
    let _ = write!(out, "\">\n{body}\n</scoped-rules>");
}

fn rank_order(left: &UsableRule, right: &UsableRule) -> Ordering {
    left.distance
        .cmp(&right.distance)
        .then(right.depth.cmp(&left.depth))
        .then(bytes(&left.candidate.source).cmp(bytes(&right.candidate.source)))
}

fn render_order(left: &UsableRule, right: &UsableRule) -> Ordering {
    left.depth
        .cmp(&right.depth)
        .then(bytes(&left.candidate.source).cmp(bytes(&right.candidate.source)))
}

fn sort_with<T>(items: &mut [T], order: fn(&T, &T) -> Ordering) {
    for end in 1..items.len() {
        let mut index = end;
        while index > 0 && order(&items[index - 1], &items[index]) == Ordering::Greater {
            items.swap(index - 1, index);
            index -= 1;
        }
    }
}

fn minimum_distance(scope: &Path, endpoints: &[PathBuf]) -> usize {
    let scope_depth = path_depth(scope);
    endpoints
        .iter()
        .filter(|endpoint| path_inside(scope, endpoint))
        .map(|endpoint| path_depth(endpoint) - scope_depth)
        .min()
        .unwrap_or(usize::MAX)
}

fn path_depth(path: &Path) -> usize {
    bytes(path)
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .count()
}

fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn parent_directory(path: &Path) -> Option<&Path> {
    dirname(bytes(path)).map(|parent| Path::new(OsStr::from_bytes(parent)))
}

fn file_name(path: &Path) -> &OsStr {
    OsStr::from_bytes(basename(bytes(path)))
}

fn push_unique(paths: &mut Vec<PathBuf>, path: &Path) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests;

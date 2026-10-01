use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use ofx_config::{
    ContextLimit, ContextLimitName, ContextLimitSource, ContextLimits, EMERGENCY_CEILING_BYTES,
};
use ofx_text::{is_model_safe_text, sanitize_model_text_owned};
use ofx_workspace::{basename, dirname};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};

use super::diagnostics::diagnostic_summary;
use crate::encoded_scalar::{encoded_scalar, write_bounded_encoded_scalar};
use crate::skill_contract::{
    Locations, MAX_NAME_BYTES, Skill, SkillDiagnostic, SkillDiagnosticScope,
};

const HEADER: &str = "Skills provide task instructions. Use named skills and clearly matching skills before substantive work.\nRead selected skills completely, including required references. Descriptions may be shortened; metadata is not loaded instructions.\n<available_skills>\n";
const FOOTER: &str = "</available_skills>\n";
const CATALOG_NOTICE_NAME_COUNT: usize = 8;
const DEFAULT_DESCRIPTION_CHARACTERS: usize = 1024;
const DEFAULT_CATALOG_CHARACTERS: usize = 8000;
const NAMESPACE_SEED: &[u8] = b"fx-skill-locations";
const LOCATION_LEAF: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    pub text: String,
    pub notice: Option<String>,
    pub diagnostic_notice: Option<String>,
    pub locations: Locations,
}

#[derive(Debug, Clone, Copy)]
struct CatalogLimits {
    description: ContextLimit,
    catalog: ContextLimit,
}

pub fn build_skill_prompt(
    skills: &[Skill],
    diagnostics: &[SkillDiagnostic],
    limits: &ContextLimits,
    context_window: Option<u32>,
) -> SkillCatalog {
    let limits = CatalogLimits {
        description: limits.get(ContextLimitName::SkillDescriptionBytes),
        catalog: limits.get(ContextLimitName::SkillCatalogBytes),
    };
    render_catalog(skills, diagnostics, limits, context_window)
}

struct VisibleSkill<'a> {
    name: &'a str,
    description: &'a str,
    path: &'a str,
}

fn render_catalog(
    skills: &[Skill],
    diagnostics: &[SkillDiagnostic],
    limits: CatalogLimits,
    context_window: Option<u32>,
) -> SkillCatalog {
    let visible: Vec<VisibleSkill<'_>> = skills
        .iter()
        .filter_map(|skill| {
            let path = skill.path.to_str()?;
            let representable =
                is_model_safe_text(skill.name.as_bytes()) && is_model_safe_text(path.as_bytes());
            representable.then_some(VisibleSkill {
                name: &skill.name,
                description: &skill.description,
                path,
            })
        })
        .collect();
    let mut catalog = render_visible_skills(
        &visible,
        limits,
        context_window,
        catalog_namespace(&visible),
    );
    catalog.locations.skills = skills.to_vec();
    catalog.locations.diagnostics = diagnostics.to_vec();
    let withheld = skills.len() - visible.len();
    if withheld > 0 {
        let mut notice = catalog.notice.take().unwrap_or_default();
        let _ = writeln!(
            notice,
            "[context] {withheld} skill identities withheld because they cannot be safely represented to the model."
        );
        catalog.notice = Some(notice);
    }
    attach_catalog_diagnostics(&mut catalog, diagnostics);
    catalog
}

fn attach_catalog_diagnostics(catalog: &mut SkillCatalog, diagnostics: &[SkillDiagnostic]) {
    let Some(summary) = diagnostic_summary(diagnostics) else {
        return;
    };
    let root_count = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.scope == SkillDiagnosticScope::Root)
        .count();
    let candidate_count = diagnostics.len() - root_count;
    let missing = if root_count > 0 { "unknown" } else { "0" };
    catalog.text = format!(
        "<skill_discovery_warning skipped_candidate_count=\"{candidate_count}\" incomplete_root_count=\"{root_count}\" missing_from_incomplete_roots=\"{missing}\" />\n{}",
        catalog.text
    );
    catalog.diagnostic_notice = Some(summary);
}

fn catalog_namespace(visible: &[VisibleSkill<'_>]) -> u64 {
    let mut hash = Sha256::new();
    hash.update(NAMESPACE_SEED);
    for skill in visible {
        for value in [skill.name, skill.path] {
            hash.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
            hash.update(value);
        }
    }
    let digest = hash.finalize();
    let mut prefix = [0; 8];
    prefix.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(prefix)
}

#[derive(Debug, Clone, Copy)]
struct Budget {
    limit: usize,
    characters: bool,
}

impl Budget {
    fn resolve(limit: ContextLimit, context_window: Option<u32>) -> Self {
        let bytes = |limit| Self {
            limit,
            characters: false,
        };
        if limit.source != ContextLimitSource::CompiledDefault {
            return bytes(limit.effective_bytes());
        }
        if let Some(window) = context_window.filter(|window| *window > 0) {
            let ceiling = EMERGENCY_CEILING_BYTES;
            let scaled = (u64::from(window) * 2 / 100).max(1) * 4;
            return bytes(usize::try_from(scaled).map_or(ceiling, |scaled| scaled.min(ceiling)));
        }
        Self {
            limit: DEFAULT_CATALOG_CHARACTERS,
            characters: true,
        }
    }

    fn cost(self, text: &str) -> usize {
        if self.characters {
            text.chars().count()
        } else {
            text.len()
        }
    }

    fn unit(self) -> &'static str {
        if self.characters {
            "characters"
        } else {
            "bytes"
        }
    }
}

struct Entry {
    prefix: String,
    description: String,
    suffix: String,
    root_count: usize,
    description_end: usize,
    description_limited: bool,
}

struct Layout<'a> {
    roots: Vec<&'a str>,
    root_lines: Vec<String>,
    entries: Vec<Entry>,
}

struct Retention {
    entries: usize,
    roots: usize,
    marker: String,
    minimum_cost: usize,
}

fn render_visible_skills(
    skills: &[VisibleSkill<'_>],
    limits: CatalogLimits,
    context_window: Option<u32>,
    namespace: u64,
) -> SkillCatalog {
    if skills.is_empty() {
        return SkillCatalog::default();
    }
    let budget = Budget::resolve(limits.catalog, context_window);
    let mut layout = layout_entries(skills, limits.description, namespace);
    let mut retention = retain_within_budget(&layout, budget);
    let output_bytes = spread_descriptions(&mut layout, &retention, budget);

    let mut text = String::new();
    if retention.minimum_cost <= budget.limit && output_bytes <= EMERGENCY_CEILING_BYTES {
        text.reserve(output_bytes);
        text.push_str(HEADER);
        for line in &layout.root_lines[..retention.roots] {
            text.push_str(line);
        }
        for entry in &layout.entries[..retention.entries] {
            text.push_str(&entry.prefix);
            text.push_str(&entry.description[..entry.description_end]);
            text.push_str(&entry.suffix);
        }
        text.push_str(&retention.marker);
        text.push_str(FOOTER);
    } else {
        retention.entries = 0;
        retention.roots = 0;
    }

    let notice = catalog_notices(
        &layout.entries[..retention.entries],
        &skills[retention.entries..],
        limits,
        budget,
    );
    SkillCatalog {
        text,
        notice: (!notice.is_empty()).then_some(notice),
        diagnostic_notice: None,
        locations: Locations {
            namespace,
            roots: layout.roots[..retention.roots]
                .iter()
                .map(PathBuf::from)
                .collect(),
            ..Locations::default()
        },
    }
}

fn layout_entries<'a>(
    skills: &[VisibleSkill<'a>],
    description_limit: ContextLimit,
    namespace: u64,
) -> Layout<'a> {
    let mut layout = Layout {
        roots: Vec::new(),
        root_lines: Vec::new(),
        entries: Vec::with_capacity(skills.len()),
    };
    let mut root_indices: HashMap<&str, usize> = HashMap::new();
    for skill in skills {
        let root = &skill.path[..dirname(skill.path.as_bytes()).map_or(0, <[u8]>::len)];
        let root_index = *root_indices.entry(root).or_insert_with(|| {
            let index = layout.roots.len();
            layout.roots.push(root);
            layout
                .root_lines
                .push(format!("Root {index}: {}\n", encoded_scalar(root)));
            index
        });
        let (description, description_limited) =
            catalog_description(skill.description, description_limit);
        let leaf = std::str::from_utf8(basename(skill.path.as_bytes())).unwrap_or_default();
        layout.entries.push(Entry {
            prefix: format!("- {}: ", encoded_scalar(skill.name)),
            description,
            suffix: format!(
                " (location: skill:{namespace:016x}:{root_index}/{})\n",
                utf8_percent_encode(leaf, LOCATION_LEAF)
            ),
            root_count: layout.roots.len(),
            description_end: 0,
            description_limited,
        });
    }
    layout
}

fn retain_within_budget(layout: &Layout<'_>, budget: Budget) -> Retention {
    let base_cost = budget.cost(HEADER) + budget.cost(FOOTER);
    let line_cost = |lines: &[String]| lines.iter().map(|line| budget.cost(line)).sum::<usize>();
    let identity_cost = |entry: &Entry| budget.cost(&entry.prefix) + budget.cost(&entry.suffix);
    let complete_cost = base_cost
        + line_cost(&layout.root_lines)
        + layout.entries.iter().map(identity_cost).sum::<usize>();
    if complete_cost <= budget.limit {
        return Retention {
            entries: layout.entries.len(),
            roots: layout.roots.len(),
            marker: String::new(),
            minimum_cost: complete_cost,
        };
    }

    let total = layout.entries.len();
    let mut retention = Retention {
        entries: 0,
        roots: 0,
        marker: String::new(),
        minimum_cost: base_cost,
    };
    let mut candidate_cost = base_cost;
    let mut candidate_roots = 0;
    for (index, entry) in layout.entries.iter().enumerate() {
        candidate_cost += line_cost(&layout.root_lines[candidate_roots..entry.root_count]);
        candidate_roots = entry.root_count;
        candidate_cost += identity_cost(entry);
        if candidate_cost + budget.cost(&omitted_marker(total - index - 1)) > budget.limit {
            break;
        }
        retention.entries = index + 1;
        retention.roots = candidate_roots;
        retention.minimum_cost = candidate_cost;
    }
    retention.marker = omitted_marker(total - retention.entries);
    retention.minimum_cost += budget.cost(&retention.marker);
    retention
}

fn spread_descriptions(layout: &mut Layout<'_>, retention: &Retention, budget: Budget) -> usize {
    let ceiling = EMERGENCY_CEILING_BYTES;
    let mut output_bytes = HEADER.len()
        + FOOTER.len()
        + retention.marker.len()
        + layout.root_lines[..retention.roots]
            .iter()
            .map(String::len)
            .sum::<usize>()
        + layout.entries[..retention.entries]
            .iter()
            .map(|entry| entry.prefix.len() + entry.suffix.len())
            .sum::<usize>();
    let mut available = budget.limit.saturating_sub(retention.minimum_cost);
    let mut growing: Vec<usize> = (0..retention.entries).collect();
    while !growing.is_empty() && available > 0 {
        growing.retain(|&index| {
            let entry = &mut layout.entries[index];
            let rest = &entry.description[entry.description_end..];
            let Some(length) = encoded_scalar_length(rest) else {
                return false;
            };
            let cost = budget.cost(&rest[..length]);
            if cost > available || length > ceiling.saturating_sub(output_bytes) {
                return false;
            }
            entry.description_end += length;
            available -= cost;
            output_bytes += length;
            true
        });
    }
    output_bytes
}

fn catalog_description(description: &str, limit: ContextLimit) -> (String, bool) {
    let safe = sanitize_model_text_owned(description.as_bytes().to_vec());
    if limit.source == ContextLimitSource::CompiledDefault {
        let end = safe
            .char_indices()
            .nth(DEFAULT_DESCRIPTION_CHARACTERS)
            .map_or(safe.len(), |(index, _)| index);
        return (encoded_scalar(&safe[..end]), end < safe.len());
    }
    let mut bounded = String::new();
    let observed =
        write_bounded_encoded_scalar(&mut bounded, safe.as_bytes(), limit.effective_bytes());
    let limited = observed > bounded.len();
    (bounded, limited)
}

fn catalog_notices(
    retained: &[Entry],
    omitted: &[VisibleSkill<'_>],
    limits: CatalogLimits,
    budget: Budget,
) -> String {
    let mut notices = String::new();
    let description_shortened = retained
        .iter()
        .filter(|entry| entry.description_limited)
        .count();
    let catalog_shortened = retained
        .iter()
        .filter(|entry| entry.description_end < entry.description.len())
        .count();
    if description_shortened > 0 {
        let _ = writeln!(
            notices,
            "[context] skill descriptions shortened: {description_shortened}; source={}",
            limits.description.source.label()
        );
    }
    let catalog_source = limits.catalog.source.label();
    if catalog_shortened > 0 {
        let _ = writeln!(
            notices,
            "[context] skill catalog shortened {catalog_shortened} descriptions: effective={} {} source={catalog_source}",
            budget.limit,
            budget.unit()
        );
    }
    if !omitted.is_empty() {
        let shown = omitted.len().min(CATALOG_NOTICE_NAME_COUNT);
        let _ = write!(
            notices,
            "[context] skill catalog omitted {} entries (",
            omitted.len()
        );
        for (index, skill) in omitted[..shown].iter().enumerate() {
            if index > 0 {
                notices.push_str(", ");
            }
            if write_bounded_encoded_scalar(&mut notices, skill.name.as_bytes(), MAX_NAME_BYTES)
                > MAX_NAME_BYTES
            {
                notices.push_str("...");
            }
        }
        if shown < omitted.len() {
            let _ = write!(notices, ", +{} more", omitted.len() - shown);
        }
        let _ = writeln!(
            notices,
            "): effective={} {} source={catalog_source}",
            budget.limit,
            budget.unit()
        );
    }
    notices
}

fn omitted_marker(count: usize) -> String {
    format!("Omitted skills: {count}.\n")
}

fn encoded_scalar_length(text: &str) -> Option<usize> {
    let first = text.chars().next()?;
    if first == '&'
        && let Some(end) = text.find(';')
    {
        return Some(end + 1);
    }
    Some(first.len_utf8())
}

#[cfg(test)]
mod tests;

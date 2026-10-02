use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::encoded_scalar::write_bounded_encoded_scalar;
use crate::skill_contract::{
    MAX_FRONTMATTER_BYTES, SkillDiagnostic, SkillDiagnosticCause, SkillDiagnosticScope,
};

const DIAGNOSTIC_NOTICE_ITEM_COUNT: usize = 4;
const DIAGNOSTIC_PATH_MAX_BYTES: usize = 4 * 1024;

pub fn diagnostic_summary(diagnostics: &[SkillDiagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    let mut summary = String::from("skill discovery warning: ");
    let shown_count = diagnostics.len().min(DIAGNOSTIC_NOTICE_ITEM_COUNT);
    for (index, diagnostic) in diagnostics[..shown_count].iter().enumerate() {
        if index > 0 {
            summary.push_str("; ");
        }
        match diagnostic.scope {
            SkillDiagnosticScope::Root => {
                summary.push_str("inventory incomplete because root \"");
                write_bounded_diagnostic_path(&mut summary, &diagnostic.path);
                summary.push_str("\" could not be read, so an unknown number of skills may be missing; fix access to the root and reload skills");
            }
            SkillDiagnosticScope::Candidate => {
                summary.push_str("candidate \"");
                write_bounded_diagnostic_path(&mut summary, &diagnostic.path);
                summary.push_str("\" was skipped because ");
                write_candidate_consequence(&mut summary, diagnostic.cause);
            }
        }
    }
    let omitted = diagnostics.len() - shown_count;
    if omitted > 0 {
        let plural = if omitted == 1 { "" } else { "s" };
        let _ = write!(summary, "; {omitted} additional diagnostic{plural} omitted");
    }
    Some(summary)
}

fn write_candidate_consequence(summary: &mut String, cause: SkillDiagnosticCause) {
    let _ = match cause {
        SkillDiagnosticCause::InvalidMetadata(cause) => write!(
            summary,
            "its metadata is invalid ({}); use one safe name and an optional inline description or a >, >-, or | block, then reload skills",
            cause.name()
        ),
        SkillDiagnosticCause::LinkedCandidateUnavailable => summary.write_str(
            "its linked skill directory could not be resolved to an authorized readable directory; repair or remove the link, or authorize its external location, then reload skills",
        ),
        SkillDiagnosticCause::Unreadable => summary.write_str(
            "SKILL.md is unreadable or not a regular file; fix the file type or access, then reload skills",
        ),
        SkillDiagnosticCause::Oversized => write!(
            summary,
            "its frontmatter exceeds the supported {MAX_FRONTMATTER_BYTES}-byte metadata header; shorten the name/description header, then reload skills"
        ),
    };
}

fn write_bounded_diagnostic_path(summary: &mut String, path: &Path) {
    let observed = write_bounded_encoded_scalar(
        summary,
        path.as_os_str().as_bytes(),
        DIAGNOSTIC_PATH_MAX_BYTES,
    );
    if observed > DIAGNOSTIC_PATH_MAX_BYTES {
        summary.push_str("...");
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;
    use crate::skill_contract::{InvalidMetadataCause, SkillSource};

    fn candidate(path: &str, cause: SkillDiagnosticCause) -> SkillDiagnostic {
        SkillDiagnostic {
            path: PathBuf::from(path),
            source: SkillSource::WorkspaceShared,
            scope: SkillDiagnosticScope::Candidate,
            cause,
        }
    }

    #[test]
    fn skill_diagnostic_summary_identifies_candidate_and_root_consequences() {
        let diagnostics = [
            candidate(
                "/tmp/hostile\npath/body-sentinel",
                SkillDiagnosticCause::InvalidMetadata(InvalidMetadataCause::MissingName),
            ),
            SkillDiagnostic {
                path: PathBuf::from("/tmp/unreadable-root"),
                source: SkillSource::GlobalOhFx,
                scope: SkillDiagnosticScope::Root,
                cause: SkillDiagnosticCause::Unreadable,
            },
        ];
        let summary = diagnostic_summary(&diagnostics).unwrap();
        assert!(summary.contains("candidate \"/tmp/hostile&#x0a;path/body-sentinel\" was skipped"));
        assert!(summary.contains("metadata is invalid (missing_name)"));
        assert!(summary.contains("optional inline description or a >, >-, or | block"));
        assert!(summary.contains("inventory incomplete because root \"/tmp/unreadable-root\""));
        assert!(summary.contains("unknown number of skills may be missing"));
        assert!(summary.ends_with("fix access to the root and reload skills"));
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn skill_diagnostic_summary_reports_exact_omitted_count_and_metadata_limit() {
        let diagnostics = vec![candidate("/tmp/oversized", SkillDiagnosticCause::Oversized); 6];
        let summary = diagnostic_summary(&diagnostics).unwrap();
        assert!(summary.contains("frontmatter exceeds the supported 65536-byte metadata header"));
        assert!(summary.contains("2 additional diagnostics omitted"));
        assert_eq!(
            diagnostic_summary(&diagnostics[..5])
                .unwrap()
                .matches("1 additional diagnostic omitted")
                .count(),
            1
        );
        assert_eq!(diagnostic_summary(&[]), None);
    }

    #[test]
    fn skill_diagnostic_summary_stops_a_path_before_invalid_utf_8() {
        let path = PathBuf::from(OsStr::from_bytes(b"/tmp/caf\xe9/skill"));
        let summary = diagnostic_summary(&[SkillDiagnostic {
            path,
            source: SkillSource::GlobalOhFx,
            scope: SkillDiagnosticScope::Candidate,
            cause: SkillDiagnosticCause::Unreadable,
        }])
        .unwrap();
        assert!(summary.starts_with("skill discovery warning: candidate \"/tmp/caf\" was skipped"));
    }

    #[test]
    fn skill_diagnostic_summary_bounds_each_path_without_splitting_an_entity() {
        let long_path = format!("/{}", "<".repeat(2000));
        let summary = diagnostic_summary(&[candidate(
            &long_path,
            SkillDiagnosticCause::LinkedCandidateUnavailable,
        )])
        .unwrap();
        let shown = summary
            .strip_prefix("skill discovery warning: candidate \"/")
            .unwrap();
        assert!(shown.starts_with(&"&lt;".repeat(1023)));
        assert!(shown.contains("&lt;...\" was skipped because its linked skill directory"));
    }
}

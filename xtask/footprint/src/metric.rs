use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Metric {
    StrippedBytes,
    ArchiveBytes,
    VersionInstructions,
    HelpInstructions,
    AskInstructions,
    StreamDeltaInstructions,
    CaStoreSyscalls,
    ToolGitProcesses,
    VersionSyscalls,
    HelpSyscalls,
    AskSyscalls,
    AskPeakRss,
    ToolPeakRss,
    StreamPeakRss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Bytes,
    Count,
    Kibibytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Size,
    Instructions,
    StreamInstructions,
    Untracked,
}

pub(crate) type Readings = BTreeMap<Metric, Result<u64, String>>;

impl Metric {
    pub(crate) const ALL: [Self; 14] = [
        Self::StrippedBytes,
        Self::ArchiveBytes,
        Self::VersionInstructions,
        Self::HelpInstructions,
        Self::AskInstructions,
        Self::StreamDeltaInstructions,
        Self::CaStoreSyscalls,
        Self::ToolGitProcesses,
        Self::VersionSyscalls,
        Self::HelpSyscalls,
        Self::AskSyscalls,
        Self::AskPeakRss,
        Self::ToolPeakRss,
        Self::StreamPeakRss,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::StrippedBytes => "stripped_bytes",
            Self::ArchiveBytes => "archive_bytes",
            Self::VersionInstructions => "version_instructions",
            Self::HelpInstructions => "help_instructions",
            Self::AskInstructions => "ask_instructions",
            Self::StreamDeltaInstructions => "stream_delta_instructions",
            Self::CaStoreSyscalls => "ca_store_syscalls",
            Self::ToolGitProcesses => "tool_git_processes",
            Self::VersionSyscalls => "version_syscalls",
            Self::HelpSyscalls => "help_syscalls",
            Self::AskSyscalls => "ask_syscalls",
            Self::AskPeakRss => "ask_peak_rss_kib",
            Self::ToolPeakRss => "tool_peak_rss_kib",
            Self::StreamPeakRss => "stream_peak_rss_kib",
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|metric| metric.id() == id)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::StrippedBytes => "Stripped binary",
            Self::ArchiveBytes => "Release archive (`.tar.gz`)",
            Self::VersionInstructions => "`--version` instructions",
            Self::HelpInstructions => "`--help` instructions",
            Self::AskInstructions => "`ask \"hi\"` instructions, http provider",
            Self::StreamDeltaInstructions => "Instructions per streamed delta",
            Self::CaStoreSyscalls => "CA store path syscalls, http provider",
            Self::ToolGitProcesses => "git processes, tool script",
            Self::VersionSyscalls => "`--version` syscalls",
            Self::HelpSyscalls => "`--help` syscalls",
            Self::AskSyscalls => "`ask \"hi\"` syscalls",
            Self::AskPeakRss => "Own peak RSS, `ask \"hi\"`",
            Self::ToolPeakRss => "Own peak RSS, tool script",
            Self::StreamPeakRss => "Own peak RSS, streamed answer",
        }
    }

    pub(crate) fn unit(self) -> Unit {
        match self {
            Self::StrippedBytes | Self::ArchiveBytes => Unit::Bytes,
            Self::AskPeakRss | Self::ToolPeakRss | Self::StreamPeakRss => Unit::Kibibytes,
            _ => Unit::Count,
        }
    }

    pub(crate) fn step(self) -> Step {
        match self {
            Self::StrippedBytes | Self::ArchiveBytes => Step::Size,
            Self::VersionInstructions | Self::HelpInstructions | Self::AskInstructions => {
                Step::Instructions
            }
            Self::StreamDeltaInstructions => Step::StreamInstructions,
            _ => Step::Untracked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_are_unique() {
        for metric in Metric::ALL {
            assert_eq!(Metric::from_id(metric.id()), Some(metric));
        }
        let mut ids: Vec<_> = Metric::ALL.iter().map(|metric| metric.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Metric::ALL.len());
        assert_eq!(Metric::from_id("unknown"), None);
    }
}

mod bounded_process;
mod fs_path;
mod glob_pattern;
mod grep_search;
mod ignored_dirs;
mod path_error;
mod pathing;
mod regular_file;
mod workspace_files;

pub use fs_path::{basename, dirname};
pub use glob_pattern::{CompileError, MAX_PATTERN_BYTES, Pattern};
pub use grep_search::{
    COLLECTION_CAP, CountResult, GrepQuery, GrepResult, Match, OUTPUT_CAP, TruncatedReason,
    collect_directory_matches, collect_regular_file_root, count_directory_matches,
    count_regular_file_root, read_model_safe,
};
pub use ignored_dirs::IGNORED_DIRECTORY_NAMES;
pub use path_error::PathError;
pub use pathing::{
    PATH_ENTRY_WHITESPACE, path_inside, resolve_workspace_or_external_path, resolve_workspace_path,
    workspace_relative_path,
};
pub use regular_file::{RegularFileError, open_regular_file};
pub use workspace_files::{
    CandidatePaths, CandidateStats, DEFAULT_CANDIDATE_CAP, Discovery, DiscoveryOptions,
    GIT_REPOSITORY_VARIABLES, MAX_RELATIVE_PATH_BYTES, Source, UntrackedFiles, discover,
    path_contains_hidden_directory_component,
};

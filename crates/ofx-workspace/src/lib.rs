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
    FileIdentity, FileKind, FileMutationTarget, MAX_PATH_BYTES, PATH_ENTRY_WHITESPACE, TargetMode,
    descriptor_identity, entry_identity, open_child_directory, open_directory, path_inside,
    resolve_file_mutation_target, resolve_workspace_or_external_path, resolve_workspace_path,
    workspace_relative_path,
};
pub use regular_file::{
    RegularFileError, open_regular_file, open_regular_file_at, open_regular_file_following_at,
    opened_file_path,
};
pub use workspace_files::{
    CandidatePaths, CandidateStats, DEFAULT_CANDIDATE_CAP, Discovery, DiscoveryOptions,
    GIT_REPOSITORY_VARIABLES, MAX_RELATIVE_PATH_BYTES, Source, UntrackedFiles, discover,
    path_contains_hidden_directory_component,
};

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ofx_tui::{
    DirectoryLister, FileMatch, FileMentionSource, IndexRevision, IndexState, MentionKind,
};
use ofx_workspace::{
    CandidateKind, FileIndex, QueryMode, ReadableRevision, SearchResult, complete_path,
    is_current_path_kind, query_mode,
};

pub(crate) struct WorkspaceFileMentions {
    index: FileIndex,
    workspace_root: PathBuf,
}

impl WorkspaceFileMentions {
    pub(crate) fn start(workspace_root: &Path, cache_dir: Option<&Path>) -> Self {
        let mut index = FileIndex::new(cache_dir.map(Path::to_owned));
        index.ensure_scope(workspace_root);
        Self {
            index,
            workspace_root: workspace_root.to_owned(),
        }
    }
}

impl FileMentionSource for WorkspaceFileMentions {
    fn revision(&self) -> IndexRevision {
        let revision = self.index.readable_revision();
        IndexRevision {
            generation: revision.generation,
            count: revision.count,
            state: index_state(revision.state),
        }
    }

    fn search(&self, revision: IndexRevision, query: &str, limit: usize) -> Option<Vec<FileMatch>> {
        if query_mode(query) != QueryMode::WorkspaceIndex {
            return None;
        }
        let readable = ReadableRevision {
            generation: revision.generation,
            count: revision.count,
            ..self.index.readable_revision()
        };
        self.index
            .search_at_revision(readable, query, limit)
            .ok()
            .map(file_matches)
    }

    fn refresh(&mut self) {
        self.index.refresh();
    }

    fn poll(&mut self) -> bool {
        self.index.join_if_done()
    }

    fn is_loading(&self) -> bool {
        self.index.is_loading()
    }

    fn depends_on_index(&self, query: &str) -> bool {
        query_mode(query) == QueryMode::WorkspaceIndex
    }

    fn is_current(&self, query: &str, path: &str, kind: MentionKind) -> bool {
        let kind = candidate_kind(kind);
        match query_mode(query) {
            QueryMode::WorkspaceIndex => self.index.is_current_candidate_kind(path, kind),
            QueryMode::ExplicitPath => is_current_path_kind(&self.workspace_root, path, kind),
        }
    }

    fn directory_lister(&self) -> DirectoryLister {
        let workspace_root = self.workspace_root.clone();
        Arc::new(move |query: &str, limit: usize, cancel: &AtomicBool| {
            let home = env::var_os("HOME");
            complete_path(&workspace_root, home.as_deref(), query, cancel, limit)
                .ok()
                .map(file_matches)
        })
    }
}

fn index_state(state: ofx_workspace::IndexState) -> IndexState {
    match state {
        ofx_workspace::IndexState::Idle => IndexState::Idle,
        ofx_workspace::IndexState::Loading => IndexState::Loading,
        ofx_workspace::IndexState::Ready => IndexState::Ready,
        ofx_workspace::IndexState::Failed => IndexState::Failed,
    }
}

fn candidate_kind(kind: MentionKind) -> CandidateKind {
    match kind {
        MentionKind::File => CandidateKind::File,
        MentionKind::Directory => CandidateKind::Directory,
    }
}

fn file_matches(results: Vec<SearchResult>) -> Vec<FileMatch> {
    results
        .into_iter()
        .map(|result| FileMatch {
            path: result.path,
            kind: match result.kind {
                CandidateKind::File => MentionKind::File,
                CandidateKind::Directory => MentionKind::Directory,
            },
            spans: result.matched_spans,
        })
        .collect()
}

#[cfg(test)]
mod tests;

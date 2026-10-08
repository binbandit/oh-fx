use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ofx_contract::LiveAdditionalRoots;
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
    roots: LiveAdditionalRoots,
    indexed_roots: Arc<[PathBuf]>,
    scope_epoch: u64,
}

impl WorkspaceFileMentions {
    pub(crate) fn start(
        workspace_root: &Path,
        roots: LiveAdditionalRoots,
        cache_dir: Option<&Path>,
    ) -> Self {
        let indexed_roots = roots.get();
        let mut index = FileIndex::new(cache_dir.map(Path::to_owned));
        index.ensure_scope(workspace_root, &indexed_roots);
        Self {
            index,
            workspace_root: workspace_root.to_owned(),
            roots,
            indexed_roots,
            scope_epoch: 0,
        }
    }

    fn scope_is_installed(&self) -> bool {
        Arc::ptr_eq(&self.roots.get(), &self.indexed_roots)
    }

    fn follow_scope(&mut self) -> bool {
        let roots = self.roots.get();
        if Arc::ptr_eq(&roots, &self.indexed_roots) {
            return false;
        }
        self.indexed_roots = roots;
        self.scope_epoch = self.scope_epoch.wrapping_add(1);
        self.index
            .refresh_scope(&self.workspace_root, &self.indexed_roots, self.scope_epoch);
        true
    }
}

impl FileMentionSource for WorkspaceFileMentions {
    fn revision(&self) -> IndexRevision {
        let revision = self.index.readable_revision();
        IndexRevision {
            scope_epoch: self.scope_epoch,
            generation: revision.generation,
            count: revision.count,
            state: index_state(revision.state),
        }
    }

    fn search(&self, revision: IndexRevision, query: &str, limit: usize) -> Option<Vec<FileMatch>> {
        let current = self.index.readable_revision();
        if query_mode(query) != QueryMode::WorkspaceIndex
            || revision.scope_epoch != self.scope_epoch
            || current.scope_epoch != self.scope_epoch
        {
            return None;
        }
        let readable = ReadableRevision {
            generation: revision.generation,
            count: revision.count,
            ..current
        };
        self.index
            .search_at_revision(readable, query, limit)
            .ok()
            .map(file_matches)
    }

    fn refresh(&mut self) {
        if !self.follow_scope() {
            self.index.refresh();
        }
    }

    fn poll(&mut self) -> bool {
        let followed = self.follow_scope();
        self.index.join_if_done() || followed
    }

    fn is_loading(&self) -> bool {
        self.index.is_loading()
    }

    fn depends_on_index(&self, query: &str) -> bool {
        query_mode(query) == QueryMode::WorkspaceIndex
    }

    fn is_current(&self, query: &str, path: &str, kind: MentionKind) -> bool {
        if !self.scope_is_installed() {
            return false;
        }
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

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ofx_permissions::{FileMutationKind, FileMutationTargets, TraversalDirectory};
use ofx_text::{encode_terminal_safe, encode_terminal_safe_path_tail};
use ofx_workspace::{
    FileIdentity, PathError, descriptor_identity, entry_identity, open_child_directory,
    open_directory,
};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RenameFlags, Stat, fchmod, fstat, linkat, mkdirat, openat,
    renameat, renameat_with, unlinkat,
};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENCODED_PATH_BYTES: usize = 4 * 1024;
const WRITE_CHUNK_BYTES: usize = 64 * 1024;
const STAGE_PREFIX: &str = ".fx-stage-";
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOCTTY);
const STAGE_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const DEFAULT_FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::WGRP)
    .union(Mode::ROTH)
    .union(Mode::WOTH);
const DEFAULT_DIRECTORY_MODE: Mode = Mode::RWXU.union(Mode::RWXG).union(Mode::RWXO);
const WRITE_BITS: Mode = Mode::WUSR.union(Mode::WGRP).union(Mode::WOTH);

const IDENTITY_CHANGED: &str =
    "file mutation preparation failed: approved filesystem identity changed";
const PREVIEW_TOO_LARGE: &str =
    "file mutation preparation failed: diff preview exceeds preparation limits";
const NOT_REGULAR_FILE: &str = "file mutation preparation failed: target is not a regular file";
const PREIMAGE_TOO_LARGE: &str =
    "file mutation preparation failed: preimage exceeds the 4 MiB preparation limit";
const PREIMAGE_UNREADABLE: &str =
    "file mutation preparation failed: unable to read the approved preimage";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MutationInput {
    Write(String),
}

impl MutationInput {
    pub(crate) fn kind(&self) -> FileMutationKind {
        match self {
            Self::Write(_) => FileMutationKind::Write,
        }
    }

    fn postimage(&self) -> Vec<u8> {
        match self {
            Self::Write(content) => content.as_bytes().to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrepareFailure {
    Semantic(String),
    Operational(PathError),
}

impl PrepareFailure {
    fn from_path(error: PathError) -> Self {
        if is_operational(error) {
            Self::Operational(error)
        } else {
            Self::Semantic(IDENTITY_CHANGED.to_owned())
        }
    }
}

fn is_operational(error: PathError) -> bool {
    matches!(
        error,
        PathError::SystemResources
            | PathError::OutOfMemory
            | PathError::ProcessFdQuotaExceeded
            | PathError::SystemFdQuotaExceeded
            | PathError::Unexpected
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Preimage {
    Absent,
    Present { content: Vec<u8>, hash: Vec<u8> },
}

#[derive(Debug)]
pub(crate) struct PreparedMutation {
    targets: FileMutationTargets,
    kind: FileMutationKind,
    preimage: Preimage,
    after: Vec<u8>,
    display_path: String,
}

impl PreparedMutation {
    pub(crate) fn prepare(
        targets: FileMutationTargets,
        requested_path: &str,
        input: &MutationInput,
    ) -> Result<Self, PrepareFailure> {
        let kind = input.kind();
        let preimage = read_preimage(&targets, kind)?;
        let after = input.postimage();
        let approval_path = if targets.target.anchor_is_external {
            targets.target.path().into_os_string()
        } else {
            OsString::from(requested_path)
        };
        let display_path =
            encode_terminal_safe_path_tail(approval_path.as_bytes(), MAX_ENCODED_PATH_BYTES)
                .ok_or_else(|| PrepareFailure::Semantic(PREVIEW_TOO_LARGE.to_owned()))?;
        Ok(Self {
            targets,
            kind,
            preimage,
            after,
            display_path,
        })
    }

    pub(crate) fn targets(&self) -> &FileMutationTargets {
        &self.targets
    }

    pub(crate) fn creates_file(&self) -> bool {
        self.preimage == Preimage::Absent
    }

    pub(crate) fn is_noop(&self) -> bool {
        matches!(&self.preimage, Preimage::Present { content, .. } if *content == self.after)
    }

    pub(crate) fn noop_message(&self) -> String {
        format!(
            "No changes to {}; it already contains the requested content",
            self.display_path
        )
    }

    pub(crate) fn success_message(&self) -> String {
        let verb = match self.kind {
            FileMutationKind::Write => "wrote",
            FileMutationKind::Edit => "edited",
        };
        let target = if self.targets.target.anchor_is_external {
            self.targets.target.path()
        } else {
            self.targets.target.components.iter().collect()
        };
        let encoded = encode_terminal_safe(target.as_os_str().as_bytes(), MAX_ENCODED_PATH_BYTES);
        format!("{verb} {} ({} bytes)", encoded.text, self.after.len())
    }

    pub(crate) fn apply(&self, cancel: &CancellationToken) -> Result<(), Rejection> {
        self.apply_with(cancel, &mut |_| {}, rename_new)
    }

    fn apply_with(
        &self,
        cancel: &CancellationToken,
        checkpoint: &mut dyn FnMut(Checkpoint),
        create_new: CreateNew,
    ) -> Result<(), Rejection> {
        if cancel.is_cancelled() {
            return Err(Rejection::new(RejectReason::Cancelled, Vec::new()));
        }
        let mut transaction = Transaction::new(self, create_new);
        transaction
            .commit(cancel, checkpoint)
            .map_err(|reason| transaction.reject(reason))
    }

    fn target_name(&self) -> &OsStr {
        self.targets
            .target
            .components
            .last()
            .map_or(OsStr::new(""), OsString::as_os_str)
    }

    fn parent_components(&self) -> &[OsString] {
        let components = &self.targets.target.components;
        &components[..components.len().saturating_sub(1)]
    }

    fn component_path(&self, index: usize) -> PathBuf {
        let mut path = self.targets.target.anchor.clone();
        path.extend(&self.targets.target.components[..=index]);
        path
    }
}

fn read_preimage(
    targets: &FileMutationTargets,
    kind: FileMutationKind,
) -> Result<Preimage, PrepareFailure> {
    let changed = || PrepareFailure::Semantic(IDENTITY_CHANGED.to_owned());
    let absent = || {
        if targets.target_identity.is_some() || kind != FileMutationKind::Write {
            Err(changed())
        } else {
            Ok(Preimage::Absent)
        }
    };
    let anchor = open_directory(&targets.target.anchor).map_err(PrepareFailure::from_path)?;
    if descriptor_identity(&anchor).map_err(PrepareFailure::from_path)? != targets.anchor_identity {
        return Err(changed());
    }
    let Some((name, parents)) = targets.target.components.split_last() else {
        return Err(changed());
    };
    let mut current = anchor;
    let mut missing = false;
    for (entry, parent) in targets.traversal.iter().zip(parents) {
        match entry {
            TraversalDirectory::Existing(_) if missing => return Err(changed()),
            TraversalDirectory::Existing(expected) => {
                let next =
                    open_child_directory(&current, parent).map_err(PrepareFailure::from_path)?;
                if descriptor_identity(&next).map_err(PrepareFailure::from_path)? != *expected {
                    return Err(changed());
                }
                current = next;
            }
            TraversalDirectory::Create if missing => {}
            TraversalDirectory::Create => match entry_identity(&current, parent) {
                Err(PathError::FileNotFound) => missing = true,
                Err(error) => return Err(PrepareFailure::from_path(error)),
                Ok(_) => return Err(changed()),
            },
        }
    }
    if missing {
        return absent();
    }
    let observed = match entry_identity(&current, name) {
        Err(PathError::FileNotFound) => return absent(),
        Err(error) => return Err(PrepareFailure::from_path(error)),
        Ok(observed) => observed,
    };
    let expected = targets.target_identity.ok_or_else(changed)?;
    if !is_regular(observed) || !is_regular(expected) {
        return Err(PrepareFailure::Semantic(NOT_REGULAR_FILE.to_owned()));
    }
    let file = File::from(
        openat(&current, name, READ_FLAGS, Mode::empty())
            .map_err(|errno| PrepareFailure::from_path(errno_error(errno)))?,
    );
    if descriptor_identity(&file).map_err(PrepareFailure::from_path)? != expected {
        return Err(changed());
    }
    let mut content = Vec::new();
    file.take(MAX_CONTENT_BYTES as u64 + 1)
        .read_to_end(&mut content)
        .map_err(|error| {
            let error = PathError::from(error);
            if is_operational(error) {
                PrepareFailure::Operational(error)
            } else {
                PrepareFailure::Semantic(PREIMAGE_UNREADABLE.to_owned())
            }
        })?;
    if content.len() > MAX_CONTENT_BYTES {
        return Err(PrepareFailure::Semantic(PREIMAGE_TOO_LARGE.to_owned()));
    }
    let hash = Sha256::digest(&content).to_vec();
    Ok(Preimage::Present { content, hash })
}

fn is_regular(identity: FileIdentity) -> bool {
    identity.kind() == ofx_workspace::FileKind::RegularFile
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    StalePreimage,
    Cancelled,
    TraversalChanged,
    StagedSourceChanged,
    IoFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResidueReason {
    NotEmpty,
    IdentityChanged,
    RemoveFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Residue {
    path: PathBuf,
    reason: ResidueReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejection {
    reason: RejectReason,
    residue: Vec<Residue>,
}

impl Rejection {
    fn new(reason: RejectReason, residue: Vec<Residue>) -> Self {
        Self { reason, residue }
    }

    pub(crate) fn message(&self) -> String {
        let mut message = match self.reason {
            RejectReason::StalePreimage => {
                "file mutation rejected because the file changed after preview; make a new tool call for a fresh preview"
            }
            RejectReason::Cancelled => "file mutation cancelled before commit",
            RejectReason::TraversalChanged => {
                "file mutation rejected because the approved path traversal changed"
            }
            RejectReason::StagedSourceChanged => {
                "file mutation rejected because the staged file changed before commit"
            }
            RejectReason::IoFailure => "file mutation failed before commit",
        }
        .to_owned();
        if !self.residue.is_empty() {
            message.push_str("; approved parent cleanup residue:");
            for residue in &self.residue {
                let path = encode_terminal_safe(
                    residue.path.as_os_str().as_bytes(),
                    MAX_ENCODED_PATH_BYTES,
                );
                let reason = match residue.reason {
                    ResidueReason::NotEmpty => "not_empty",
                    ResidueReason::IdentityChanged => "identity_changed",
                    ResidueReason::RemoveFailed => "remove_failed",
                };
                let _ = write!(message, " {} ({reason})", path.text);
            }
        }
        message
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Changed,
    Failed,
}

impl Check {
    fn of(error: PathError) -> Self {
        if matches!(
            error,
            PathError::FileNotFound
                | PathError::NotDir
                | PathError::SymLinkLoop
                | PathError::PathAlreadyExists
                | PathError::IsDir
        ) {
            Self::Changed
        } else {
            Self::Failed
        }
    }

    fn reason(self, changed: RejectReason) -> RejectReason {
        match self {
            Self::Changed => changed,
            Self::Failed => RejectReason::IoFailure,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checkpoint {
    Staged,
    Validated,
}

struct Stage {
    name: OsString,
    identity: Option<FileIdentity>,
}

type CreateNew = fn(&OwnedFd, &OsStr, &OsStr) -> Result<(), RejectReason>;

struct Transaction<'a> {
    mutation: &'a PreparedMutation,
    create_new: CreateNew,
    realized: Vec<FileIdentity>,
    created: Vec<(usize, FileIdentity)>,
    stage: Option<Stage>,
}

impl<'a> Transaction<'a> {
    fn new(mutation: &'a PreparedMutation, create_new: CreateNew) -> Self {
        Self {
            mutation,
            create_new,
            realized: Vec::new(),
            created: Vec::new(),
            stage: None,
        }
    }

    fn commit(
        &mut self,
        cancel: &CancellationToken,
        checkpoint: &mut dyn FnMut(Checkpoint),
    ) -> Result<(), RejectReason> {
        let parent = self.realize_traversal()?;
        let result = self.stage_and_rename(&parent, cancel, checkpoint);
        if result.is_err() {
            self.remove_stage(&parent);
        }
        result
    }

    fn stage_and_rename(
        &mut self,
        parent: &OwnedFd,
        cancel: &CancellationToken,
        checkpoint: &mut dyn FnMut(Checkpoint),
    ) -> Result<(), RejectReason> {
        let mutation = self.mutation;
        let name = mutation.target_name();
        let permissions = validate_preimage(parent, name, mutation, None)?;
        let stage_name = stage_name()?;
        let descriptor = openat(
            parent,
            stage_name.as_os_str(),
            STAGE_FLAGS,
            permissions.unwrap_or(DEFAULT_FILE_MODE),
        )
        .map_err(|_| RejectReason::IoFailure)?;
        let stage = self.stage.insert(Stage {
            name: stage_name,
            identity: None,
        });
        let identity = descriptor_identity(&descriptor).map_err(|_| RejectReason::IoFailure)?;
        stage.identity = Some(identity);
        let stat = fstat(&descriptor).map_err(|_| RejectReason::IoFailure)?;
        if !is_regular_stat(&stat) || stat.st_nlink != 1 {
            return Err(RejectReason::StagedSourceChanged);
        }
        if let Some(permissions) = permissions {
            fchmod(&descriptor, permissions).map_err(|_| RejectReason::IoFailure)?;
        }
        let mut file = File::from(descriptor);
        for chunk in mutation.after.chunks(WRITE_CHUNK_BYTES) {
            if cancel.is_cancelled() {
                return Err(RejectReason::Cancelled);
            }
            file.write_all(chunk).map_err(|_| RejectReason::IoFailure)?;
        }
        if cancel.is_cancelled() {
            return Err(RejectReason::Cancelled);
        }
        file.sync_all().map_err(|_| RejectReason::IoFailure)?;
        if cancel.is_cancelled() {
            return Err(RejectReason::Cancelled);
        }
        let stage_name = stage.name.clone();
        checkpoint(Checkpoint::Staged);
        validate_preimage(parent, name, mutation, permissions)?;
        validate_staged_content(parent, &stage_name, &mut file, identity, &mutation.after)
            .map_err(|check| check.reason(RejectReason::StagedSourceChanged))?;
        if cancel.is_cancelled() {
            return Err(RejectReason::Cancelled);
        }
        let commit_parent = self.revalidate_traversal()?;
        validate_target_entry(&commit_parent, name, mutation.targets.target_identity)
            .map_err(|check| check.reason(RejectReason::StalePreimage))?;
        validate_staged_identity(
            &commit_parent,
            &stage_name,
            &file,
            identity,
            mutation.after.len(),
        )
        .map_err(|check| check.reason(RejectReason::StagedSourceChanged))?;
        checkpoint(Checkpoint::Validated);
        if cancel.is_cancelled() {
            return Err(RejectReason::Cancelled);
        }
        if mutation.creates_file() {
            (self.create_new)(&commit_parent, &stage_name, name)?;
        } else {
            renameat(&commit_parent, &stage_name, &commit_parent, name)
                .map_err(|_| RejectReason::IoFailure)?;
        }
        self.stage = None;
        Ok(())
    }

    fn realize_traversal(&mut self) -> Result<OwnedFd, RejectReason> {
        let mutation = self.mutation;
        let mut current =
            open_anchor(mutation).map_err(|check| check.reason(RejectReason::TraversalChanged))?;
        let traversal = mutation
            .targets
            .traversal
            .iter()
            .zip(mutation.parent_components());
        for (index, (entry, component)) in traversal.enumerate() {
            let next = match entry {
                TraversalDirectory::Existing(expected) => {
                    let next = open_child_directory(&current, component)
                        .map_err(|error| Check::of(error).reason(RejectReason::TraversalChanged))?;
                    let identity =
                        descriptor_identity(&next).map_err(|_| RejectReason::IoFailure)?;
                    if identity != *expected {
                        return Err(RejectReason::TraversalChanged);
                    }
                    self.realized.push(identity);
                    next
                }
                TraversalDirectory::Create => {
                    let (next, identity) = create_directory(&current, component)?;
                    self.realized.push(identity);
                    self.created.push((index, identity));
                    next
                }
            };
            current = next;
        }
        Ok(current)
    }

    fn revalidate_traversal(&self) -> Result<OwnedFd, RejectReason> {
        let changed = |check: Check| check.reason(RejectReason::TraversalChanged);
        let mut current = open_anchor(self.mutation).map_err(changed)?;
        for (component, expected) in self.mutation.parent_components().iter().zip(&self.realized) {
            let next = open_child_directory(&current, component)
                .map_err(|error| changed(Check::of(error)))?;
            if descriptor_identity(&next).map_err(|_| RejectReason::IoFailure)? != *expected {
                return Err(RejectReason::TraversalChanged);
            }
            current = next;
        }
        Ok(current)
    }

    fn remove_stage(&mut self, parent: &OwnedFd) {
        let Some(Stage {
            name,
            identity: Some(identity),
        }) = self.stage.take()
        else {
            return;
        };
        if observed_staged_identity(parent, &name) == Some(identity) {
            let _ = unlinkat(parent, &name, AtFlags::empty());
        }
    }

    fn reject(&self, reason: RejectReason) -> Rejection {
        let residue = self
            .created
            .iter()
            .rev()
            .filter_map(|(index, identity)| self.remove_created_directory(*index, *identity))
            .collect();
        Rejection::new(reason, residue)
    }

    fn remove_created_directory(&self, index: usize, created: FileIdentity) -> Option<Residue> {
        let residue = |reason| {
            Some(Residue {
                path: self.mutation.component_path(index),
                reason,
            })
        };
        let changed = || residue(ResidueReason::IdentityChanged);
        let Ok(mut current) = open_anchor(self.mutation) else {
            return changed();
        };
        let components = self.mutation.parent_components();
        for (component, expected) in components[..index].iter().zip(&self.realized) {
            let Ok(next) = open_child_directory(&current, component) else {
                return changed();
            };
            if descriptor_identity(&next).ok() != Some(*expected) {
                return changed();
            }
            current = next;
        }
        let component = &components[index];
        let child = match open_child_directory(&current, component) {
            Err(PathError::FileNotFound) => return None,
            Err(_) => return changed(),
            Ok(child) => child,
        };
        if descriptor_identity(&child).ok() != Some(created) {
            return changed();
        }
        match unlinkat(&current, component, AtFlags::REMOVEDIR) {
            Ok(()) => None,
            Err(Errno::NOTEMPTY | Errno::EXIST) => residue(ResidueReason::NotEmpty),
            Err(_) => residue(ResidueReason::RemoveFailed),
        }
    }
}

fn open_anchor(mutation: &PreparedMutation) -> Result<OwnedFd, Check> {
    let anchor = open_directory(&mutation.targets.target.anchor).map_err(Check::of)?;
    match descriptor_identity(&anchor) {
        Ok(identity) if identity == mutation.targets.anchor_identity => Ok(anchor),
        Ok(_) => Err(Check::Changed),
        Err(_) => Err(Check::Failed),
    }
}

fn create_directory(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<(OwnedFd, FileIdentity), RejectReason> {
    mkdirat(parent, name, DEFAULT_DIRECTORY_MODE)
        .map_err(|errno| Check::of(errno_error(errno)).reason(RejectReason::TraversalChanged))?;
    let created = open_child_directory(parent, name).and_then(|directory| {
        let identity = descriptor_identity(&directory)?;
        Ok((directory, identity))
    });
    if created.is_err() {
        let _ = unlinkat(parent, name, AtFlags::REMOVEDIR);
    }
    created.map_err(|_| RejectReason::IoFailure)
}

fn rename_new(parent: &OwnedFd, stage: &OsStr, name: &OsStr) -> Result<(), RejectReason> {
    match renameat_with(parent, stage, parent, name, RenameFlags::NOREPLACE) {
        Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP | Errno::PERM) => {
            link_new(parent, stage, name)
        }
        result => exclusive(result),
    }
}

fn link_new(parent: &OwnedFd, stage: &OsStr, name: &OsStr) -> Result<(), RejectReason> {
    exclusive(linkat(parent, stage, parent, name, AtFlags::empty()))?;
    let _ = unlinkat(parent, stage, AtFlags::empty());
    Ok(())
}

fn exclusive(result: Result<(), Errno>) -> Result<(), RejectReason> {
    match result {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(RejectReason::StalePreimage),
        Err(_) => Err(RejectReason::IoFailure),
    }
}

fn validate_preimage(
    parent: &OwnedFd,
    name: &OsStr,
    mutation: &PreparedMutation,
    expected_permissions: Option<Mode>,
) -> Result<Option<Mode>, RejectReason> {
    let stale = RejectReason::StalePreimage;
    let Preimage::Present { content, hash } = &mutation.preimage else {
        return match entry_identity(parent, name) {
            Err(PathError::FileNotFound) => Ok(None),
            Err(error) => Err(Check::of(error).reason(stale)),
            Ok(_) => Err(stale),
        };
    };
    let descriptor = openat(parent, name, READ_FLAGS, Mode::empty())
        .map_err(|errno| Check::of(errno_error(errno)).reason(stale))?;
    let stat = fstat(&descriptor).map_err(|_| RejectReason::IoFailure)?;
    if !is_regular_stat(&stat) || usize::try_from(stat.st_size).ok() != Some(content.len()) {
        return Err(stale);
    }
    let identity = descriptor_identity(&descriptor).map_err(|_| RejectReason::IoFailure)?;
    if Some(identity) != mutation.targets.target_identity {
        return Err(stale);
    }
    let permissions = Mode::from_raw_mode(stat.st_mode);
    if !permissions.intersects(WRITE_BITS) {
        return Err(RejectReason::IoFailure);
    }
    if expected_permissions.is_some_and(|expected| expected != permissions) {
        return Err(stale);
    }
    let mut file = File::from(descriptor);
    if file_hash(&mut file).map_err(|_| RejectReason::IoFailure)? != *hash {
        return Err(stale);
    }
    validate_target_entry(parent, name, Some(identity)).map_err(|check| check.reason(stale))?;
    Ok(Some(permissions))
}

fn validate_target_entry(
    parent: &OwnedFd,
    name: &OsStr,
    expected: Option<FileIdentity>,
) -> Result<(), Check> {
    match (entry_identity(parent, name), expected) {
        (Err(PathError::FileNotFound), None) => Ok(()),
        (Err(error), _) => Err(Check::of(error)),
        (Ok(observed), Some(expected)) if is_regular(observed) && observed == expected => Ok(()),
        (Ok(_), _) => Err(Check::Changed),
    }
}

fn validate_staged_content(
    parent: &OwnedFd,
    name: &OsStr,
    file: &mut File,
    identity: FileIdentity,
    content: &[u8],
) -> Result<(), Check> {
    validate_staged_identity(parent, name, file, identity, content.len())?;
    file.seek(SeekFrom::Start(0)).map_err(|_| Check::Failed)?;
    if file_hash(file).map_err(|_| Check::Failed)? != Sha256::digest(content).to_vec() {
        return Err(Check::Changed);
    }
    Ok(())
}

fn validate_staged_identity(
    parent: &OwnedFd,
    name: &OsStr,
    file: &File,
    identity: FileIdentity,
    size: usize,
) -> Result<(), Check> {
    let stat = fstat(file).map_err(|_| Check::Failed)?;
    if !is_regular_stat(&stat)
        || stat.st_nlink != 1
        || usize::try_from(stat.st_size).ok() != Some(size)
    {
        return Err(Check::Changed);
    }
    if descriptor_identity(file).map_err(|_| Check::Failed)? != identity {
        return Err(Check::Changed);
    }
    if observed_staged_identity(parent, name) != Some(identity) {
        return Err(Check::Changed);
    }
    Ok(())
}

fn observed_staged_identity(parent: &OwnedFd, name: &OsStr) -> Option<FileIdentity> {
    let stat = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    if !is_regular_stat(&stat) || stat.st_nlink != 1 {
        return None;
    }
    entry_identity(parent, name).ok()
}

fn file_hash(file: &mut File) -> io::Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().to_vec());
        }
        hasher.update(&buffer[..read]);
    }
}

fn stage_name() -> Result<OsString, RejectReason> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| RejectReason::IoFailure)?;
    let name = random
        .iter()
        .fold(STAGE_PREFIX.to_owned(), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        });
    Ok(OsString::from(name))
}

fn is_regular_stat(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
}

fn errno_error(errno: Errno) -> PathError {
    io::Error::from(errno).into()
}

#[cfg(test)]
mod tests;

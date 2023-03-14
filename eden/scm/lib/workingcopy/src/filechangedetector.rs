/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fs::Metadata;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::anyhow;
use anyhow::Error;
use anyhow::Result;
use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use futures::StreamExt;
use manifest::Manifest;
use manifest_tree::TreeManifest;
use parking_lot::RwLock;
use pathmatcher::ExactMatcher;
use storemodel::minibytes::Bytes;
use storemodel::ReadFileContents;
use treestate::filestate::FileStateV2;
use treestate::filestate::StateFlags;
use types::Key;
use types::RepoPath;
use types::RepoPathBuf;
use vfs::is_executable;
use vfs::is_symlink;
use vfs::VFS;

use crate::filesystem::ChangeType;
use crate::walker::WalkError;

pub type ArcReadFileContents = Arc<dyn ReadFileContents<Error = anyhow::Error> + Send + Sync>;

/// Represents a file modification time in Mercurial, in seconds since the unix epoch.
#[derive(Clone, Copy, PartialEq)]
pub struct HgModifiedTime(u64);

impl From<u64> for HgModifiedTime {
    fn from(value: u64) -> Self {
        HgModifiedTime(value)
    }
}

impl From<u32> for HgModifiedTime {
    fn from(value: u32) -> Self {
        HgModifiedTime(value.into())
    }
}

impl TryFrom<SystemTime> for HgModifiedTime {
    type Error = Error;
    fn try_from(value: SystemTime) -> Result<Self> {
        Ok(value
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs()
            .into())
    }
}

impl TryFrom<i32> for HgModifiedTime {
    type Error = Error;
    fn try_from(value: i32) -> Result<Self> {
        Ok(HgModifiedTime(value.try_into()?))
    }
}

pub enum FileChangeResult {
    Yes(ChangeType),
    No,
    Maybe,
}

impl FileChangeResult {
    fn changed(path: RepoPathBuf) -> Self {
        Self::Yes(ChangeType::Changed(path))
    }

    fn deleted(path: RepoPathBuf) -> Self {
        Self::Yes(ChangeType::Deleted(path))
    }
}

#[derive(Debug)]
pub enum ResolvedFileChangeResult {
    Yes(ChangeType),
    No(RepoPathBuf),
}

pub trait FileChangeDetectorTrait: IntoIterator<Item = Result<ResolvedFileChangeResult>> {
    fn submit(&mut self, state: Option<FileStateV2>, path: &RepoPath);
}

pub struct FileChangeDetector {
    vfs: VFS,
    last_write: HgModifiedTime,
    results: Vec<Result<ResolvedFileChangeResult>>,
    lookups: Vec<RepoPathBuf>,
    manifest: Arc<RwLock<TreeManifest>>,
    store: ArcReadFileContents,
}

impl FileChangeDetector {
    pub fn new(
        vfs: VFS,
        last_write: HgModifiedTime,
        manifest: Arc<RwLock<TreeManifest>>,
        store: ArcReadFileContents,
    ) -> Self {
        FileChangeDetector {
            vfs,
            last_write,
            lookups: Vec::new(),
            results: Vec::new(),
            manifest,
            store,
        }
    }
}

const NEED_CHECK: StateFlags = StateFlags::NEED_CHECK;
const EXIST_P1: StateFlags = StateFlags::EXIST_P1;

pub fn file_changed_given_metadata(
    vfs: &VFS,
    path: &RepoPath,
    last_write: HgModifiedTime,
    metadata: Option<Metadata>,
    state: Option<FileStateV2>,
) -> Result<FileChangeResult> {
    // First handle when metadata is None (i.e. file doesn't exist).
    let (metadata, state) = match (metadata, state) {
        // File was untracked during crawl but no longer exists.
        (None, None) => return Ok(FileChangeResult::No),

        // File was not found but exists in P1: mark as deleted.
        (None, Some(state)) if state.state.intersects(EXIST_P1) => {
            return Ok(FileChangeResult::deleted(path.to_owned()));
        }

        // File doesn't exist, isn't in P1 but exists in treestate.
        // This can happen when watchman is tracking that this file needs
        // checking for example.
        (None, Some(_)) => return Ok(FileChangeResult::No),

        (Some(m), s) => (m, s),
    };

    // Don't check EXIST_P2. If file is only in P2 we want to report "changed"
    // even if its contents happen to match an untracked file on disk.
    let in_parent = matches!(&state, Some(s) if s.state.intersects(EXIST_P1));
    let is_trackable_file = metadata.is_file() || metadata.is_symlink();

    let state = match (in_parent, is_trackable_file) {
        // If the file is not valid (e.g. a directory or a weird file like
        // a fifo file) but exists in P1 (as a valid file at some previous
        // time) then we consider it now deleted.
        (true, false) => return Ok(FileChangeResult::deleted(path.to_owned())),
        // File not in parent and not trackable - skip it. We can get here if
        // the file was valid during the crawl but no longer is.
        (false, false) => return Ok(FileChangeResult::No),
        // File exists but is not in the treestate (untracked)
        (false, true) => return Ok(FileChangeResult::changed(path.to_owned())),
        (true, true) => state.unwrap(),
    };

    let flags = state.state;

    // If working copy file size or flags are different from what is in treestate, it has changed.
    // Note: state.size is i32 since Mercurial uses negative numbers to indicate special files.
    // A -1 indicates the file is either in a merge state or a lookup state.
    // A -2 indicates the file comes from the other parent (and may or may not exist in the
    // current parent).
    //
    // Regardless, if the size is negative, we'll do a lookup comparison since we can't
    // determine if the file has changed relative to p1. This logic is a mess and we should get
    // rid of all these negative numbers.
    let valid_size = state.size >= 0;
    if valid_size {
        let size_different = metadata.len() != state.size.try_into().unwrap_or(std::u64::MAX);
        let exec_different =
            vfs.supports_executables() && is_executable(&metadata) != state.is_executable();
        let symlink_different =
            vfs.supports_symlinks() && is_symlink(&metadata) != state.is_symlink();

        if size_different || exec_different || symlink_different {
            return Ok(FileChangeResult::changed(path.to_owned()));
        }
    }

    // If it's marked NEED_CHECK, we always need to do a lookup, regardless of the mtime.
    let needs_check = flags.intersects(NEED_CHECK) || !valid_size;
    if needs_check {
        return Ok(FileChangeResult::Maybe);
    }

    // If the mtime has changed or matches the last normal() write time, we need to compare the
    // file contents in the later Lookups phase.  mtime can be negative as well. A -1 indicates
    // the file is in a lookup state. Since a -1 will always cause the equality comparison
    // below to fail and force a lookup, the -1 is handled correctly without special casing. In
    // theory all -1 files should be marked NEED_CHECK above (I think).
    if state.mtime < 0 {
        return Ok(FileChangeResult::Maybe);
    }

    let state_mtime: Result<HgModifiedTime> = state.mtime.try_into();
    let state_mtime = state_mtime.map_err(|e| WalkError::InvalidMTime(path.to_owned(), e))?;
    let mtime: HgModifiedTime = metadata.modified()?.try_into()?;

    if mtime != state_mtime || mtime == last_write {
        return Ok(FileChangeResult::Maybe);
    }

    Ok(FileChangeResult::No)
}

impl FileChangeDetector {
    pub fn has_changed_with_fresh_metadata(
        &mut self,
        state: Option<FileStateV2>,
        path: &RepoPath,
        metadata: Option<Metadata>,
    ) -> Result<FileChangeResult> {
        let res = file_changed_given_metadata(&self.vfs, path, self.last_write, metadata, state);

        if matches!(res, Ok(FileChangeResult::Maybe)) {
            self.lookups.push(path.to_owned());
        }

        res
    }
}

impl FileChangeDetectorTrait for FileChangeDetector {
    fn submit(&mut self, state: Option<FileStateV2>, path: &RepoPath) {
        let metadata = match self.vfs.metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(e) => match e.downcast_ref::<std::io::Error>() {
                Some(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                _ => {
                    self.results.push(Err(e));
                    return;
                }
            },
        };

        match self.has_changed_with_fresh_metadata(state, path, metadata) {
            Ok(res) => match res {
                FileChangeResult::Yes(change) => {
                    self.results.push(Ok(ResolvedFileChangeResult::Yes(change)))
                }
                FileChangeResult::No => self
                    .results
                    .push(Ok(ResolvedFileChangeResult::No(path.to_owned()))),
                FileChangeResult::Maybe => self.lookups.push(path.to_owned()),
            },
            Err(err) => self.results.push(Err(err)),
        };
    }
}

impl IntoIterator for FileChangeDetector {
    type Item = Result<ResolvedFileChangeResult>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(mut self) -> Self::IntoIter {
        // First, get the keys for the paths from the current manifest.
        let matcher = ExactMatcher::new(self.lookups.iter(), self.vfs.case_sensitive());
        let keys = self
            .manifest
            .read()
            .files(matcher)
            .filter_map(|result| {
                let file = match result {
                    Ok(file) => file,
                    Err(e) => {
                        self.results.push(Err(e));
                        return None;
                    }
                };
                Some(Key::new(file.path, file.meta.hgid))
            })
            .collect::<Vec<_>>();

        // Then fetch the contents of each file and check it against the filesystem.
        // TODO: if the underlying stores gain the ability to do hash-based comparisons,
        // switch this to use that (rather than pulling down the entire contents of each
        // file).
        let vfs = self.vfs.clone();
        let comparisons = async_runtime::block_on(async {
            self.store
                .read_file_contents(keys)
                .await
                .map(|result| {
                    let (expected, key) = match result {
                        Ok(x) => x,
                        Err(e) => return Err(e),
                    };
                    let actual = match vfs.read(&key.path) {
                        Ok(x) => x,
                        Err(e) => match e.downcast_ref::<std::io::Error>() {
                            Some(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                return Ok(ResolvedFileChangeResult::Yes(ChangeType::Deleted(
                                    key.path,
                                )));
                            }
                            _ => return Err(e),
                        },
                    };
                    if expected == actual {
                        Ok(ResolvedFileChangeResult::No(key.path))
                    } else {
                        Ok(ResolvedFileChangeResult::Yes(ChangeType::Changed(key.path)))
                    }
                })
                .collect::<Vec<_>>()
                .await
        });
        self.results.extend(comparisons);
        self.results.into_iter()
    }
}

/// ParallelDetector uses a fixed number of worker threads to parallelize file
/// metadata checks and file content checks. The initial metadata checks are
/// parallelized, then the paths needing content check are collected into a big
/// list, fetched from the store, then there's a final parallelized step to
/// compared repo contents to disk contents.
///
/// ParallelDetector theoretically supports multiple submitters and consumers of
/// results, but note that results will not start flowing until all clones of a
/// ParallelDetector have been dropped (typically via conversion
/// to iterator).
#[derive(Clone)]
pub struct ParallelDetector {
    vfs: VFS,
    manifest: Arc<RwLock<TreeManifest>>,
    store: ArcReadFileContents,
    result_send: Sender<Result<ResolvedFileChangeResult>>,
    result_recv: Receiver<Result<ResolvedFileChangeResult>>,
    check_contents_recv: Receiver<RepoPathBuf>,
    // Store as an option so we can explicitly drop to disconnect the check_contents channel.
    check_metadata_send: Option<Sender<(RepoPathBuf, Option<FileStateV2>)>>,
    worker_count: usize,
}

// Regarding error handling, all errors should be propagated to the user via the
// result_send channel. The exception is that any channel send errors are
// instead silently propagated, causing threads to exit. If we fail to send, that
// means the receiver has been dropped, so it is appropriate to cancel
// everything.
impl ParallelDetector {
    pub fn new(
        vfs: VFS,
        last_write: HgModifiedTime,
        manifest: Arc<RwLock<TreeManifest>>,
        store: ArcReadFileContents,
        worker_count: usize,
    ) -> Self {
        // Channel to submit request for file's metadata to be checked against
        // treestate state. If the metadata check isn't conclusive, the path will be
        // forwarded for a full content check.
        let (check_metadata_send, check_metadata_recv) =
            crossbeam::channel::unbounded::<(RepoPathBuf, Option<FileStateV2>)>();

        // Channel to submit request to compare file's on-disk contents to repo's contents.
        let (check_contents_send, check_contents_recv) =
            crossbeam::channel::unbounded::<RepoPathBuf>();

        // Channel for the detector to relay results back to the caller.
        let (result_send, result_recv) =
            crossbeam::channel::unbounded::<Result<ResolvedFileChangeResult>>();

        // Spin up workers to handle file metadata checks. The user submits
        // these via submit(). These threads will naturally exit once
        // check_metadata_send is dropped (thus disconnecting the channel).
        for _ in 0..worker_count {
            let check_metadata_recv = check_metadata_recv.clone();
            let vfs = vfs.clone();
            let check_contents_send = check_contents_send.clone();
            let result_send = result_send.clone();
            std::thread::spawn(move || -> Result<()> {
                for (path, state) in check_metadata_recv {
                    Self::perform_metadata_check(
                        &vfs,
                        path,
                        last_write,
                        state,
                        &result_send,
                        &check_contents_send,
                    )?;
                }

                Ok(())
            });
        }

        Self {
            vfs,
            manifest,
            store,
            check_metadata_send: Some(check_metadata_send),
            result_send,
            result_recv,
            check_contents_recv,
            worker_count,
        }
    }

    // Read file bytes from disk and compare to the file's pristine repo bytes.
    fn compare_repo_bytes_to_disk(&self, repo_bytes: Bytes, path: RepoPathBuf) -> Result<()> {
        match self.vfs.read(&path) {
            Ok(disk_bytes) => {
                if disk_bytes == repo_bytes {
                    self.result_send
                        .send(Ok(ResolvedFileChangeResult::No(path)))?;
                } else {
                    self.result_send
                        .send(Ok(ResolvedFileChangeResult::Yes(ChangeType::Changed(path))))?;
                }
            }
            Err(e) => match e.downcast_ref::<std::io::Error>() {
                Some(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.result_send
                        .send(Ok(ResolvedFileChangeResult::Yes(ChangeType::Deleted(path))))?;
                }
                _ => {
                    self.result_send.send(Err(e))?;
                }
            },
        };

        Ok(())
    }

    // Fetch the repo contents for all files needing content checks and then
    // submit work to compare each file's repo contents with the on-disk
    // contents.
    fn fetch_repo_contents(&self, disk_send: Sender<(RepoPathBuf, Bytes)>) -> Result<()> {
        // Slurp up all the paths needing content checks. The ReadFileContents
        // trait is already batched, so let's keep things simple and just build
        // one big batch. The alternative is to perform our own batching so we
        // can start comparing content before finishing comparing metadata, but
        // that probably isn't worth the trouble.
        let mut lookups = Vec::new();
        for path in self.check_contents_recv.clone() {
            lookups.push(path);
        }

        let matcher = ExactMatcher::new(lookups.iter(), self.vfs.case_sensitive());
        let keys = self
            .manifest
            .read()
            .files(matcher)
            .filter_map(|result| match result {
                Ok(file) => Some(Ok(Key::new(file.path, file.meta.hgid))),
                Err(e) => match self.result_send.send(Err(e)) {
                    Ok(()) => None,
                    Err(err) => Some(Err(anyhow!(err))),
                },
            })
            .collect::<Result<Vec<_>>>()?;

        async_runtime::block_on(async {
            // TODO: if the underlying stores gain the ability to do hash-based comparisons,
            // switch this to use that (rather than pulling down the entire contents of each
            // file).
            let mut contents = self.store.read_file_contents(keys).await;

            while let Some(result) = contents.next().await {
                match result {
                    Ok((bytes, key)) => {
                        disk_send.send((key.path, bytes))?;
                    }
                    Err(e) => {
                        self.result_send.send(Err(e))?;
                    }
                };
            }

            Ok::<(), Error>(())
        })
    }

    // Perform comparison between a file's treestate state and on-disk metadata
    // (i.e. file size and modified time). If the comparison isn't conclusive,
    // submit a work unit for a full content check.
    fn perform_metadata_check(
        vfs: &VFS,
        path: RepoPathBuf,
        last_write: HgModifiedTime,
        state: Option<FileStateV2>,
        result_send: &Sender<Result<ResolvedFileChangeResult>>,
        lookup_send: &Sender<RepoPathBuf>,
    ) -> Result<()> {
        let metadata = match vfs.metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(e) => match e.downcast_ref::<std::io::Error>() {
                Some(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                _ => {
                    result_send.send(Err(e))?;
                    return Ok(());
                }
            },
        };

        match file_changed_given_metadata(vfs, &path, last_write, metadata, state) {
            Ok(res) => match res {
                FileChangeResult::Yes(change) => {
                    result_send.send(Ok(ResolvedFileChangeResult::Yes(change)))?;
                }
                FileChangeResult::No => {
                    result_send.send(Ok(ResolvedFileChangeResult::No(path)))?;
                }
                FileChangeResult::Maybe => {
                    lookup_send.send(path)?;
                }
            },
            Err(err) => result_send.send(Err(err))?,
        }

        Ok(())
    }
}

impl FileChangeDetectorTrait for ParallelDetector {
    fn submit(&mut self, state: Option<FileStateV2>, path: &RepoPath) {
        self.check_metadata_send
            .as_ref()
            .unwrap()
            .send((path.to_owned(), state))
            .unwrap();
    }
}

impl IntoIterator for ParallelDetector {
    type Item = Result<ResolvedFileChangeResult>;
    type IntoIter = crossbeam::channel::IntoIter<Self::Item>;

    fn into_iter(mut self) -> Self::IntoIter {
        // Drop the metadata channel. This is important since it disconnects the
        // channel, causing the recv worker threads to exit. We do this so we
        // can continue to use self as a convenience container object below.
        self.check_metadata_send.take();

        let result_iter = self.result_recv.clone().into_iter();

        std::thread::spawn(move || -> Result<()> {
            let (disk_send, disk_recv) = crossbeam::channel::unbounded::<(RepoPathBuf, Bytes)>();

            // Spin up worker threads to read file contents from disk and
            // compare to repo contents. Threads will naturally exit when
            // disk_send is dropped (i.e. repo contents are done being fetched).
            for _ in 0..self.worker_count {
                let detector = self.clone();
                let disk_recv = disk_recv.clone();
                std::thread::spawn(move || -> Result<()> {
                    for (path, repo_bytes) in disk_recv {
                        detector.compare_repo_bytes_to_disk(repo_bytes, path)?;
                    }
                    Ok(())
                });
            }

            self.fetch_repo_contents(disk_send)
        });

        result_iter
    }
}

//! Directory comparison: a two-tree merge walk, classified per entry.
//!
//! Deliberately no walker crate. `walkdir` does one-tree DFS, so interleaving two trees
//! means materialising and sorting both anyway; the recursive merge below produces the
//! parent/child/depth structure the tree view needs *as a byproduct*, in deterministic DFS
//! order, so the UI can push rows without sorting. `ignore` is rejected outright: gitignore
//! filtering in a diff tool would silently hide a differing file, which is a correctness
//! bug rather than a feature.
//!
//! The walk runs on a worker thread and reports over a channel. The forcing argument is not
//! the walk itself but the content comparison: any pair of equal-sized differing files is
//! read in full, so a synchronous scan would freeze the event loop with no way to press `q`.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use crate::model::{MAX_SIDES, SideIdx};
use crate::text::files_equal;

/// What kind of thing an entry is. Symlinks are their own kind because yadt does not
/// follow them by default, and a symlink facing a regular file is a type mismatch rather
/// than a content difference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl EntryKind {
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// One name inside a directory, on one side.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: OsString,
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryStatus {
    /// A directory whose subtree has not finished being classified.
    Pending,
    Identical,
    Differing,
    /// Present on exactly one side. Generalises left-only/right-only to N sides.
    OnlyIn(SideIdx),
    /// The two sides disagree about what this name *is*.
    TypeMismatch,
    /// Following symlinks led back to a directory already on the path. Reported rather
    /// than walked, so a cycle cannot spin the scan.
    Cycle,
    Unreadable(io::ErrorKind),
}

impl EntryStatus {
    pub const fn is_identical(self) -> bool {
        matches!(self, Self::Identical)
    }

    /// Should this entry be counted as a difference between the trees?
    pub const fn differs(self) -> bool {
        !matches!(self, Self::Identical | Self::Pending)
    }

    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "\u{b7}", // ·
            Self::Identical => " ",
            Self::Differing => "\u{25ac}",             // ▬
            Self::OnlyIn(SideIdx::LEFT) => "\u{25c0}", // ◀
            Self::OnlyIn(_) => "\u{25b6}",             // ▶
            Self::TypeMismatch => "\u{2260}",          // ≠
            Self::Cycle => "\u{21bb}",                 // ↻
            Self::Unreadable(_) => "?",
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Pending => "scanning".to_string(),
            Self::Identical => "identical".to_string(),
            Self::Differing => "differs".to_string(),
            Self::OnlyIn(SideIdx::LEFT) => "only on the left".to_string(),
            Self::OnlyIn(_) => "only on the right".to_string(),
            Self::TypeMismatch => "different kinds".to_string(),
            Self::Cycle => "symlink cycle".to_string(),
            Self::Unreadable(kind) => format!("unreadable: {kind:?}"),
        }
    }
}

/// One row of the directory tree.
#[derive(Clone, Debug)]
pub struct DirEntryRow {
    pub rel: PathBuf,
    pub depth: u16,
    /// What this entry is for tree purposes: `Dir` only when both sides agree it is one,
    /// so a type mismatch is never expandable.
    pub kind: EntryKind,
    /// What each side actually holds. A type mismatch is a directory on one side and a
    /// file on the other, and each pane has to say so.
    pub kinds: [Option<EntryKind>; MAX_SIDES],
    /// Which sides hold this name. `present` is the truth; `status` is the summary.
    pub present: [bool; MAX_SIDES],
    pub status: EntryStatus,
    pub sizes: [Option<u64>; MAX_SIDES],
}

impl DirEntryRow {
    /// What `side` holds here, if anything.
    pub fn kind_on(&self, side: SideIdx) -> Option<EntryKind> {
        self.kinds[side.idx()]
    }

    pub fn name(&self) -> String {
        self.rel.file_name().map_or_else(
            || self.rel.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }
}

/// What the scan thread reports.
#[derive(Debug)]
pub enum ScanEvent {
    Entry(Box<DirEntryRow>),
    /// Refine an already-emitted entry — how a directory learns its rolled-up status once
    /// its subtree is done.
    Update {
        rel: PathBuf,
        status: EntryStatus,
    },
    Progress {
        entries: usize,
    },
    Error {
        rel: PathBuf,
        message: String,
    },
    /// The walk is over. It carries no counts: the view tallies differences itself as
    /// entries and roll-up updates arrive, and two counters would drift.
    Done {
        cancelled: bool,
    },
}

/// A stable identity for a directory, used to break symlink cycles.
pub type DirId = (u64, u64);

/// Where a scan reads its entries from.
///
/// Exists so the merge can be tested without touching a filesystem: production uses
/// [`FsSource`], tests use an in-memory fake. Cheaper and more precise than fixture
/// directories.
pub trait DirSource: Send + Sync {
    /// Identity of the directory at `rel`, when the source can cheaply provide one.
    ///
    /// `None` means "cycles are impossible here", which is the case whenever symlinks are
    /// not followed -- the walk can then only descend real directories.
    fn dir_id(&self, _side: SideIdx, _rel: &Path) -> Option<DirId> {
        None
    }

    /// Entries directly under `rel` on `side`.
    fn list(&self, side: SideIdx, rel: &Path) -> io::Result<Vec<DirEntry>>;
    /// Are the regular files at `rel` byte-identical on both sides?
    fn content_eq(&self, rel: &Path) -> io::Result<bool>;
    /// Do the symlinks at `rel` point at the same target on both sides?
    fn link_eq(&self, rel: &Path) -> io::Result<bool>;
}

/// The real filesystem.
pub struct FsSource {
    pub roots: [PathBuf; 2],
    /// Off by default: following symlinks risks cycles, and a viewer should not traverse
    /// out of the tree it was pointed at. Note this differs from GNU `diff -r`.
    pub follow_symlinks: bool,
}

impl FsSource {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self {
            roots: [left, right],
            follow_symlinks: false,
        }
    }

    fn path(&self, side: SideIdx, rel: &Path) -> PathBuf {
        self.roots[side.idx()].join(rel)
    }
}

impl DirSource for FsSource {
    fn dir_id(&self, side: SideIdx, rel: &Path) -> Option<DirId> {
        // Without following, the walk only descends real directories, so no cycle exists
        // and the stat is wasted work.
        if !self.follow_symlinks {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(self.path(side, rel)).ok()?;
            Some((meta.dev(), meta.ino()))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    fn list(&self, side: SideIdx, rel: &Path) -> io::Result<Vec<DirEntry>> {
        let dir = self.path(side, rel);
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = if self.follow_symlinks {
                std::fs::metadata(entry.path())
            } else {
                entry.path().symlink_metadata()
            };
            let (kind, size) = match meta {
                Ok(m) if m.is_dir() => (EntryKind::Dir, 0),
                Ok(m) if m.is_file() => (EntryKind::File, m.len()),
                Ok(m) if m.file_type().is_symlink() => (EntryKind::Symlink, 0),
                Ok(_) => (EntryKind::Other, 0),
                // A name we cannot stat still exists; report it rather than dropping it.
                Err(_) => (EntryKind::Other, 0),
            };
            out.push(DirEntry {
                name: entry.file_name(),
                kind,
                size,
            });
        }
        Ok(out)
    }

    fn content_eq(&self, rel: &Path) -> io::Result<bool> {
        files_equal(
            &self.path(SideIdx::LEFT, rel),
            &self.path(SideIdx::RIGHT, rel),
        )
    }

    fn link_eq(&self, rel: &Path) -> io::Result<bool> {
        let l = std::fs::read_link(self.path(SideIdx::LEFT, rel))?;
        let r = std::fs::read_link(self.path(SideIdx::RIGHT, rel))?;
        Ok(l == r)
    }
}

/// Walk both trees together, emitting rows in DFS order.
///
/// Returns true when everything at or below `rel` is identical, which is how a directory
/// gets its rolled-up status.
///
/// `visited` holds the directory identities on the *current path*. Entries are removed on
/// the way back up, so this detects a true ancestor cycle without mistaking two symlinks
/// pointing at the same sibling directory for one.
pub fn merge(
    source: &dyn DirSource,
    rel: &Path,
    depth: u16,
    cancel: &AtomicBool,
    visited: &mut HashSet<DirId>,
    out: &mut dyn FnMut(ScanEvent),
) -> bool {
    if cancel.load(Ordering::Relaxed) {
        return true;
    }

    // Each level marks *itself* as being on the path, so callers can pass an empty set and
    // the guard still holds from the root down. Doing this at the recursion site instead
    // would leave the root unmarked, and a link straight back to it would be missed.
    let own: Vec<DirId> = [SideIdx::LEFT, SideIdx::RIGHT]
        .into_iter()
        .filter_map(|side| source.dir_id(side, rel))
        .collect();
    for id in &own {
        visited.insert(*id);
    }
    let all_identical = merge_level(source, rel, depth, cancel, visited, out);
    for id in &own {
        visited.remove(id);
    }
    all_identical
}

fn merge_level(
    source: &dyn DirSource,
    rel: &Path,
    depth: u16,
    cancel: &AtomicBool,
    visited: &mut HashSet<DirId>,
    out: &mut dyn FnMut(ScanEvent),
) -> bool {
    // Collect both sides into sorted maps, then walk their union once. BTreeMap keyed by
    // the raw bytes gives a deterministic order that does not depend on locale.
    let mut sides: [BTreeMap<Vec<u8>, DirEntry>; 2] = [BTreeMap::new(), BTreeMap::new()];
    for (i, side) in [SideIdx::LEFT, SideIdx::RIGHT].into_iter().enumerate() {
        match source.list(side, rel) {
            Ok(entries) => {
                for entry in entries {
                    sides[i].insert(entry.name.as_encoded_bytes().to_vec(), entry);
                }
            }
            Err(e) => out(ScanEvent::Error {
                rel: rel.to_path_buf(),
                message: format!("{}: {e}", rel.display()),
            }),
        }
    }

    let mut names: Vec<&Vec<u8>> = sides[0].keys().chain(sides[1].keys()).collect();
    names.sort_unstable();
    names.dedup();
    let names: Vec<Vec<u8>> = names.into_iter().cloned().collect();

    let mut all_identical = true;

    for name in names {
        if cancel.load(Ordering::Relaxed) {
            return all_identical;
        }
        let left = sides[0].get(&name);
        let right = sides[1].get(&name);
        let entry = left.or(right).expect("name came from one of the maps");
        let child = rel.join(entry.name.clone());

        let present = [left.is_some(), right.is_some(), false];
        let kinds = [left.map(|e| e.kind), right.map(|e| e.kind), None];
        let sizes = [left.map(|e| e.size), right.map(|e| e.size), None];

        let (kind, status) = match (left, right) {
            // Present on one side only. The directory is not enumerated: a one-sided
            // node_modules/ must not cost 50k stats to tell you it is missing.
            (Some(e), None) => (e.kind, EntryStatus::OnlyIn(SideIdx::LEFT)),
            (None, Some(e)) => (e.kind, EntryStatus::OnlyIn(SideIdx::RIGHT)),
            // Not `l.kind`: the sides disagree, so there is no single kind, and calling it a
            // directory would make it expandable with nothing beneath it.
            (Some(l), Some(r)) if l.kind != r.kind => (EntryKind::Other, EntryStatus::TypeMismatch),
            (Some(l), Some(_)) => match l.kind {
                EntryKind::Dir => (EntryKind::Dir, EntryStatus::Pending),
                EntryKind::File => (
                    EntryKind::File,
                    match source.content_eq(&child) {
                        Ok(true) => EntryStatus::Identical,
                        Ok(false) => EntryStatus::Differing,
                        Err(e) => EntryStatus::Unreadable(e.kind()),
                    },
                ),
                EntryKind::Symlink => (
                    EntryKind::Symlink,
                    match source.link_eq(&child) {
                        Ok(true) => EntryStatus::Identical,
                        Ok(false) => EntryStatus::Differing,
                        Err(e) => EntryStatus::Unreadable(e.kind()),
                    },
                ),
                EntryKind::Other => (
                    EntryKind::Other,
                    EntryStatus::Unreadable(io::ErrorKind::Unsupported),
                ),
            },
            (None, None) => unreachable!("name came from one of the maps"),
        };

        // A directory we would descend into, but which is already on the path, is a cycle:
        // report it and stop, rather than following a symlink back up the tree.
        let ids: Vec<DirId> = if status == EntryStatus::Pending {
            [SideIdx::LEFT, SideIdx::RIGHT]
                .into_iter()
                .filter_map(|side| source.dir_id(side, &child))
                .collect()
        } else {
            Vec::new()
        };
        let is_cycle = ids.iter().any(|id| visited.contains(id));
        // A cycle has no children to show, so it must not render as expandable. `kinds`
        // still says Dir on both sides, so each pane keeps its trailing slash.
        let (kind, status) = if is_cycle {
            (EntryKind::Other, EntryStatus::Cycle)
        } else {
            (kind, status)
        };

        out(ScanEvent::Entry(Box::new(DirEntryRow {
            rel: child.clone(),
            depth,
            kind,
            kinds,
            present,
            status,
            sizes,
        })));

        if status.differs() {
            all_identical = false;
        }

        // Recurse into directories present on both sides, then tell the UI what the
        // subtree turned out to be.
        if status == EntryStatus::Pending {
            let subtree_identical = merge(source, &child, depth + 1, cancel, visited, out);
            let rolled = if subtree_identical {
                EntryStatus::Identical
            } else {
                EntryStatus::Differing
            };
            out(ScanEvent::Update {
                rel: child,
                status: rolled,
            });
            if !subtree_identical {
                all_identical = false;
            }
        }
    }

    all_identical
}

/// A running scan.
pub struct ScanHandle {
    pub rx: Receiver<ScanEvent>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        // The walk checks the flag per entry, so this returns promptly rather than
        // blocking on a large tree.
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Start scanning two directory trees on a worker thread.
pub fn spawn_scan(source: Box<dyn DirSource>) -> ScanHandle {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    let handle = std::thread::spawn(move || {
        run_scan(source.as_ref(), &worker_cancel, &tx);
    });

    ScanHandle {
        rx,
        cancel,
        handle: Some(handle),
    }
}

fn run_scan(source: &dyn DirSource, cancel: &AtomicBool, tx: &Sender<ScanEvent>) {
    let mut entries = 0usize;
    let mut send_failed = false;

    {
        let mut emit = |event: ScanEvent| {
            if send_failed {
                return;
            }
            if matches!(event, ScanEvent::Entry(_)) {
                entries += 1;
            }
            if tx.send(event).is_err() {
                // The UI is gone; stop walking rather than filling a dead channel.
                send_failed = true;
                cancel.store(true, Ordering::Relaxed);
                return;
            }
            if entries.is_multiple_of(256) && tx.send(ScanEvent::Progress { entries }).is_err() {
                send_failed = true;
                cancel.store(true, Ordering::Relaxed);
            }
        };

        merge(
            source,
            Path::new(""),
            0,
            cancel,
            &mut HashSet::new(),
            &mut emit,
        );
    }

    let _ = tx.send(ScanEvent::Done {
        cancelled: cancel.load(Ordering::Relaxed),
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// An in-memory pair of trees. Paths are `/`-joined relative strings; a value of
    /// `None` means "directory", `Some(content)` means "file with this content".
    #[derive(Default)]
    struct FakeSource {
        left: HashMap<String, Option<String>>,
        right: HashMap<String, Option<String>>,
        links: HashMap<String, (String, String)>,
    }

    impl FakeSource {
        fn dir(mut self, side: SideIdx, path: &str) -> Self {
            self.tree_mut(side).insert(path.to_string(), None);
            self
        }

        fn file(mut self, side: SideIdx, path: &str, body: &str) -> Self {
            self.tree_mut(side)
                .insert(path.to_string(), Some(body.to_string()));
            self
        }

        /// Add the same file to both sides with identical content.
        fn both(self, path: &str, body: &str) -> Self {
            self.file(SideIdx::LEFT, path, body)
                .file(SideIdx::RIGHT, path, body)
        }

        fn both_dir(self, path: &str) -> Self {
            self.dir(SideIdx::LEFT, path).dir(SideIdx::RIGHT, path)
        }

        fn tree_mut(&mut self, side: SideIdx) -> &mut HashMap<String, Option<String>> {
            if side == SideIdx::LEFT {
                &mut self.left
            } else {
                &mut self.right
            }
        }

        fn tree(&self, side: SideIdx) -> &HashMap<String, Option<String>> {
            if side == SideIdx::LEFT {
                &self.left
            } else {
                &self.right
            }
        }
    }

    impl DirSource for FakeSource {
        fn list(&self, side: SideIdx, rel: &Path) -> io::Result<Vec<DirEntry>> {
            let prefix = rel.to_string_lossy().replace('\\', "/");
            let prefix = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            };
            let mut out = Vec::new();
            for (path, body) in self.tree(side) {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    continue;
                };
                // Direct children only.
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                let kind = if self.links.contains_key(path) {
                    EntryKind::Symlink
                } else if body.is_some() {
                    EntryKind::File
                } else {
                    EntryKind::Dir
                };
                out.push(DirEntry {
                    name: OsString::from(rest),
                    kind,
                    size: body.as_ref().map_or(0, |b| b.len() as u64),
                });
            }
            Ok(out)
        }

        fn content_eq(&self, rel: &Path) -> io::Result<bool> {
            let key = rel.to_string_lossy().to_string();
            Ok(self.left.get(&key) == self.right.get(&key))
        }

        fn link_eq(&self, rel: &Path) -> io::Result<bool> {
            let key = rel.to_string_lossy().to_string();
            Ok(self.links.get(&key).is_some_and(|(l, r)| l == r))
        }
    }

    /// Run a merge and render the result as an assertable table.
    fn scan(source: &dyn DirSource) -> String {
        let cancel = AtomicBool::new(false);
        let mut rows: Vec<DirEntryRow> = Vec::new();
        let mut updates: Vec<(PathBuf, EntryStatus)> = Vec::new();
        merge(
            source,
            Path::new(""),
            0,
            &cancel,
            &mut HashSet::new(),
            &mut |event| match event {
                ScanEvent::Entry(row) => rows.push(*row),
                ScanEvent::Update { rel, status } => updates.push((rel, status)),
                _ => {}
            },
        );
        // Apply roll-ups, exactly as the view does.
        for (rel, status) in updates {
            if let Some(row) = rows.iter_mut().find(|r| r.rel == rel) {
                row.status = status;
            }
        }

        rows.iter()
            .map(|r| {
                format!(
                    "{}{}{} {}\n",
                    "  ".repeat(r.depth as usize),
                    r.rel.display(),
                    if r.kind.is_dir() { "/" } else { "" },
                    r.status.describe()
                )
            })
            .collect()
    }

    #[test]
    fn identical_trees_report_everything_identical() {
        let src = FakeSource::default().both("a.txt", "x").both("b.txt", "y");
        assert_eq!(scan(&src), "a.txt identical\nb.txt identical\n");
    }

    #[test]
    fn differing_content_is_detected() {
        let src = FakeSource::default()
            .file(SideIdx::LEFT, "a.txt", "one")
            .file(SideIdx::RIGHT, "a.txt", "two");
        assert_eq!(scan(&src), "a.txt differs\n");
    }

    #[test]
    fn one_sided_entries_report_which_side() {
        let src = FakeSource::default()
            .file(SideIdx::LEFT, "only_left.txt", "x")
            .file(SideIdx::RIGHT, "only_right.txt", "y");
        assert_eq!(
            scan(&src),
            "only_left.txt only on the left\nonly_right.txt only on the right\n"
        );
    }

    #[test]
    fn entries_come_out_in_deterministic_order() {
        let src = FakeSource::default()
            .both("zebra", "z")
            .both("alpha", "a")
            .both("middle", "m");
        assert_eq!(
            scan(&src),
            "alpha identical\nmiddle identical\nzebra identical\n"
        );
    }

    #[test]
    fn a_file_facing_a_directory_is_a_type_mismatch() {
        let src = FakeSource::default()
            .file(SideIdx::LEFT, "thing", "x")
            .dir(SideIdx::RIGHT, "thing");
        assert_eq!(scan(&src), "thing different kinds\n");
    }

    #[test]
    fn directories_roll_up_from_their_children() {
        let src = FakeSource::default()
            .both_dir("same")
            .both("same/a.txt", "x")
            .both_dir("changed")
            .file(SideIdx::LEFT, "changed/b.txt", "one")
            .file(SideIdx::RIGHT, "changed/b.txt", "two");

        assert_eq!(
            scan(&src),
            "changed/ differs\n  changed/b.txt differs\nsame/ identical\n  same/a.txt identical\n"
        );
    }

    #[test]
    fn a_deep_difference_rolls_all_the_way_up() {
        let src = FakeSource::default()
            .both_dir("a")
            .both_dir("a/b")
            .both_dir("a/b/c")
            .file(SideIdx::LEFT, "a/b/c/deep.txt", "one")
            .file(SideIdx::RIGHT, "a/b/c/deep.txt", "two");

        let out = scan(&src);
        assert!(out.starts_with("a/ differs\n"), "{out}");
        assert!(out.contains("  a/b/ differs\n"), "{out}");
        assert!(out.contains("    a/b/c/ differs\n"), "{out}");
    }

    #[test]
    fn a_one_sided_directory_is_not_enumerated() {
        let src = FakeSource::default()
            .dir(SideIdx::LEFT, "gone")
            .file(SideIdx::LEFT, "gone/inside.txt", "x")
            .both("kept.txt", "y");

        // "gone" is reported, but walking into it is deliberately skipped -- a one-sided
        // node_modules/ must not cost thousands of stats.
        assert_eq!(scan(&src), "gone/ only on the left\nkept.txt identical\n");
    }

    #[test]
    fn an_empty_pair_of_trees_produces_nothing() {
        assert_eq!(scan(&FakeSource::default()), "");
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let src = FakeSource::default()
            .both("a", "1")
            .both("b", "2")
            .both("c", "3");
        let cancel = AtomicBool::new(true);
        let mut count = 0;
        merge(
            &src,
            Path::new(""),
            0,
            &cancel,
            &mut HashSet::new(),
            &mut |_| count += 1,
        );
        assert_eq!(count, 0, "a pre-cancelled scan should emit nothing");
    }
}

/// Symlink-cycle tests. These use the real filesystem on purpose: the bug they guard
/// against lives in `FsSource::dir_id`, which the in-memory fake cannot exercise.
#[cfg(all(test, unix))]
mod cycle_tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    /// A scratch tree that removes itself. Keyed by name so parallel tests do not collide.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("yadt-cycle-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `root/side/inner/back` is a symlink pointing back at `root/side`.
    fn looping_tree(root: &Path, side: &str) -> PathBuf {
        let base = root.join(side);
        std::fs::create_dir_all(base.join("inner")).unwrap();
        std::fs::write(base.join("inner/file.txt"), b"x\n").unwrap();
        // The link sits in `base/inner`, so `..` is `base` -- a true loop back up the path.
        std::os::unix::fs::symlink("..", base.join("inner/back")).unwrap();
        base
    }

    fn scan_with(follow: bool, left: PathBuf, right: PathBuf) -> Vec<DirEntryRow> {
        let mut source = FsSource::new(left, right);
        source.follow_symlinks = follow;
        let cancel = AtomicBool::new(false);
        let mut rows = Vec::new();
        merge(
            &source,
            Path::new(""),
            0,
            &cancel,
            &mut HashSet::new(),
            &mut |event| {
                if let ScanEvent::Entry(row) = event {
                    rows.push(*row);
                }
            },
        );
        rows
    }

    #[test]
    fn following_symlinks_reports_a_cycle_instead_of_walking_it() {
        let scratch = Scratch::new("follow");
        let left = looping_tree(scratch.path(), "l");
        let right = looping_tree(scratch.path(), "r");

        let rows = scan_with(true, left, right);

        // inner/, inner/file.txt, inner/back -- and nothing beyond the loop.
        assert_eq!(
            rows.len(),
            3,
            "a cycle must not multiply entries: {:?}",
            rows.iter()
                .map(|r| r.rel.display().to_string())
                .collect::<Vec<_>>()
        );

        let back = rows
            .iter()
            .find(|r| r.rel.ends_with("back"))
            .expect("the looping link should be reported");
        assert_eq!(back.status, EntryStatus::Cycle);
        // Not expandable -- there is nothing beneath it -- but each side still knows it is
        // a directory, so the panes render it with a trailing slash.
        assert_eq!(back.kind, EntryKind::Other);
        assert_eq!(back.kind_on(SideIdx::LEFT), Some(EntryKind::Dir));

        // Nothing below the link was walked.
        assert!(
            !rows
                .iter()
                .any(|r| r.rel.to_string_lossy().contains("back/")),
            "the walk descended into the cycle"
        );
    }

    #[test]
    fn not_following_symlinks_never_descends_and_compares_targets() {
        let scratch = Scratch::new("nofollow");
        let left = looping_tree(scratch.path(), "l");
        let right = looping_tree(scratch.path(), "r");

        let rows = scan_with(false, left, right);

        assert_eq!(rows.len(), 3);
        let back = rows.iter().find(|r| r.rel.ends_with("back")).unwrap();
        // Both links point at the same relative target, so they compare equal -- and the
        // entry is a symlink, not a directory.
        assert_eq!(back.kind, EntryKind::Symlink);
        assert_eq!(back.status, EntryStatus::Identical);
    }

    #[test]
    fn two_links_to_the_same_sibling_are_not_a_cycle() {
        // The visited set is per-path, not global: a directory reachable twice by
        // different routes must be walked both times.
        let scratch = Scratch::new("sibling");
        for side in ["l", "r"] {
            let base = scratch.path().join(side);
            std::fs::create_dir_all(base.join("target")).unwrap();
            std::fs::write(base.join("target/f.txt"), b"x\n").unwrap();
            std::os::unix::fs::symlink("target", base.join("one")).unwrap();
            std::os::unix::fs::symlink("target", base.join("two")).unwrap();
        }

        let rows = scan_with(true, scratch.path().join("l"), scratch.path().join("r"));

        assert!(
            rows.iter().all(|r| r.status != EntryStatus::Cycle),
            "a sibling link is not a cycle: {:?}",
            rows.iter()
                .map(|r| (r.rel.display().to_string(), r.status))
                .collect::<Vec<_>>()
        );
        // Both links were followed and their contents compared.
        assert!(rows.iter().any(|r| r.rel.ends_with("one/f.txt")));
        assert!(rows.iter().any(|r| r.rel.ends_with("two/f.txt")));
    }
}

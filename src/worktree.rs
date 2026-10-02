//! Operations that connect the work tree, the index and the commits: staging files, writing
//! trees, status, committing, and switching the work tree to another commit.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::err;
use crate::error::Result;
use crate::ignore::{IgnoreFile, Matcher};
use crate::index::{Entry, Index};
use crate::object::{object_id, Commit, Kind, Tree, TreeEntry, MODE_DIR, MODE_EXEC, MODE_FILE, MODE_LINK};
use crate::repo::{path_is_safe, Repo};
use crate::sha1::Oid;

/// Resolves a path typed relative to `cwd` (itself relative to the repository root) into a
/// repository-relative path with `/` separators. `.` is the root, which is the empty string.
pub fn normalize_pathspec(cwd: &str, arg: &str) -> Result<String> {
    let arg = arg.replace('\\', "/");
    let mut parts: Vec<&str> = if arg.starts_with('/') { vec![] } else { cwd.split('/').filter(|s| !s.is_empty()).collect() };
    for c in arg.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(err!("{arg:?} is outside the repository"));
                }
            }
            c => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}

fn secs_nanos(t: std::time::SystemTime) -> (u32, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as u32, d.subsec_nanos())
}

/// Stat data of a work tree file as stored in the index.
fn stat_fields(meta: &fs::Metadata) -> ((u32, u32), (u32, u32), u32) {
    let mtime = meta.modified().map(secs_nanos).unwrap_or((0, 0));
    let ctime = meta.created().map(secs_nanos).unwrap_or(mtime);
    (ctime, mtime, meta.len().min(u32::MAX as u64) as u32)
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

/// A file as found in the work tree.
struct Found {
    data: Vec<u8>,
    mode: u32,
    meta: fs::Metadata,
}

impl Repo {
    fn abs(&self, path: &str) -> std::path::PathBuf {
        self.work.join(path)
    }

    /// Reads a work tree file the way it would be committed. `known_mode` is the mode the index
    /// already has: the executable bit cannot be read on every platform, so it is kept.
    fn read_work_file(&self, path: &str, known_mode: Option<u32>) -> Result<Option<Found>> {
        let abs = self.abs(path);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(None),
            Err(e) => return Err(err!("{path}: {e}")),
        };
        if meta.is_dir() {
            return Ok(None);
        }
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&abs).map_err(|e| err!("{path}: {e}"))?;
            return Ok(Some(Found { data: target.to_string_lossy().replace('\\', "/").into_bytes(), mode: MODE_LINK, meta }));
        }
        let data = fs::read(&abs).map_err(|e| err!("{path}: {e}"))?;
        let mode = match known_mode {
            Some(MODE_LINK) => MODE_LINK,
            Some(m) if cfg!(not(unix)) => m,
            _ if is_executable(&meta) => MODE_EXEC,
            _ => MODE_FILE,
        };
        Ok(Some(Found { data, mode, meta }))
    }

    // ---------------------------------------------------------------- ignore rules

    /// The ignore rules that apply to paths inside `dir` (relative, `""` for the root).
    pub fn matcher_for(&self, dir: &str) -> Matcher {
        let ignore_case = self.config().get_bool("core.ignorecase").unwrap_or(false);
        let mut files = Vec::new();
        if let Ok(text) = fs::read_to_string(self.git.join("info").join("exclude")) {
            files.push(IgnoreFile::parse("", &text));
        }
        let mut cur = String::new();
        let mut parts = vec![""];
        parts.extend(dir.split('/').filter(|s| !s.is_empty()));
        for p in parts {
            if !p.is_empty() {
                if !cur.is_empty() {
                    cur.push('/');
                }
                cur.push_str(p);
            }
            if let Ok(text) = fs::read_to_string(self.abs(&cur).join(".gitignore")) {
                files.push(IgnoreFile::parse(&cur, &text));
            }
        }
        Matcher { files, ignore_case }
    }

    // ---------------------------------------------------------------- staging

    /// Hashes and stores one work tree file. Returns `Unchanged` without reading it when the stat
    /// data of the existing index entry still matches.
    fn stage_one(&self, path: &str, known: Option<&Entry>, index_written: (u32, u32)) -> Result<Staged> {
        if let Some(k) = known {
            if k.stage == 0 && self.work_matches(k, index_written)? == WorkState::Same {
                return Ok(Staged::Unchanged);
            }
        }
        let Some(found) = self.read_work_file(path, known.map(|e| e.mode))? else {
            return Err(err!("{path}: not a file"));
        };
        let (ctime, mtime, size) = stat_fields(&found.meta);
        let oid = self.odb.write(Kind::Blob, &found.data)?;
        let mut e = Entry::new(path, found.mode, oid);
        e.ctime = ctime;
        e.mtime = mtime;
        e.size = size;
        let changed = !known.is_some_and(|k| k.oid == oid && k.mode == found.mode);
        Ok(Staged::Entry(e, changed))
    }

    /// Stages many files, hashing and storing them on several threads (writing a loose object is
    /// mostly waiting for the file system). Returns the paths whose staged content changed.
    fn stage_many(&self, index: &mut Index, paths: &[String]) -> Result<Vec<String>> {
        for p in paths {
            if !path_is_safe(p) {
                return Err(err!("refusing to add {p:?}: not a safe path"));
            }
        }
        let known: Vec<Option<Entry>> = paths.iter().map(|p| index.get(p).cloned()).collect();
        let written = index.written;
        let threads =
            if paths.len() < 16 { 1 } else { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16) };
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: std::sync::Mutex<Vec<(usize, Result<Staged>)>> = std::sync::Mutex::new(Vec::with_capacity(paths.len()));
        let work = || loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i >= paths.len() {
                break;
            }
            let r = self.stage_one(&paths[i], known[i].as_ref(), written);
            results.lock().unwrap_or_else(|e| e.into_inner()).push((i, r));
        };
        if threads == 1 {
            work();
        } else {
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(work);
                }
            });
        }
        let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
        results.sort_by_key(|(i, _)| *i);
        let mut changed = Vec::new();
        for (i, r) in results {
            if let Staged::Entry(e, was_changed) = r? {
                index.add(e);
                if was_changed {
                    changed.push(paths[i].clone());
                }
            }
        }
        Ok(changed)
    }

    /// Stages paths (files, or directories with everything below them). A path that no longer
    /// exists is removed from the index. Ignored files are skipped unless named exactly with `force`.
    pub fn add_paths(&self, index: &mut Index, specs: &[String], force: bool) -> Result<AddReport> {
        let mut report = AddReport::default();
        for spec in specs {
            let abs = self.abs(spec);
            let meta = fs::symlink_metadata(&abs).ok();
            match meta {
                Some(m) if m.is_dir() => {
                    let mut seen = BTreeSet::new();
                    let mut found = Vec::new();
                    self.collect_dir(index, spec, self.matcher_for(spec), &mut seen, &mut report, force, &mut found)?;
                    report.added.extend(self.stage_many(index, &found)?);
                    // Tracked files below this directory that are gone from disk.
                    let prefix = if spec.is_empty() { String::new() } else { format!("{spec}/") };
                    let gone: Vec<String> = index
                        .entries()
                        .filter(|e| e.path.starts_with(&prefix) && !seen.contains(&e.path))
                        .map(|e| e.path.clone())
                        .collect();
                    for p in gone {
                        if !abs.join(p.strip_prefix(&prefix).unwrap_or(&p)).exists() {
                            index.remove(&p);
                            report.removed.push(p);
                        }
                    }
                }
                Some(_) => {
                    let matcher = self.matcher_for(spec.rsplit_once('/').map(|(d, _)| d).unwrap_or(""));
                    if !force && index.get(spec).is_none() && matcher.is_ignored(spec, false) {
                        report.ignored.push(spec.clone());
                        continue;
                    }
                    report.added.extend(self.stage_many(index, std::slice::from_ref(spec))?);
                }
                None => {
                    let prefix = format!("{spec}/");
                    let doomed: Vec<String> = index
                        .entries()
                        .filter(|e| e.path == *spec || e.path.starts_with(&prefix))
                        .map(|e| e.path.clone())
                        .collect();
                    if doomed.is_empty() {
                        return Err(err!("pathspec {spec:?} did not match any files"));
                    }
                    for p in doomed {
                        index.remove(&p);
                        report.removed.push(p);
                    }
                }
            }
        }
        Ok(report)
    }

    /// The rules for a subdirectory: the parent's rules plus the directory's own `.gitignore`.
    fn extend_matcher(&self, parent: &Matcher, dir: &str) -> Matcher {
        let mut m = parent.clone();
        if let Ok(text) = fs::read_to_string(self.abs(dir).join(".gitignore")) {
            m.files.push(IgnoreFile::parse(dir, &text));
        }
        m
    }

    /// Lists the files below `dir` that `add` should stage. `matcher` already includes the
    /// directory's own `.gitignore`.
    #[allow(clippy::too_many_arguments)]
    fn collect_dir(
        &self,
        index: &Index,
        dir: &str,
        matcher: Matcher,
        seen: &mut BTreeSet<String>,
        report: &mut AddReport,
        force: bool,
        out: &mut Vec<String>,
    ) -> Result<()> {
        let mut names: Vec<(String, bool)> = Vec::new();
        for e in fs::read_dir(self.abs(dir))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let ft = e.file_type()?;
            names.push((name, ft.is_dir()));
        }
        names.sort();
        for (name, is_dir) in names {
            if name == ".git" {
                continue;
            }
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            let tracked = index.get(&rel).is_some();
            if is_dir {
                if self.abs(&rel).join(".git").exists() {
                    report.skipped.push(format!("{rel}/ (a repository inside this one)"));
                    continue;
                }
                if !force && matcher.is_ignored(&rel, true) {
                    continue;
                }
                let child = self.extend_matcher(&matcher, &rel);
                self.collect_dir(index, &rel, child, seen, report, force, out)?;
            } else {
                if !force && !tracked && matcher.is_ignored(&rel, false) {
                    continue;
                }
                seen.insert(rel.clone());
                out.push(rel);
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- trees

    /// Writes the trees for the index and returns the root tree's id.
    pub fn write_tree(&self, index: &Index) -> Result<Oid> {
        if index.has_conflicts() {
            return Err(err!("the index has unresolved conflicts: {}", index.conflicted_paths().join(", ")));
        }
        #[derive(Default)]
        struct Dir {
            files: BTreeMap<String, (u32, Oid)>,
            dirs: BTreeMap<String, Dir>,
        }
        let mut root = Dir::default();
        for e in index.entries() {
            let mut cur = &mut root;
            let mut parts = e.path.split('/').peekable();
            while let Some(p) = parts.next() {
                if parts.peek().is_some() {
                    cur = cur.dirs.entry(p.to_string()).or_default();
                } else {
                    cur.files.insert(p.to_string(), (e.mode, e.oid));
                }
            }
        }
        fn write(repo: &Repo, d: &Dir) -> Result<Oid> {
            let mut entries: Vec<TreeEntry> =
                d.files.iter().map(|(n, (m, o))| TreeEntry { mode: *m, name: n.clone(), oid: *o }).collect();
            for (n, sub) in &d.dirs {
                entries.push(TreeEntry { mode: MODE_DIR, name: n.clone(), oid: write(repo, sub)? });
            }
            repo.odb.write(Kind::Tree, &Tree { entries }.serialize())
        }
        write(self, &root)
    }

    // ---------------------------------------------------------------- status

    /// Whether the work tree file for an index entry still has the entry's content.
    pub(crate) fn work_matches(&self, e: &Entry, index_written: (u32, u32)) -> Result<WorkState> {
        let abs = self.abs(&e.path);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(_) => return Ok(WorkState::Missing),
        };
        if meta.is_dir() {
            return Ok(WorkState::Missing);
        }
        let (_, mtime, size) = stat_fields(&meta);
        // The stat data is trusted unless the file could have changed in the same clock tick as
        // the index was written ("racily clean").
        let racy = mtime >= index_written;
        if !racy && e.size == size && e.mtime == mtime && e.mtime != (0, 0) {
            return Ok(WorkState::Same);
        }
        let Some(found) = self.read_work_file(&e.path, Some(e.mode))? else { return Ok(WorkState::Missing) };
        Ok(if object_id(Kind::Blob, &found.data) == e.oid && found.mode == e.mode { WorkState::Same } else { WorkState::Changed })
    }

    /// Compares every stage 0 index entry with the work tree, on several threads: for a clean
    /// tree this is one stat call per file, which is mostly waiting for the file system.
    fn work_states(&self, index: &Index) -> Result<std::collections::HashMap<String, WorkState>> {
        let entries: Vec<&Entry> = index.entries().filter(|e| e.stage == 0).collect();
        let threads =
            if entries.len() < 256 { 1 } else { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16) };
        let chunk = entries.len().div_ceil(threads).max(1);
        let written = index.written;
        let mut out = std::collections::HashMap::with_capacity(entries.len());
        let parts: Vec<Result<Vec<(String, WorkState)>>> = std::thread::scope(|s| {
            let handles: Vec<_> = entries
                .chunks(chunk)
                .map(|part| {
                    s.spawn(move || -> Result<Vec<(String, WorkState)>> {
                        part.iter().map(|e| Ok((e.path.clone(), self.work_matches(e, written)?))).collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(err!("a status thread panicked")))).collect()
        });
        for p in parts {
            out.extend(p?);
        }
        Ok(out)
    }

    pub fn status(&self) -> Result<Status> {
        let index = self.read_index()?;
        let head = self.head_commit()?;
        let head_files = self.commit_files(head.as_ref())?;
        let mut paths: BTreeSet<String> = head_files.keys().cloned().collect();
        paths.extend(index.entries().map(|e| e.path.clone()));

        let mut entries = Vec::new();
        let conflicted: BTreeSet<String> = index.conflicted_paths().into_iter().collect();
        let states = self.work_states(&index)?;
        for path in &paths {
            if conflicted.contains(path) {
                let has = |s: u8| index.get_stage(path, s).is_some();
                let code = match (has(1), has(2), has(3)) {
                    (true, true, true) => "UU",
                    (false, true, true) => "AA",
                    (true, false, false) => "DD",
                    (false, true, false) => "AU",
                    (false, false, true) => "UA",
                    (true, true, false) => "UD",
                    (true, false, true) => "DU",
                    (false, false, false) => unreachable!(),
                };
                entries.push(FileStatus { path: path.clone(), x: code.chars().next().unwrap(), y: code.chars().nth(1).unwrap() });
                continue;
            }
            let idx = index.get(path);
            let head_entry = head_files.get(path);
            let x = match (head_entry, idx) {
                (None, Some(_)) => 'A',
                (Some(_), None) => 'D',
                (Some((m, o)), Some(i)) if *m != i.mode || *o != i.oid => 'M',
                _ => ' ',
            };
            let y = match idx {
                Some(i) => match states.get(i.path.as_str()).copied().unwrap_or(WorkState::Missing) {
                    WorkState::Same => ' ',
                    WorkState::Changed => 'M',
                    WorkState::Missing => 'D',
                },
                None => ' ',
            };
            if x != ' ' || y != ' ' {
                entries.push(FileStatus { path: path.clone(), x, y });
            }
        }
        let untracked = self.untracked(&index)?;
        Ok(Status { entries, untracked })
    }

    fn untracked(&self, index: &Index) -> Result<Vec<String>> {
        let tracked: BTreeSet<&str> = index.entries().map(|e| e.path.as_str()).collect();
        let mut tracked_dirs: BTreeSet<String> = BTreeSet::new();
        for p in &tracked {
            let mut cur = *p;
            while let Some(i) = cur.rfind('/') {
                cur = &cur[..i];
                tracked_dirs.insert(cur.to_string());
            }
        }
        let mut out = Vec::new();
        let root = self.matcher_for("");
        self.walk_untracked("", &tracked, &tracked_dirs, &root, &mut out)?;
        out.sort();
        Ok(out)
    }

    /// True if `dir` holds at least one file that is neither tracked nor ignored.
    fn has_untracked_inside(&self, dir: &str, parent: &Matcher) -> Result<bool> {
        let Ok(rd) = fs::read_dir(self.abs(dir)) else { return Ok(false) };
        let matcher = self.extend_matcher(parent, dir);
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let rel = format!("{dir}/{name}");
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if matcher.is_ignored(&rel, is_dir) {
                continue;
            }
            if !is_dir || self.has_untracked_inside(&rel, &matcher)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn walk_untracked(
        &self,
        dir: &str,
        tracked: &BTreeSet<&str>,
        tracked_dirs: &BTreeSet<String>,
        matcher: &Matcher,
        out: &mut Vec<String>,
    ) -> Result<()> {
        for e in fs::read_dir(self.abs(dir))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            let is_dir = e.file_type()?.is_dir();
            if is_dir {
                if matcher.is_ignored(&rel, true) {
                    continue;
                }
                if tracked_dirs.contains(&rel) {
                    let child = self.extend_matcher(matcher, &rel);
                    self.walk_untracked(&rel, tracked, tracked_dirs, &child, out)?;
                } else if self.abs(&rel).join(".git").exists() || self.has_untracked_inside(&rel, matcher)? {
                    out.push(format!("{rel}/"));
                }
            } else if !tracked.contains(rel.as_str()) && !matcher.is_ignored(&rel, false) {
                out.push(rel);
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- commit

    /// Cleans a commit message the way `git commit` does by default: trailing whitespace removed
    /// from every line, runs of blank lines collapsed, leading and trailing blank lines dropped,
    /// and a final newline added.
    pub fn clean_message(msg: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        let mut blank_pending = false;
        let normalized = msg.replace("\r\n", "\n");
        for line in normalized.split('\n') {
            let line = line.trim_end();
            if line.is_empty() {
                blank_pending = !out.is_empty();
            } else {
                if blank_pending {
                    out.push("");
                    blank_pending = false;
                }
                out.push(line);
            }
        }
        if out.is_empty() {
            return String::new();
        }
        let mut s = out.join("\n");
        s.push('\n');
        s
    }

    /// Commits the index. Returns the new commit's id.
    pub fn commit(&self, message: &str, allow_empty: bool, amend: bool) -> Result<Oid> {
        let index = self.read_index()?;
        let tree = self.write_tree(&index)?;
        let head = self.head_commit()?;
        let merge_head = self.read_merge_head()?;
        let mut message = Repo::clean_message(message);

        let (parents, author_override) = if amend {
            let old = head.ok_or_else(|| err!("nothing to amend: there are no commits yet"))?;
            let c = self.read_commit(&old)?;
            if message.is_empty() {
                message = c.message.clone();
            }
            (c.parents.clone(), Some(c.author))
        } else {
            let mut p: Vec<Oid> = head.into_iter().collect();
            p.extend(merge_head);
            (p, None)
        };
        if message.is_empty() {
            return Err(err!("aborting commit due to an empty commit message"));
        }
        if !allow_empty && !amend && merge_head.is_none() {
            let same = match head {
                Some(h) => self.read_commit(&h)?.tree == tree,
                None => index.is_empty(),
            };
            if same {
                return Err(err!(
                    "nothing to commit (the index matches {})",
                    if head.is_some() { "HEAD" } else { "an empty tree" }
                ));
            }
        }
        let commit = Commit {
            tree,
            parents,
            author: match author_override {
                Some(a) => a,
                None => self.signature("AUTHOR")?,
            },
            committer: self.signature("COMMITTER")?,
            extra: vec![],
            message,
        };
        let id = self.odb.write(Kind::Commit, &commit.serialize())?;
        self.advance_head(&id)?;
        self.clear_merge_state()?;
        Ok(id)
    }

    // ---------------------------------------------------------------- switching trees

    /// Makes the work tree and the index match `target`'s tree. `HEAD` is not moved.
    ///
    /// Without `force`, a path whose content would be lost (modified or deleted locally, staged,
    /// or an untracked file in the way) stops the whole operation before anything is changed.
    /// With `force`, the work tree is made to match whatever it takes; untracked files that are
    /// not in the target are left alone.
    pub fn switch_to(&self, target: Option<&Oid>, force: bool) -> Result<SwitchReport> {
        let target_files = self.commit_files(target)?;
        self.switch_files(&target_files, force)
    }

    /// Like [`Repo::switch_to`], for a set of files that need not be a commit (a merge result).
    pub fn switch_files(&self, target_files: &BTreeMap<String, (u32, Oid)>, force: bool) -> Result<SwitchReport> {
        let mut index = self.read_index()?;
        if index.has_conflicts() && !force {
            return Err(err!(
                "you have unmerged paths: {} (resolve them or use reset --hard)",
                index.conflicted_paths().join(", ")
            ));
        }
        let head_files = self.commit_files(self.head_commit()?.as_ref())?;

        // Which paths change?
        let mut changed: BTreeSet<String> = BTreeSet::new();
        if force {
            for (p, t) in target_files {
                let differs = match index.get(p) {
                    Some(i) => i.oid != t.1 || i.mode != t.0 || self.work_matches(i, index.written)? != WorkState::Same,
                    None => true,
                };
                if differs {
                    changed.insert(p.clone());
                }
            }
            for e in index.entries() {
                if !target_files.contains_key(&e.path) {
                    changed.insert(e.path.clone());
                }
            }
        } else {
            for p in head_files.keys().chain(target_files.keys()) {
                if head_files.get(p) != target_files.get(p) {
                    changed.insert(p.clone());
                }
            }
        }

        // Refuse to lose anything.
        if !force {
            let mut would_lose: Vec<String> = Vec::new();
            let mut untracked_in_way: Vec<String> = Vec::new();
            for p in &changed {
                let idx = index.get(p);
                let want = target_files.get(p);
                let staged_differs = match (idx, head_files.get(p)) {
                    (Some(i), Some((m, o))) => i.oid != *o || i.mode != *m,
                    (Some(_), None) => true,
                    (None, Some(_)) => true,
                    (None, None) => false,
                };
                let staged_is_target = match (idx, want) {
                    (Some(i), Some((m, o))) => i.oid == *o && i.mode == *m,
                    (None, None) => true,
                    _ => false,
                };
                match idx {
                    Some(i) => {
                        let state = self.work_matches(i, index.written)?;
                        let work_is_target = || -> Result<bool> {
                            Ok(match (self.read_work_file(p, Some(i.mode))?, want) {
                                (Some(f), Some((m, o))) => object_id(Kind::Blob, &f.data) == *o && f.mode == *m,
                                (None, None) => true,
                                _ => false,
                            })
                        };
                        if staged_differs && !staged_is_target || state != WorkState::Same && !work_is_target()? {
                            would_lose.push(p.clone());
                        }
                    }
                    None => {
                        if staged_differs && head_files.contains_key(p) && !staged_is_target {
                            would_lose.push(p.clone());
                        } else if want.is_some() {
                            if let Some(f) = self.read_work_file(p, None)? {
                                let same = want.is_some_and(|(_, o)| object_id(Kind::Blob, &f.data) == *o);
                                if !same {
                                    untracked_in_way.push(p.clone());
                                }
                            } else if self.abs(p).is_dir() {
                                untracked_in_way.push(p.clone());
                            }
                        }
                    }
                }
            }
            if !would_lose.is_empty() {
                return Err(err!(
                    "your local changes to the following files would be overwritten:\n\t{}\ncommit or discard them first (or use --force)",
                    would_lose.join("\n\t")
                ));
            }
            if !untracked_in_way.is_empty() {
                return Err(err!(
                    "untracked files in the way would be overwritten:\n\t{}\nmove or remove them first (or use --force)",
                    untracked_in_way.join("\n\t")
                ));
            }
        }

        // Validate everything that will be written before anything is changed, so a refusal
        // leaves the repository exactly as it was.
        for p in &changed {
            if let Some((_, oid)) = target_files.get(p) {
                if !path_is_safe(p) {
                    return Err(err!("refusing to check out the unsafe path {p:?}"));
                }
                if !self.odb.exists(oid) {
                    return Err(err!("cannot check out {p:?}: object {oid} is missing from the repository"));
                }
            }
        }

        // Apply: removals first, so a file can become a directory and the other way round.
        let mut report = SwitchReport::default();
        for p in &changed {
            if !target_files.contains_key(p) {
                if !path_is_safe(p) {
                    continue;
                }
                let abs = self.abs(p);
                if fs::symlink_metadata(&abs).map(|m| !m.is_dir()).unwrap_or(false) {
                    fs::remove_file(&abs).map_err(|e| err!("cannot remove {p}: {e}"))?;
                    self.prune_empty_dirs(Path::new(p).parent());
                }
                index.remove(p);
                report.removed += 1;
            }
        }
        for p in &changed {
            let Some((mode, oid)) = target_files.get(p) else { continue };
            if !path_is_safe(p) {
                return Err(err!("refusing to check out unsafe path {p:?}"));
            }
            let data = self.odb.read_kind(oid, Kind::Blob)?;
            let abs = self.abs(p);
            if let Some(parent) = abs.parent() {
                // A tracked file in the way of a new directory was removed above; anything else is a real obstacle.
                fs::create_dir_all(parent).map_err(|e| err!("cannot create the directory for {p}: {e}"))?;
            }
            if abs.is_dir() {
                fs::remove_dir(&abs).map_err(|e| err!("cannot replace the directory {p} with a file: {e}"))?;
            }
            self.write_work_file(&abs, &data, *mode)?;
            let meta = fs::symlink_metadata(&abs)?;
            let (ctime, mtime, size) = stat_fields(&meta);
            let mut e = Entry::new(p, *mode, *oid);
            e.ctime = ctime;
            e.mtime = mtime;
            e.size = size;
            index.add(e);
            report.written += 1;
        }
        if force {
            // Whatever is staged but not in the target is gone; make the index exactly the target.
            let stale: Vec<String> =
                index.entries().filter(|e| !target_files.contains_key(&e.path) || e.stage != 0).map(|e| e.path.clone()).collect();
            for p in stale {
                index.remove(&p);
            }
        }
        self.write_index(&index)?;
        Ok(report)
    }

    #[cfg(unix)]
    fn write_work_file(&self, abs: &Path, data: &[u8], mode: u32) -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let _ = fs::remove_file(abs);
        if mode == MODE_LINK {
            symlink(String::from_utf8_lossy(data).as_ref(), abs)?;
            return Ok(());
        }
        fs::write(abs, data)?;
        fs::set_permissions(abs, fs::Permissions::from_mode(if mode == MODE_EXEC { 0o755 } else { 0o644 }))?;
        Ok(())
    }

    /// Without symlink support a link is written as a file holding its target, as git does with
    /// `core.symlinks=false`.
    #[cfg(not(unix))]
    fn write_work_file(&self, abs: &Path, data: &[u8], _mode: u32) -> Result<()> {
        fs::write(abs, data).map_err(|e| err!("{}: {e}", abs.display()))
    }

    fn prune_empty_dirs(&self, mut dir: Option<&Path>) {
        while let Some(d) = dir {
            if d.as_os_str().is_empty() || fs::remove_dir(self.work.join(d)).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    // ---------------------------------------------------------------- merge state files

    pub fn read_merge_head(&self) -> Result<Option<Oid>> {
        match fs::read_to_string(self.git.join("MERGE_HEAD")) {
            Ok(t) => Ok(Oid::from_hex(t.trim())),
            Err(_) => Ok(None),
        }
    }

    pub fn clear_merge_state(&self) -> Result<()> {
        for f in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
            let _ = fs::remove_file(self.git.join(f));
        }
        Ok(())
    }
}

/// Result of staging one file.
enum Staged {
    Unchanged,
    /// The new index entry, and whether its content differs from what the index had.
    Entry(Entry, bool),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum WorkState {
    Same,
    Changed,
    Missing,
}

#[derive(Debug, Default)]
pub struct AddReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub ignored: Vec<String>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SwitchReport {
    pub written: usize,
    pub removed: usize,
}

/// One line of `git status --porcelain`: the staged state (`x`) and the unstaged state (`y`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub path: String,
    pub x: char,
    pub y: char,
}

#[derive(Debug, Default)]
pub struct Status {
    pub entries: Vec<FileStatus>,
    pub untracked: Vec<String>,
}

impl Status {
    /// The same text as `git status --porcelain` (version 1).
    pub fn porcelain(&self) -> String {
        let mut s = String::new();
        for e in &self.entries {
            s.push_str(&format!("{}{} {}\n", e.x, e.y, e.path));
        }
        for u in &self.untracked {
            s.push_str(&format!("?? {u}\n"));
        }
        s
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty() && self.untracked.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathspecs() {
        assert_eq!(normalize_pathspec("", "a/b").unwrap(), "a/b");
        assert_eq!(normalize_pathspec("", ".").unwrap(), "");
        assert_eq!(normalize_pathspec("sub", "x.txt").unwrap(), "sub/x.txt");
        assert_eq!(normalize_pathspec("sub/deep", "../x.txt").unwrap(), "sub/x.txt");
        assert_eq!(normalize_pathspec("sub", "..").unwrap(), "");
        assert_eq!(normalize_pathspec("sub", "./a//b/").unwrap(), "sub/a/b");
        assert_eq!(normalize_pathspec("sub", "a\\b").unwrap(), "sub/a/b");
        assert!(normalize_pathspec("", "..").is_err());
        assert!(normalize_pathspec("sub", "../../x").is_err());
    }

    #[test]
    fn message_cleanup() {
        assert_eq!(Repo::clean_message("subject"), "subject\n");
        assert_eq!(Repo::clean_message("subject\n\n\nbody  \n\n\n"), "subject\n\nbody\n");
        assert_eq!(Repo::clean_message("\n\n  \nsubject   \r\n\r\nbody\r\n"), "subject\n\nbody\n");
        assert_eq!(Repo::clean_message("  \n\n"), "");
        assert_eq!(Repo::clean_message("a\n\n\n\nb"), "a\n\nb\n");
        assert_eq!(Repo::clean_message("  indented stays\n"), "  indented stays\n");
    }
}

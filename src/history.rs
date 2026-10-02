//! Commit history: walking it, finding common ancestors, branches, tags, merging and checking
//! the repository for damage.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet};
use std::fs;

use crate::diff::{is_binary, merge3};
use crate::err;
use crate::error::Result;
use crate::index::Entry;
use crate::object::{Commit, Kind, Signature, TagObject, Tree, MODE_DIR, MODE_FILE};
use crate::repo::{valid_ref_name, Head, Repo};
use crate::sha1::Oid;

impl Repo {
    /// Commits reachable from `start`, newest first (by commit time, children before parents).
    pub fn log(&self, start: &[Oid], limit: Option<usize>) -> Result<Vec<(Oid, Commit)>> {
        let mut heap: BinaryHeap<(i64, Reverse<usize>, Oid)> = BinaryHeap::new();
        let mut seen: HashSet<Oid> = HashSet::new();
        let mut order = 0usize;
        let mut cache: BTreeMap<Oid, Commit> = BTreeMap::new();
        for s in start {
            let id = self.peel_to_commit(s)?;
            if seen.insert(id) {
                let c = self.read_commit(&id)?;
                heap.push((c.committer.when, Reverse(order), id));
                order += 1;
                cache.insert(id, c);
            }
        }
        let mut out = Vec::new();
        while let Some((_, _, id)) = heap.pop() {
            let c = cache.remove(&id).expect("queued commits are cached");
            for p in &c.parents {
                if seen.insert(*p) {
                    let pc = self.read_commit(p)?;
                    heap.push((pc.committer.when, Reverse(order), *p));
                    order += 1;
                    cache.insert(*p, pc);
                }
            }
            out.push((id, c));
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
        }
        Ok(out)
    }

    /// Every commit reachable from `id`, including itself.
    pub fn ancestors(&self, id: &Oid) -> Result<HashSet<Oid>> {
        let mut seen = HashSet::new();
        let mut stack = vec![*id];
        while let Some(c) = stack.pop() {
            if !seen.insert(c) {
                continue;
            }
            stack.extend(self.read_commit(&c)?.parents);
        }
        Ok(seen)
    }

    pub fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool> {
        Ok(self.ancestors(descendant)?.contains(ancestor))
    }

    /// The best common ancestors of two commits: common ancestors that are not themselves
    /// ancestors of another common ancestor. Usually there is exactly one.
    pub fn merge_bases(&self, a: &Oid, b: &Oid) -> Result<Vec<Oid>> {
        let (aa, ab) = (self.ancestors(a)?, self.ancestors(b)?);
        let commons: BTreeSet<Oid> = aa.intersection(&ab).copied().collect();
        let mut dominated: HashSet<Oid> = HashSet::new();
        for c in &commons {
            for p in self.read_commit(c)?.parents {
                dominated.extend(self.ancestors(&p)?);
            }
        }
        Ok(commons.into_iter().filter(|c| !dominated.contains(c)).collect())
    }

    // ---------------------------------------------------------------- branches and tags

    pub fn branches(&self) -> Result<Vec<(String, Oid)>> {
        Ok(self.list_refs("refs/heads/")?.into_iter().map(|(n, o)| (n["refs/heads/".len()..].to_string(), o)).collect())
    }

    pub fn create_branch(&self, name: &str, at: &Oid) -> Result<()> {
        if !valid_ref_name(name) || name.starts_with('-') {
            return Err(err!("{name:?} is not a valid branch name"));
        }
        if self.read_ref(&format!("refs/heads/{name}"))?.is_some() {
            return Err(err!("a branch named {name:?} already exists"));
        }
        self.write_ref(&format!("refs/heads/{name}"), &self.peel_to_commit(at)?)
    }

    pub fn delete_branch(&self, name: &str, force: bool) -> Result<Oid> {
        let full = format!("refs/heads/{name}");
        let tip = self.read_ref(&full)?.ok_or_else(|| err!("no branch named {name:?}"))?;
        if self.head()? == Head::Branch(name.to_string()) {
            return Err(err!("cannot delete the branch {name:?} that is checked out"));
        }
        if !force {
            match self.head_commit()? {
                Some(h) if self.is_ancestor(&tip, &h)? => {}
                _ => return Err(err!("the branch {name:?} is not fully merged into HEAD (use -D to delete it anyway)")),
            }
        }
        self.delete_ref(&full)?;
        Ok(tip)
    }

    pub fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        let tip = self.read_ref(&format!("refs/heads/{old}"))?.ok_or_else(|| err!("no branch named {old:?}"))?;
        if !valid_ref_name(new) {
            return Err(err!("{new:?} is not a valid branch name"));
        }
        if self.read_ref(&format!("refs/heads/{new}"))?.is_some() {
            return Err(err!("a branch named {new:?} already exists"));
        }
        self.write_ref(&format!("refs/heads/{new}"), &tip)?;
        if self.head()? == Head::Branch(old.to_string()) {
            self.set_head_branch(new)?;
        }
        self.delete_ref(&format!("refs/heads/{old}"))?;
        Ok(())
    }

    pub fn tags(&self) -> Result<Vec<(String, Oid)>> {
        Ok(self.list_refs("refs/tags/")?.into_iter().map(|(n, o)| (n["refs/tags/".len()..].to_string(), o)).collect())
    }

    /// Creates a lightweight tag, or an annotated one when `message` is given.
    pub fn create_tag(&self, name: &str, at: &Oid, message: Option<&str>) -> Result<Oid> {
        if !valid_ref_name(name) || name.starts_with('-') {
            return Err(err!("{name:?} is not a valid tag name"));
        }
        let full = format!("refs/tags/{name}");
        if self.read_ref(&full)?.is_some() {
            return Err(err!("a tag named {name:?} already exists"));
        }
        let target = match message {
            None => *at,
            Some(m) => {
                let (obj, kind) = {
                    let (k, _) = self.odb.read(at)?;
                    (*at, k)
                };
                let tag = TagObject {
                    object: obj,
                    kind,
                    name: name.to_string(),
                    tagger: Some(self.signature("COMMITTER")?),
                    message: Repo::clean_message(m),
                };
                self.odb.write(Kind::Tag, &tag.serialize())?
            }
        };
        self.write_ref(&full, &target)?;
        Ok(target)
    }

    pub fn delete_tag(&self, name: &str) -> Result<()> {
        if self.delete_ref(&format!("refs/tags/{name}"))? {
            Ok(())
        } else {
            Err(err!("no tag named {name:?}"))
        }
    }

    // ---------------------------------------------------------------- reset and restore

    /// `reset`: moves the branch (or detached HEAD) to `target`, and with `mixed` or `hard` the
    /// index, and with `hard` the work tree.
    pub fn reset(&self, target: &Oid, mode: ResetMode) -> Result<()> {
        let commit = self.peel_to_commit(target)?;
        match mode {
            ResetMode::Soft => {}
            ResetMode::Hard => {
                self.switch_to(Some(&commit), true)?;
            }
            ResetMode::Mixed => {
                let files = self.commit_files(Some(&commit))?;
                let mut index = crate::index::Index::new();
                let old = self.read_index()?;
                for (p, (mode, oid)) in files {
                    // Keep the stat data of entries that did not change, so they are not rehashed.
                    let mut e = match old.get(&p) {
                        Some(o) if o.oid == oid && o.mode == mode => o.clone(),
                        _ => Entry::new(&p, mode, oid),
                    };
                    e.stage = 0;
                    index.add(e);
                }
                self.write_index(&index)?;
            }
        }
        self.advance_head(&commit)?;
        self.clear_merge_state()?;
        Ok(())
    }

    /// Resets the index entries of `paths` (files or directories) to `HEAD`, leaving the work tree alone.
    pub fn unstage(&self, paths: &[String]) -> Result<()> {
        let head = self.commit_files(self.head_commit()?.as_ref())?;
        let mut index = self.read_index()?;
        for spec in paths {
            let prefix = format!("{spec}/");
            let in_scope = |p: &str| spec.is_empty() || p == spec || p.starts_with(&prefix);
            let current: Vec<String> = index.entries().filter(|e| in_scope(&e.path)).map(|e| e.path.clone()).collect();
            for p in current {
                index.remove(&p);
            }
            for (p, (mode, oid)) in head.iter().filter(|(p, _)| in_scope(p)) {
                index.add(Entry::new(p, *mode, *oid));
            }
        }
        self.write_index(&index)
    }

    /// Replaces work tree files with their version in the index (or in `source`, a commit).
    pub fn restore_files(&self, paths: &[String], source: Option<&Oid>) -> Result<usize> {
        let mut index = self.read_index()?;
        let files: BTreeMap<String, (u32, Oid)> = match source {
            Some(c) => self.commit_files(Some(c))?,
            None => index.entries().filter(|e| e.stage == 0).map(|e| (e.path.clone(), (e.mode, e.oid))).collect(),
        };
        let mut n = 0;
        for spec in paths {
            let prefix = format!("{spec}/");
            let selected: Vec<(&String, &(u32, Oid))> =
                files.iter().filter(|(p, _)| spec.is_empty() || *p == spec || p.starts_with(&prefix)).collect();
            if selected.is_empty() {
                return Err(err!("pathspec {spec:?} did not match any tracked file"));
            }
            for (p, (mode, oid)) in selected {
                if !crate::repo::path_is_safe(p) {
                    return Err(err!("refusing to write unsafe path {p:?}"));
                }
                let data = self.odb.read_kind(oid, Kind::Blob)?;
                let abs = self.work.join(p);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&abs, &data)?;
                n += 1;
                // Keep the index entry fresh when restoring from the index itself.
                if source.is_some() {
                    index.add(Entry::new(p, *mode, *oid));
                } else if let Some(e) = index.get(p).cloned() {
                    let meta = fs::metadata(&abs)?;
                    let mut e = e;
                    e.size = meta.len() as u32;
                    if let Ok(m) = meta.modified() {
                        let d = m.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                        e.mtime = (d.as_secs() as u32, d.subsec_nanos());
                        e.ctime = e.mtime;
                    }
                    index.add(e);
                }
            }
        }
        self.write_index(&index)?;
        Ok(n)
    }

    // ---------------------------------------------------------------- merge

    /// Merges `other` into the current branch.
    pub fn merge(&self, other_spec: &str, no_ff: bool, message: Option<&str>) -> Result<MergeOutcome> {
        if self.read_merge_head()?.is_some() {
            return Err(err!("a merge is already in progress; finish it with commit or cancel it with merge --abort"));
        }
        let ours = self.head_commit()?.ok_or_else(|| err!("cannot merge into a branch with no commits"))?;
        let theirs = self.peel_to_commit(&self.rev_parse(other_spec)?)?;
        if self.is_ancestor(&theirs, &ours)? {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        let status = self.status()?;
        if status.entries.iter().any(|e| e.x != '?') {
            return Err(err!(
                "you have local changes to tracked files; commit or discard them before merging:\n{}",
                status.porcelain().trim_end()
            ));
        }
        let bases = self.merge_bases(&ours, &theirs)?;
        let label = self.merge_label(other_spec, &theirs);
        if bases.first() == Some(&ours) && !no_ff {
            self.switch_to(Some(&theirs), false)?;
            self.advance_head(&theirs)?;
            return Ok(MergeOutcome::FastForward(theirs));
        }
        let base_files = match bases.first() {
            Some(b) => self.commit_files(Some(b))?,
            None => BTreeMap::new(),
        };
        let our_files = self.commit_files(Some(&ours))?;
        let their_files = self.commit_files(Some(&theirs))?;

        let mut result: BTreeMap<String, (u32, Oid)> = BTreeMap::new();
        let mut conflicts: Vec<Conflict> = Vec::new();
        let mut paths: BTreeSet<&String> = BTreeSet::new();
        paths.extend(base_files.keys());
        paths.extend(our_files.keys());
        paths.extend(their_files.keys());
        for p in paths {
            let (b, o, t) = (base_files.get(p).copied(), our_files.get(p).copied(), their_files.get(p).copied());
            if o == t {
                if let Some(v) = o {
                    result.insert(p.clone(), v);
                }
            } else if b == o {
                if let Some(v) = t {
                    result.insert(p.clone(), v);
                }
            } else if b == t {
                if let Some(v) = o {
                    result.insert(p.clone(), v);
                }
            } else {
                match self.merge_file(p, b, o, t, "HEAD", other_spec)? {
                    FileMerge::Clean(v) => {
                        result.insert(p.clone(), v);
                    }
                    FileMerge::Conflict(c) => conflicts.push(c),
                }
            }
        }
        // A file and a directory with the same name cannot both exist.
        for p in result.keys() {
            let mut cur = p.as_str();
            while let Some(i) = cur.rfind('/') {
                cur = &cur[..i];
                if result.contains_key(cur) || conflicts.iter().any(|c| c.path == cur) {
                    return Err(err!("directory/file conflict at {cur:?}: this merge cannot be done automatically"));
                }
            }
        }

        // Apply the clean part to the work tree and index.
        self.switch_files(&result, false)?;
        let mut index = self.read_index()?;
        for c in &conflicts {
            index.remove(&c.path);
            let stages = [(1u8, c.base), (2, c.ours), (3, c.theirs)];
            for (stage, side) in stages {
                if let Some((mode, oid)) = side {
                    let mut e = Entry::new(&c.path, mode, oid);
                    e.stage = stage;
                    index.add(e);
                }
            }
            let abs = self.work.join(&c.path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            match &c.content {
                Some(bytes) => fs::write(&abs, bytes)?,
                None => {
                    let _ = fs::remove_file(&abs);
                }
            }
        }
        self.write_index(&index)?;

        let default_msg = format!("Merge {label}\n");
        let msg = message.map(Repo::clean_message).unwrap_or(default_msg);
        if !conflicts.is_empty() {
            fs::write(self.git.join("MERGE_HEAD"), format!("{theirs}\n"))?;
            let mut m = msg.clone();
            m.push_str("\nConflicts:\n");
            for c in &conflicts {
                m.push_str(&format!("\t{}\n", c.path));
            }
            fs::write(self.git.join("MERGE_MSG"), m)?;
            return Ok(MergeOutcome::Conflicts(conflicts.into_iter().map(|c| c.path).collect()));
        }
        let tree = self.write_tree(&self.read_index()?)?;
        let commit = Commit {
            tree,
            parents: vec![ours, theirs],
            author: self.signature("AUTHOR")?,
            committer: self.signature("COMMITTER")?,
            extra: vec![],
            message: msg,
        };
        let id = self.odb.write(Kind::Commit, &commit.serialize())?;
        self.advance_head(&id)?;
        Ok(MergeOutcome::Merged(id))
    }

    fn merge_label(&self, spec: &str, id: &Oid) -> String {
        if self.read_ref(&format!("refs/heads/{spec}")).ok().flatten().is_some() {
            format!("branch '{spec}'")
        } else if self.read_ref(&format!("refs/tags/{spec}")).ok().flatten().is_some() {
            format!("tag '{spec}'")
        } else {
            format!("commit '{}'", id.hex())
        }
    }

    fn merge_file(
        &self,
        path: &str,
        b: Option<(u32, Oid)>,
        o: Option<(u32, Oid)>,
        t: Option<(u32, Oid)>,
        ours_label: &str,
        theirs_label: &str,
    ) -> Result<FileMerge> {
        let read = |v: Option<(u32, Oid)>| -> Result<Option<Vec<u8>>> {
            v.map(|(_, oid)| self.odb.read_kind(&oid, Kind::Blob)).transpose()
        };
        let conflict = |content: Option<Vec<u8>>| {
            FileMerge::Conflict(Conflict { path: path.to_string(), base: b, ours: o, theirs: t, content })
        };
        let (Some(ov), Some(tv)) = (o, t) else {
            // Deleted on one side and changed on the other: keep the surviving version in the work tree.
            return Ok(conflict(read(o.or(t))?));
        };
        let (ob, tb) = (read(o)?.unwrap_or_default(), read(t)?.unwrap_or_default());
        let bb = read(b)?.unwrap_or_default();
        if is_binary(&ob) || is_binary(&tb) || is_binary(&bb) {
            return Ok(conflict(Some(ob)));
        }
        let merged = merge3(
            &String::from_utf8_lossy(&bb),
            &String::from_utf8_lossy(&ob),
            &String::from_utf8_lossy(&tb),
            ours_label,
            theirs_label,
        );
        // The mode follows whichever side changed it.
        let base_mode = b.map(|(m, _)| m).unwrap_or(MODE_FILE);
        let mode = if ov.0 == base_mode { tv.0 } else { ov.0 };
        if merged.conflicts > 0 {
            return Ok(conflict(Some(merged.text.into_bytes())));
        }
        let oid = self.odb.write(Kind::Blob, merged.text.as_bytes())?;
        Ok(FileMerge::Clean((mode, oid)))
    }

    /// Cancels a merge that stopped on conflicts.
    pub fn merge_abort(&self) -> Result<()> {
        if self.read_merge_head()?.is_none() {
            return Err(err!("there is no merge to abort"));
        }
        let head = self.head_commit()?.ok_or_else(|| err!("no commits"))?;
        self.reset(&head, ResetMode::Hard)
    }

    // ---------------------------------------------------------------- fsck

    /// Checks every object (hash, structure, references) and every ref. Returns the problems found.
    pub fn fsck(&self) -> Result<FsckReport> {
        let mut report = FsckReport::default();
        let ids = self.odb.all_ids()?;
        report.objects = ids.len();
        let mut referenced: HashSet<Oid> = HashSet::new();
        let mut kinds: BTreeMap<Oid, Kind> = BTreeMap::new();
        for id in &ids {
            let (kind, data) = match self.odb.read(id) {
                Ok(v) => v,
                Err(e) => {
                    report.problems.push(e.msg);
                    continue;
                }
            };
            kinds.insert(*id, kind);
            match kind {
                Kind::Blob => {}
                Kind::Tree => match Tree::parse(&data) {
                    Ok(t) => {
                        let mut names = BTreeSet::new();
                        for e in &t.entries {
                            if !names.insert(e.name.clone()) {
                                report.problems.push(format!("tree {id}: duplicate entry {:?}", e.name));
                            }
                            if !crate::repo::path_is_safe(&e.name) {
                                report.problems.push(format!("tree {id}: unsafe entry name {:?}", e.name));
                            }
                            if e.mode != crate::object::MODE_GITLINK {
                                referenced.insert(e.oid);
                                if !self.odb.exists(&e.oid) {
                                    report.problems.push(format!(
                                        "tree {id}: missing {} {}",
                                        if e.mode == MODE_DIR { "tree" } else { "blob" },
                                        e.oid
                                    ));
                                }
                            }
                        }
                    }
                    Err(e) => report.problems.push(format!("tree {id}: {}", e.msg)),
                },
                Kind::Commit => match Commit::parse(&data) {
                    Ok(c) => {
                        for r in std::iter::once(&c.tree).chain(c.parents.iter()) {
                            referenced.insert(*r);
                            if !self.odb.exists(r) {
                                report.problems.push(format!("commit {id}: missing object {r}"));
                            }
                        }
                    }
                    Err(e) => report.problems.push(format!("commit {id}: {}", e.msg)),
                },
                Kind::Tag => match TagObject::parse(&data) {
                    Ok(t) => {
                        referenced.insert(t.object);
                        if !self.odb.exists(&t.object) {
                            report.problems.push(format!("tag {id}: missing object {}", t.object));
                        }
                    }
                    Err(e) => report.problems.push(format!("tag {id}: {}", e.msg)),
                },
            }
        }
        for (name, oid) in self.list_refs("refs/")? {
            referenced.insert(oid);
            if !self.odb.exists(&oid) {
                report.problems.push(format!("ref {name} points at the missing object {oid}"));
            } else if name.starts_with("refs/heads/") && kinds.get(&oid) != Some(&Kind::Commit) {
                report.problems.push(format!("branch {name} does not point at a commit"));
            }
        }
        if let Some(h) = self.head_commit().ok().flatten() {
            referenced.insert(h);
            if !self.odb.exists(&h) {
                report.problems.push(format!("HEAD points at the missing object {h}"));
            }
        }
        // Dangling: stored but referenced by nothing (not an error, just unreachable).
        let reachable = self.reachable_from_refs(&kinds)?;
        report.dangling = ids.iter().filter(|i| !reachable.contains(i)).copied().collect();
        Ok(report)
    }

    fn reachable_from_refs(&self, kinds: &BTreeMap<Oid, Kind>) -> Result<HashSet<Oid>> {
        let mut roots: Vec<Oid> = self.list_refs("refs/")?.into_iter().map(|(_, o)| o).collect();
        if let Some(h) = self.head_commit().ok().flatten() {
            roots.push(h);
        }
        let idx = self.read_index().unwrap_or_default();
        roots.extend(idx.entries().map(|e| e.oid));
        let mut seen = HashSet::new();
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(kind) = kinds.get(&id) else { continue };
            let Ok((_, data)) = self.odb.read(&id) else { continue };
            match kind {
                Kind::Commit => {
                    if let Ok(c) = Commit::parse(&data) {
                        stack.push(c.tree);
                        stack.extend(c.parents);
                    }
                }
                Kind::Tree => {
                    if let Ok(t) = Tree::parse(&data) {
                        stack.extend(t.entries.iter().filter(|e| e.mode != crate::object::MODE_GITLINK).map(|e| e.oid));
                    }
                }
                Kind::Tag => {
                    if let Ok(t) = TagObject::parse(&data) {
                        stack.push(t.object);
                    }
                }
                Kind::Blob => {}
            }
        }
        Ok(seen)
    }

    /// Identity used for a `Signature` when none is configured, for tests and plumbing.
    pub fn fallback_signature() -> Signature {
        Signature { name: "mg".into(), email: "mg@localhost".into(), when: 0, tz: "+0000".into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    AlreadyUpToDate,
    FastForward(Oid),
    Merged(Oid),
    Conflicts(Vec<String>),
}

struct Conflict {
    path: String,
    base: Option<(u32, Oid)>,
    ours: Option<(u32, Oid)>,
    theirs: Option<(u32, Oid)>,
    /// What to leave in the work tree (`None` removes the file).
    content: Option<Vec<u8>>,
}

enum FileMerge {
    Clean((u32, Oid)),
    Conflict(Conflict),
}

#[derive(Debug, Default)]
pub struct FsckReport {
    pub objects: usize,
    pub problems: Vec<String>,
    pub dangling: Vec<Oid>,
}

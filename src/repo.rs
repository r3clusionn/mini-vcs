//! A repository: where it is, its refs, its config, its identity and revision parsing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::err;
use crate::error::Result;
use crate::index::Index;
use crate::object::{Commit, Kind, Signature, TagObject, Tree, MODE_DIR};
use crate::odb::Odb;
use crate::sha1::Oid;

#[derive(Debug)]
pub struct Repo {
    pub work: PathBuf,
    pub git: PathBuf,
    pub odb: Odb,
    /// Content of work tree files that a diff has hashed but that is not in the object store.
    pub(crate) scratch: std::sync::Mutex<BTreeMap<Oid, Vec<u8>>>,
}

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// On a branch, by its short name (`main`). The branch may not exist yet.
    Branch(String),
    Detached(Oid),
}

/// Whether a name is acceptable as a branch, tag or ref name (git-check-ref-format).
pub fn valid_ref_name(name: &str) -> bool {
    if name.is_empty()
        || name == "@"
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
    {
        return false;
    }
    if name.chars().any(|c| c.is_control() || c == ' ' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')) {
        return false;
    }
    name.split('/').all(|c| !c.starts_with('.') && !c.ends_with(".lock"))
}

/// Whether a repository-relative path may be created in the work tree: no `..`, no `.git` (in any
/// spelling Windows would resolve to it), no drive letters, backslashes or alternate data streams.
pub fn path_is_safe(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }
    path.split('/').all(|c| {
        let stripped = c.trim_end_matches(['.', ' ']);
        !c.is_empty()
            && c != "."
            && c != ".."
            && !stripped.eq_ignore_ascii_case(".git")
            && !stripped.eq_ignore_ascii_case("git~1")
            && !c.contains(['\\', ':'])
            && !c.chars().any(|ch| ch.is_control())
    })
}

impl Repo {
    /// Creates a repository in `dir` (which must exist). Returns `Ok(None)` if one is already there.
    pub fn init(dir: &Path, branch: &str) -> Result<Option<Repo>> {
        let git = dir.join(".git");
        if git.join("HEAD").exists() {
            return Ok(None);
        }
        if !valid_ref_name(branch) {
            return Err(err!("{branch:?} is not a valid branch name"));
        }
        for d in ["objects", "refs/heads", "refs/tags"] {
            fs::create_dir_all(git.join(d))?;
        }
        fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
        let windows = cfg!(windows);
        let config = format!(
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = {}\n\tbare = false\n\tsymlinks = {}\n\tignorecase = {}\n",
            !windows, !windows, windows
        );
        fs::write(git.join("config"), config)?;
        Ok(Some(Repo::open(dir)?))
    }

    /// Opens the repository whose work tree is `dir`.
    pub fn open(dir: &Path) -> Result<Repo> {
        let work = fs::canonicalize(dir).map_err(|e| err!("{}: {e}", dir.display()))?;
        let work = PathBuf::from(work.to_string_lossy().trim_start_matches(r"\\?\"));
        let dot = work.join(".git");
        let git = if dot.is_file() {
            // A `.git` file redirects to the real directory (submodules and linked work trees).
            let text = fs::read_to_string(&dot)?;
            let target =
                text.trim().strip_prefix("gitdir:").ok_or_else(|| err!("{} is not a git directory file", dot.display()))?.trim();
            let p = PathBuf::from(target);
            if p.is_absolute() {
                p
            } else {
                work.join(p)
            }
        } else {
            dot
        };
        if !git.join("HEAD").is_file() || !git.join("objects").is_dir() {
            return Err(err!("{} is not a repository (no .git/HEAD and .git/objects)", work.display()));
        }
        let odb = Odb::new(git.join("objects"));
        Ok(Repo { work, git, odb, scratch: Default::default() })
    }

    /// Finds the repository that contains `start` (or is `start`).
    pub fn discover(start: &Path) -> Result<Repo> {
        let mut dir = fs::canonicalize(start).map_err(|e| err!("{}: {e}", start.display()))?;
        loop {
            if dir.join(".git").exists() {
                return Repo::open(&dir);
            }
            if !dir.pop() {
                return Err(err!("not a repository (or any parent): no .git found from {}", start.display()));
            }
        }
    }

    // ---------------------------------------------------------------- config and identity

    pub fn config(&self) -> Config {
        Config::parse(&fs::read_to_string(self.git.join("config")).unwrap_or_default())
    }

    pub fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let mut c = self.config();
        c.set(key, value)?;
        fs::write(self.git.join("config"), c.to_text())?;
        Ok(())
    }

    /// A config value from the repository, falling back to the user's global git config.
    pub fn config_get(&self, key: &str) -> Option<String> {
        if let Some(v) = self.config().get(key) {
            return Some(v);
        }
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
        for rel in [".gitconfig", ".config/git/config"] {
            if let Ok(text) = fs::read_to_string(Path::new(&home).join(rel)) {
                if let Some(v) = Config::parse(&text).get(key) {
                    return Some(v);
                }
            }
        }
        None
    }

    /// The author or committer (`kind` is `AUTHOR` or `COMMITTER`): from `GIT_<kind>_NAME`,
    /// `GIT_<kind>_EMAIL` and `GIT_<kind>_DATE` if set, otherwise from the config. Without a date
    /// the current time is used, in UTC.
    pub fn signature(&self, kind: &str) -> Result<Signature> {
        let env = |suffix: &str| std::env::var(format!("GIT_{kind}_{suffix}")).ok().filter(|v| !v.is_empty());
        let name = env("NAME").or_else(|| self.config_get("user.name"));
        let email = env("EMAIL").or_else(|| self.config_get("user.email"));
        let (Some(name), Some(email)) = (name, email) else {
            return Err(err!(
                "author identity unknown: run  mg config user.name \"Your Name\"  and  mg config user.email you@example.com"
            ));
        };
        let (when, tz) = match env("DATE") {
            Some(d) => parse_date(&d)?,
            None => (
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
                "+0000".to_string(),
            ),
        };
        if name.contains(['<', '>', '\n']) || email.contains(['<', '>', '\n']) {
            return Err(err!("a name or email may not contain <, > or a newline"));
        }
        Ok(Signature { name, email, when, tz })
    }

    // ---------------------------------------------------------------- refs

    fn ref_path(&self, full: &str) -> PathBuf {
        self.git.join(full)
    }

    fn packed_refs(&self) -> Vec<(String, Oid)> {
        let text = fs::read_to_string(self.git.join("packed-refs")).unwrap_or_default();
        text.lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with('^'))
            .filter_map(|l| {
                let (h, name) = l.split_once(' ')?;
                Some((name.to_string(), Oid::from_hex(h)?))
            })
            .collect()
    }

    /// The commit or object a full ref name points at, following symbolic refs.
    pub fn read_ref(&self, full: &str) -> Result<Option<Oid>> {
        let mut name = full.to_string();
        for _ in 0..8 {
            match fs::read_to_string(self.ref_path(&name)) {
                Ok(text) => {
                    let text = text.trim();
                    if let Some(target) = text.strip_prefix("ref:") {
                        name = target.trim().to_string();
                        continue;
                    }
                    return Oid::from_hex(text).map(Some).ok_or_else(|| err!("ref {name} is corrupt: {text:?}"));
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                    return Ok(self.packed_refs().into_iter().find(|(n, _)| *n == name).map(|(_, o)| o));
                }
                Err(e) if self.ref_path(&name).is_dir() => {
                    let _ = e;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(err!("symbolic ref {full} points too deep"))
    }

    pub fn head(&self) -> Result<Head> {
        let text = fs::read_to_string(self.git.join("HEAD"))?;
        let text = text.trim();
        if let Some(target) = text.strip_prefix("ref:") {
            let target = target.trim();
            let name =
                target.strip_prefix("refs/heads/").ok_or_else(|| err!("HEAD points at {target}, which is not a branch"))?;
            return Ok(Head::Branch(name.to_string()));
        }
        Oid::from_hex(text).map(Head::Detached).ok_or_else(|| err!("HEAD is corrupt: {text:?}"))
    }

    /// The commit `HEAD` resolves to, or `None` on a branch that has no commits yet.
    pub fn head_commit(&self) -> Result<Option<Oid>> {
        match self.head()? {
            Head::Detached(o) => Ok(Some(o)),
            Head::Branch(b) => self.read_ref(&format!("refs/heads/{b}")),
        }
    }

    /// Creates or moves a ref. The write goes to a lock file and is renamed into place.
    pub fn write_ref(&self, full: &str, oid: &Oid) -> Result<()> {
        if !full.starts_with("refs/") || !valid_ref_name(full) {
            return Err(err!("{full:?} is not a valid ref name"));
        }
        let path = self.ref_path(full);
        if path.is_dir() {
            return Err(err!("cannot create {full}: refs below it exist"));
        }
        // `refs/heads/a` cannot exist if `refs/heads/a/b` is wanted.
        let mut parent = Path::new(full).parent();
        while let Some(p) = parent {
            if p.as_os_str().is_empty() || p == Path::new("refs") {
                break;
            }
            let pp = self.git.join(p);
            if pp.is_file() {
                return Err(err!("cannot create {full}: {} exists as a ref", p.display()));
            }
            parent = p.parent();
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let lock = path.with_extension("lock");
        let mut lock_name = path.file_name().unwrap().to_os_string();
        lock_name.push(".lock");
        let lock = lock.with_file_name(lock_name);
        fs::write(&lock, format!("{oid}\n")).map_err(|e| err!("cannot lock {full}: {e}"))?;
        if let Err(e) = fs::rename(&lock, &path) {
            // Windows cannot rename over an existing file in every situation.
            let _ = fs::remove_file(&path);
            fs::rename(&lock, &path).map_err(|e2| err!("cannot update {full}: {e} / {e2}"))?;
        }
        Ok(())
    }

    pub fn delete_ref(&self, full: &str) -> Result<bool> {
        let mut existed = false;
        let path = self.ref_path(full);
        if path.is_file() {
            fs::remove_file(&path)?;
            existed = true;
            // Remove directories that became empty, up to refs/.
            let mut dir = path.parent().map(Path::to_path_buf);
            while let Some(d) = dir {
                if d == self.git.join("refs")
                    || d == self.git.join("refs/heads")
                    || d == self.git.join("refs/tags")
                    || fs::remove_dir(&d).is_err()
                {
                    break;
                }
                dir = d.parent().map(Path::to_path_buf);
            }
        }
        let packed = self.packed_refs();
        if packed.iter().any(|(n, _)| n == full) {
            existed = true;
            let text = fs::read_to_string(self.git.join("packed-refs"))?;
            let mut out = String::new();
            let mut skip_peeled = false;
            for line in text.lines() {
                if let Some((_, name)) = line.split_once(' ').filter(|_| !line.starts_with('#') && !line.starts_with('^')) {
                    if name == full {
                        skip_peeled = true;
                        continue;
                    }
                }
                if line.starts_with('^') && skip_peeled {
                    skip_peeled = false;
                    continue;
                }
                skip_peeled = false;
                out.push_str(line);
                out.push('\n');
            }
            fs::write(self.git.join("packed-refs"), out)?;
        }
        Ok(existed)
    }

    /// All refs below `prefix` (`refs/heads/`), loose refs overriding packed ones, sorted by name.
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, Oid)>> {
        let mut map: BTreeMap<String, Oid> = self.packed_refs().into_iter().filter(|(n, _)| n.starts_with(prefix)).collect();
        fn walk(repo: &Repo, rel: &str, prefix: &str, map: &mut BTreeMap<String, Oid>) -> Result<()> {
            let dir = repo.git.join(rel);
            let Ok(rd) = fs::read_dir(&dir) else { return Ok(()) };
            for e in rd.flatten() {
                let name = format!("{rel}/{}", e.file_name().to_string_lossy());
                if e.path().is_dir() {
                    walk(repo, &name, prefix, map)?;
                } else if name.starts_with(prefix) && !name.ends_with(".lock") {
                    if let Some(o) = repo.read_ref(&name)? {
                        map.insert(name, o);
                    }
                }
            }
            Ok(())
        }
        let base = prefix.trim_end_matches('/');
        let start = if base.is_empty() { "refs".to_string() } else { base.to_string() };
        walk(self, &start, prefix, &mut map)?;
        Ok(map.into_iter().collect())
    }

    pub fn set_head_branch(&self, name: &str) -> Result<()> {
        if !valid_ref_name(name) {
            return Err(err!("{name:?} is not a valid branch name"));
        }
        fs::write(self.git.join("HEAD"), format!("ref: refs/heads/{name}\n"))?;
        Ok(())
    }

    pub fn set_head_detached(&self, oid: &Oid) -> Result<()> {
        fs::write(self.git.join("HEAD"), format!("{oid}\n"))?;
        Ok(())
    }

    /// Moves the current branch (or detached `HEAD`) to a commit.
    pub fn advance_head(&self, oid: &Oid) -> Result<()> {
        match self.head()? {
            Head::Branch(b) => self.write_ref(&format!("refs/heads/{b}"), oid),
            Head::Detached(_) => self.set_head_detached(oid),
        }
    }

    // ---------------------------------------------------------------- objects

    pub fn read_commit(&self, id: &Oid) -> Result<Commit> {
        Commit::parse(&self.odb.read_kind(id, Kind::Commit)?)
    }

    pub fn read_tree(&self, id: &Oid) -> Result<Tree> {
        Tree::parse(&self.odb.read_kind(id, Kind::Tree)?)
    }

    /// Follows tags until something that is not a tag.
    pub fn peel(&self, id: &Oid) -> Result<Oid> {
        let mut cur = *id;
        for _ in 0..16 {
            let (kind, data) = self.odb.read(&cur)?;
            if kind != Kind::Tag {
                return Ok(cur);
            }
            cur = TagObject::parse(&data)?.object;
        }
        Err(err!("tag chain too long"))
    }

    pub fn peel_to_commit(&self, id: &Oid) -> Result<Oid> {
        let p = self.peel(id)?;
        let (kind, _) = self.odb.read(&p)?;
        if kind != Kind::Commit {
            return Err(err!("{id} is a {}, not a commit", kind.name()));
        }
        Ok(p)
    }

    /// Every file below a tree, as path -> (mode, id). Subdirectories are expanded.
    pub fn tree_files(&self, tree: &Oid) -> Result<BTreeMap<String, (u32, Oid)>> {
        let mut out = BTreeMap::new();
        self.walk_tree(tree, "", &mut out)?;
        Ok(out)
    }

    fn walk_tree(&self, tree: &Oid, prefix: &str, out: &mut BTreeMap<String, (u32, Oid)>) -> Result<()> {
        for e in self.read_tree(tree)?.entries {
            let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            if e.mode == MODE_DIR {
                self.walk_tree(&e.oid, &path, out)?;
            } else {
                out.insert(path, (e.mode, e.oid));
            }
        }
        Ok(())
    }

    /// The files of a commit's tree, or none for `None` (the empty history).
    pub fn commit_files(&self, commit: Option<&Oid>) -> Result<BTreeMap<String, (u32, Oid)>> {
        match commit {
            Some(c) => self.tree_files(&self.read_commit(c)?.tree),
            None => Ok(BTreeMap::new()),
        }
    }

    // ---------------------------------------------------------------- the index file

    pub fn read_index(&self) -> Result<Index> {
        let path = self.git.join("index");
        match fs::read(&path) {
            Ok(bytes) => {
                let mut idx = Index::parse(&bytes)?;
                if let Ok(m) = fs::metadata(&path).and_then(|m| m.modified()) {
                    let d = m.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    idx.written = (d.as_secs() as u32, d.subsec_nanos());
                }
                Ok(idx)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_index(&self, index: &Index) -> Result<()> {
        let bytes = index.serialize()?;
        let path = self.git.join("index");
        let lock = self.git.join("index.lock");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|e| err!("cannot lock the index ({e}); if no other mg process is running, delete {}", lock.display()))?;
        use std::io::Write;
        f.write_all(&bytes)?;
        drop(f);
        if fs::rename(&lock, &path).is_err() {
            let _ = fs::remove_file(&path);
            fs::rename(&lock, &path)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- revisions

    /// Resolves a revision such as `HEAD`, `main~2`, `v1.0^{commit}`, `a1b2c3d` or `HEAD^2`.
    pub fn rev_parse(&self, spec: &str) -> Result<Oid> {
        let split = spec.find(['^', '~']).unwrap_or(spec.len());
        let (base, mut ops) = spec.split_at(split);
        let mut cur = self.resolve_name(if base.is_empty() { "HEAD" } else { base })?;
        while !ops.is_empty() {
            if let Some(rest) = ops.strip_prefix("^{") {
                let end = rest.find('}').ok_or_else(|| err!("bad revision {spec:?}: unterminated ^{{"))?;
                cur = match &rest[..end] {
                    "" => self.peel(&cur)?,
                    "commit" => self.peel_to_commit(&cur)?,
                    "tree" => {
                        let p = self.peel(&cur)?;
                        match self.odb.read(&p)?.0 {
                            Kind::Tree => p,
                            Kind::Commit => self.read_commit(&p)?.tree,
                            k => return Err(err!("{p} is a {}, not a tree", k.name())),
                        }
                    }
                    other => return Err(err!("unsupported peel type {other:?} in {spec:?}")),
                };
                ops = &rest[end + 1..];
            } else if let Some(rest) = ops.strip_prefix('^') {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                let n: usize = if digits == 0 { 1 } else { rest[..digits].parse().map_err(|_| err!("bad revision {spec:?}"))? };
                let commit = self.peel_to_commit(&cur)?;
                cur = if n == 0 {
                    commit
                } else {
                    *self
                        .read_commit(&commit)?
                        .parents
                        .get(n - 1)
                        .ok_or_else(|| err!("{spec:?}: commit {} has no parent number {n}", commit.short()))?
                };
                ops = &rest[digits..];
            } else if let Some(rest) = ops.strip_prefix('~') {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                let n: usize = if digits == 0 { 1 } else { rest[..digits].parse().map_err(|_| err!("bad revision {spec:?}"))? };
                for _ in 0..n {
                    let commit = self.peel_to_commit(&cur)?;
                    cur = *self
                        .read_commit(&commit)?
                        .parents
                        .first()
                        .ok_or_else(|| err!("{spec:?}: ran out of history at {}", commit.short()))?;
                }
                ops = &rest[digits..];
            } else {
                return Err(err!("bad revision {spec:?}"));
            }
        }
        Ok(cur)
    }

    fn resolve_name(&self, name: &str) -> Result<Oid> {
        if name == "HEAD" || name == "@" {
            return self.head_commit()?.ok_or_else(|| err!("HEAD does not point at a commit yet (no commits)"));
        }
        // A ref wins over an abbreviated id with the same spelling, like git; a full id wins over everything.
        if let Some(id) = Oid::from_hex(name) {
            return if self.odb.exists(&id) { Ok(id) } else { Err(err!("object {id} not found")) };
        }
        for candidate in [
            name.to_string(),
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/remotes/{name}"),
        ] {
            if valid_ref_name(&candidate) {
                if let Some(id) = self.read_ref(&candidate)? {
                    return Ok(id);
                }
            }
        }
        if name.len() >= 4 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            return self.odb.resolve_prefix(name);
        }
        Err(err!("unknown revision {name:?}"))
    }
}

/// Parses `GIT_*_DATE` values: git's raw form `1700000000 +0100` or ISO 8601 like
/// `2024-03-01T12:30:00+0200` and `2024-03-01 12:30:00 +0000` (a missing zone means UTC).
pub fn parse_date(s: &str) -> Result<(i64, String)> {
    let s = s.trim();
    let raw = s.strip_prefix('@').unwrap_or(s);
    if let Some((secs, tz)) = raw.split_once(' ') {
        if let (Ok(n), true) = (secs.parse::<i64>(), valid_tz(tz)) {
            return Ok((n, tz.to_string()));
        }
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Ok((n, "+0000".to_string()));
    }
    // ISO 8601
    let (date, rest) = s.split_once(['T', ' ']).ok_or_else(|| err!("unrecognised date {s:?}"))?;
    let d: Vec<i64> = date.split('-').map(|p| p.parse().unwrap_or(-1)).collect();
    let rest = rest.trim();
    let (time, tz) = match rest.find(['+', '-', 'Z']) {
        Some(i) => (rest[..i].trim(), rest[i..].trim()),
        None => (rest, "+0000"),
    };
    let tz = if tz == "Z" { "+0000" } else { tz };
    let t: Vec<i64> = time.split(':').map(|p| p.parse().unwrap_or(-1)).collect();
    if d.len() != 3 || t.len() < 2 || d.iter().chain(t.iter()).any(|v| *v < 0) || !valid_tz(tz) {
        return Err(err!("unrecognised date {s:?}"));
    }
    let (y, m, day) = (d[0], d[1] as u32, d[2] as u32);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || t[0] > 23 || t[1] > 59 || t.get(2).is_some_and(|s| *s > 60) {
        return Err(err!("unrecognised date {s:?}"));
    }
    let days = days_from_civil(y, m, day);
    let local = days * 86_400 + t[0] * 3600 + t[1] * 60 + t.get(2).copied().unwrap_or(0);
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let offset = sign * (tz[1..3].parse::<i64>().unwrap_or(0) * 3600 + tz[3..5].parse::<i64>().unwrap_or(0) * 60);
    Ok((local - offset, tz.to_string()))
}

fn valid_tz(tz: &str) -> bool {
    tz.len() == 5 && (tz.starts_with('+') || tz.starts_with('-')) && tz[1..].bytes().all(|b| b.is_ascii_digit())
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names() {
        for good in ["main", "feature/x", "v1.0", "a-b_c", "release/1.2.3", "ünï"] {
            assert!(valid_ref_name(good), "{good}");
        }
        for bad in [
            "", "@", "/a", "a/", "a//b", "a..b", ".hidden", "a/.b", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b",
            "x.lock", "a/x.lock", "end.", "a@{b", "tab\t",
        ] {
            assert!(!valid_ref_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn safe_paths() {
        for good in ["a", "a/b.txt", "dir/.gitignore", ".gitattributes", "gitfile", "my.git.txt", "a b/c"] {
            assert!(path_is_safe(good), "{good}");
        }
        for bad in [
            "",
            "/a",
            "a//b",
            "../a",
            "a/../b",
            "./a",
            ".git",
            ".git/config",
            "a/.git/x",
            ".GIT/x",
            ".git.",
            ".git ",
            "git~1/x",
            "a\\b",
            "c:x",
            "a/b:stream",
            "a\u{1}b",
        ] {
            assert!(!path_is_safe(bad), "{bad:?}");
        }
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("1700000000 +0100").unwrap(), (1_700_000_000, "+0100".to_string()));
        assert_eq!(parse_date("@1700000000 -0830").unwrap(), (1_700_000_000, "-0830".to_string()));
        assert_eq!(parse_date("1700000000").unwrap(), (1_700_000_000, "+0000".to_string()));
        assert_eq!(parse_date("2023-11-14T22:13:20Z").unwrap(), (1_700_000_000, "+0000".to_string()));
        assert_eq!(parse_date("2023-11-14 23:13:20 +0100").unwrap(), (1_700_000_000, "+0100".to_string()));
        assert_eq!(parse_date("2023-11-14T17:43:20-0430").unwrap(), (1_700_000_000, "-0430".to_string()));
        assert_eq!(parse_date("1970-01-01T00:00:00").unwrap(), (0, "+0000".to_string()));
        for bad in ["", "yesterday", "2023-13-01T00:00:00", "2023-11-14T25:00:00", "2023-11-14T22:13:20 +99", "1700000000 0100"] {
            assert!(parse_date(bad).is_err(), "{bad:?}");
        }
    }

    fn repo() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(d.path(), "main").unwrap().unwrap();
        (d, r)
    }

    #[test]
    fn init_and_open() {
        let (d, r) = repo();
        assert_eq!(r.head().unwrap(), Head::Branch("main".into()));
        assert_eq!(r.head_commit().unwrap(), None);
        assert!(Repo::init(d.path(), "main").unwrap().is_none(), "second init reports the repository exists");
        assert!(Repo::open(d.path()).is_ok());
        let sub = d.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(Repo::discover(&sub).unwrap().work, r.work);
        assert!(Repo::open(&sub).is_err());
        assert!(Repo::init(d.path().join("a").as_path(), "bad name").is_err());
    }

    #[test]
    fn refs_loose_and_packed() {
        let (_d, r) = repo();
        let a = crate::sha1::sha1(b"a");
        let b = crate::sha1::sha1(b"b");
        r.write_ref("refs/heads/main", &a).unwrap();
        r.write_ref("refs/heads/feature/x", &b).unwrap();
        assert_eq!(r.read_ref("refs/heads/main").unwrap(), Some(a));
        assert_eq!(r.head_commit().unwrap(), Some(a));
        assert_eq!(
            r.list_refs("refs/heads/").unwrap(),
            vec![("refs/heads/feature/x".to_string(), b), ("refs/heads/main".to_string(), a)]
        );
        // Moving a ref.
        r.write_ref("refs/heads/main", &b).unwrap();
        assert_eq!(r.read_ref("refs/heads/main").unwrap(), Some(b));
        // Directory/file conflicts.
        assert!(r.write_ref("refs/heads/main/sub", &a).is_err());
        assert!(r.write_ref("refs/heads/feature", &a).is_err());
        assert!(r.write_ref("refs/heads/bad name", &a).is_err());
        assert!(r.write_ref("heads/x", &a).is_err());
        // Packed refs are read, and a loose ref overrides them.
        fs::write(
            r.git.join("packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n{a} refs/tags/v1\n{a} refs/heads/packed\n{a} refs/heads/main\n"
            ),
        )
        .unwrap();
        assert_eq!(r.read_ref("refs/tags/v1").unwrap(), Some(a));
        assert_eq!(r.read_ref("refs/heads/main").unwrap(), Some(b), "loose beats packed");
        let names: Vec<String> = r.list_refs("refs/heads/").unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["refs/heads/feature/x", "refs/heads/main", "refs/heads/packed"]);
        // Deleting removes both the loose and the packed entry.
        assert!(r.delete_ref("refs/heads/main").unwrap());
        assert_eq!(r.read_ref("refs/heads/main").unwrap(), None);
        assert!(r.delete_ref("refs/heads/packed").unwrap());
        assert!(!r.delete_ref("refs/heads/packed").unwrap());
        assert!(r.delete_ref("refs/heads/feature/x").unwrap());
        assert!(!r.git.join("refs/heads/feature").exists(), "the empty directory is removed");
    }

    #[test]
    fn head_states() {
        let (_d, r) = repo();
        let a = crate::sha1::sha1(b"a");
        r.set_head_detached(&a).unwrap();
        assert_eq!(r.head().unwrap(), Head::Detached(a));
        r.set_head_branch("topic").unwrap();
        assert_eq!(r.head().unwrap(), Head::Branch("topic".into()));
        assert!(r.set_head_branch("a b").is_err());
        fs::write(r.git.join("HEAD"), "garbage\n").unwrap();
        assert!(r.head().is_err());
    }
}

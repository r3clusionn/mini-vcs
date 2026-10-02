//! The index (staging area), in git's binary format, version 2.
//!
//! ```text
//! "DIRC" version(4) count(4)
//! entry*   ctime(8) mtime(8) dev ino mode uid gid size (4 each) id(20) flags(2) path NUL padding
//! extension*  signature(4) size(4) data
//! sha1(20) of everything before it
//! ```
//!
//! Entries are sorted by path, then stage. Stage 0 is a normal entry; stages 1, 2 and 3 are the
//! base, ours and theirs versions of a file that a merge could not resolve.
//!
//! Optional extensions (the tree cache, the resolve-undo record) are dropped when the index is
//! rewritten, which git tolerates and rebuilds. An index with a version other than 2 or 3, or
//! with an extension that git marks as required, is refused rather than damaged.

use std::collections::BTreeMap;

use crate::err;
use crate::error::Result;
use crate::sha1::{sha1, Oid};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ctime: (u32, u32),
    pub mtime: (u32, u32),
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: Oid,
    pub path: String,
    pub stage: u8,
}

impl Entry {
    pub fn new(path: &str, mode: u32, oid: Oid) -> Entry {
        Entry {
            ctime: (0, 0),
            mtime: (0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            path: path.to_string(),
            stage: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    entries: BTreeMap<(String, u8), Entry>,
    /// Seconds and nanoseconds when the index file was last written, for the "racily clean" check.
    pub written: (u32, u32),
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(b.get(at..at + 4).ok_or_else(|| err!("corrupt index: truncated"))?.try_into().unwrap()))
}

impl Index {
    pub fn new() -> Index {
        Index::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// The merged (stage 0) entry for a path.
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(&(path.to_string(), 0))
    }

    pub fn get_stage(&self, path: &str, stage: u8) -> Option<&Entry> {
        self.entries.get(&(path.to_string(), stage))
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries.keys().any(|(_, s)| *s != 0)
    }

    pub fn conflicted_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.entries.keys().filter(|(_, s)| *s != 0).map(|(p, _)| p.clone()).collect();
        v.dedup();
        v
    }

    /// Stages an entry. A file replaces any directory of the same name (and the other way
    /// round), and any conflict stages for the path.
    pub fn add(&mut self, entry: Entry) {
        let path = entry.path.clone();
        // A file `a` cannot coexist with `a/b`.
        let prefix = format!("{path}/");
        let below: Vec<(String, u8)> = self
            .entries
            .range((prefix.clone(), 0)..)
            .take_while(|((p, _), _)| p.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for k in below {
            self.entries.remove(&k);
        }
        let mut parent = path.as_str();
        while let Some(i) = parent.rfind('/') {
            parent = &parent[..i];
            self.entries.remove(&(parent.to_string(), 0));
        }
        if entry.stage == 0 {
            for s in 1..=3 {
                self.entries.remove(&(path.clone(), s));
            }
        }
        self.entries.insert((path, entry.stage), entry);
    }

    /// Removes every stage of a path; returns whether anything was removed.
    pub fn remove(&mut self, path: &str) -> bool {
        let mut any = false;
        for s in 0..=3 {
            any |= self.entries.remove(&(path.to_string(), s)).is_some();
        }
        any
    }

    /// Removes a path and everything below it.
    pub fn remove_tree(&mut self, dir: &str) -> usize {
        let prefix = format!("{dir}/");
        let keys: Vec<(String, u8)> = self.entries.keys().filter(|(p, _)| p == dir || p.starts_with(&prefix)).cloned().collect();
        for k in &keys {
            self.entries.remove(k);
        }
        keys.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn parse(data: &[u8]) -> Result<Index> {
        if data.len() < 12 + 20 {
            return Err(err!("corrupt index: too short"));
        }
        let (body, trailer) = data.split_at(data.len() - 20);
        if sha1(body).0 != trailer {
            return Err(err!("corrupt index: checksum does not match"));
        }
        if &body[..4] != b"DIRC" {
            return Err(err!("corrupt index: bad signature"));
        }
        let version = be32(body, 4)?;
        if version != 2 && version != 3 {
            return Err(err!("index version {version} is not supported (only 2 and 3)"));
        }
        let count = be32(body, 8)? as usize;
        let mut entries = BTreeMap::new();
        let mut at = 12;
        let mut last: Option<(String, u8)> = None;
        for _ in 0..count {
            let start = at;
            let word = |i: usize| be32(body, start + i * 4);
            let (ctime, mtime) = ((word(0)?, word(1)?), (word(2)?, word(3)?));
            let (dev, ino, mode, uid, gid, size) = (word(4)?, word(5)?, word(6)?, word(7)?, word(8)?, word(9)?);
            let oid =
                Oid(body.get(start + 40..start + 60).ok_or_else(|| err!("corrupt index: truncated entry"))?.try_into().unwrap());
            let flags = u16::from_be_bytes(
                body.get(start + 60..start + 62).ok_or_else(|| err!("corrupt index: truncated entry"))?.try_into().unwrap(),
            );
            let mut name_at = start + 62;
            if flags & 0x4000 != 0 {
                if version < 3 {
                    return Err(err!("corrupt index: extended flags in a version 2 index"));
                }
                name_at += 2;
            }
            let nul = body
                .get(name_at..)
                .and_then(|b| b.iter().position(|c| *c == 0))
                .ok_or_else(|| err!("corrupt index: unterminated path"))?
                + name_at;
            let path =
                std::str::from_utf8(&body[name_at..nul]).map_err(|_| err!("corrupt index: path is not UTF-8"))?.to_string();
            if path.is_empty()
                || path.starts_with('/')
                || path.split('/').any(|c| c.is_empty() || c == "." || c == ".." || c.eq_ignore_ascii_case(".git"))
            {
                return Err(err!("corrupt index: unsafe path {path:?}"));
            }
            let stage = ((flags >> 12) & 3) as u8;
            let key = (path.clone(), stage);
            if last.as_ref().is_some_and(|l| *l >= key) {
                return Err(err!("corrupt index: entries out of order at {path:?}"));
            }
            last = Some(key.clone());
            entries.insert(key, Entry { ctime, mtime, dev, ino, mode, uid, gid, size, oid, path, stage });
            // Entry length is padded with NULs to a multiple of eight bytes (at least one).
            at = start + ((nul - start + 8) & !7);
        }
        // Extensions.
        while at + 8 <= body.len() {
            let sig = &body[at..at + 4];
            let size = be32(body, at + 4)? as usize;
            if !sig[0].is_ascii_uppercase() {
                return Err(err!("index has a required extension {:?} that is not supported", String::from_utf8_lossy(sig)));
            }
            at = at
                .checked_add(8 + size)
                .filter(|e| *e <= body.len())
                .ok_or_else(|| err!("corrupt index: extension overruns the file"))?;
        }
        if at != body.len() {
            return Err(err!("corrupt index: trailing bytes"));
        }
        Ok(Index { entries, written: (0, 0) })
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend(b"DIRC");
        out.extend(2u32.to_be_bytes());
        out.extend((self.entries.len() as u32).to_be_bytes());
        for e in self.entries.values() {
            let start = out.len();
            for v in [e.ctime.0, e.ctime.1, e.mtime.0, e.mtime.1, e.dev, e.ino, e.mode, e.uid, e.gid, e.size] {
                out.extend(v.to_be_bytes());
            }
            out.extend(e.oid.0);
            let name_len = e.path.len().min(0xFFF) as u16;
            out.extend((((e.stage as u16) << 12) | name_len).to_be_bytes());
            out.extend(e.path.as_bytes());
            let used = out.len() - start;
            out.extend(std::iter::repeat_n(0u8, ((used + 8) & !7) - used));
        }
        let sum = sha1(&out);
        out.extend(sum.0);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{MODE_EXEC, MODE_FILE};

    fn entry(path: &str, n: u8) -> Entry {
        let mut e = Entry::new(path, MODE_FILE, sha1(&[n]));
        e.size = n as u32;
        e.mtime = (1_700_000_000 + n as u32, 123);
        e
    }

    #[test]
    fn round_trip() {
        let mut idx = Index::new();
        for (i, p) in ["b.txt", "a/z.rs", "a/b/c.rs", "a.txt", "dir with space/f", "é/ü.txt"].iter().enumerate() {
            idx.add(entry(p, i as u8));
        }
        let mut exec = entry("run.sh", 99);
        exec.mode = MODE_EXEC;
        idx.add(exec);
        let bytes = idx.serialize().unwrap();
        let back = Index::parse(&bytes).unwrap();
        assert_eq!(back.entries().cloned().collect::<Vec<_>>(), idx.entries().cloned().collect::<Vec<_>>());
        assert_eq!(back.serialize().unwrap(), bytes);
        // Sorted by path bytes.
        let paths: Vec<&str> = back.entries().map(|e| e.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn entry_padding_is_a_multiple_of_eight() {
        for name_len in 1..40 {
            let mut idx = Index::new();
            idx.add(entry(&"x".repeat(name_len), 1));
            let bytes = idx.serialize().unwrap();
            // header 12 + entry + checksum 20
            assert_eq!((bytes.len() - 12 - 20) % 8, 0, "name of {name_len} bytes");
            assert!(Index::parse(&bytes).is_ok());
        }
    }

    #[test]
    fn corruption_is_detected() {
        let mut idx = Index::new();
        idx.add(entry("a", 1));
        idx.add(entry("b", 2));
        let good = idx.serialize().unwrap();
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 1;
            assert!(Index::parse(&bad).is_err(), "byte {i}");
        }
        for cut in 0..good.len() {
            assert!(Index::parse(&good[..cut]).is_err(), "cut {cut}");
        }
    }

    /// Builds an index byte string by hand (with a valid checksum) to test the validation paths.
    fn forged(version: u32, entries: &[(&str, u8)], extension: Option<(&[u8; 4], &[u8])>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(b"DIRC");
        out.extend(version.to_be_bytes());
        out.extend((entries.len() as u32).to_be_bytes());
        for (path, stage) in entries {
            let start = out.len();
            out.extend([0u8; 24]);
            out.extend(MODE_FILE.to_be_bytes());
            out.extend([0u8; 12]);
            out.extend([7u8; 20]);
            out.extend((((*stage as u16) << 12) | path.len() as u16).to_be_bytes());
            out.extend(path.as_bytes());
            let used = out.len() - start;
            out.extend(std::iter::repeat_n(0u8, ((used + 8) & !7) - used));
        }
        if let Some((sig, data)) = extension {
            out.extend(sig);
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(data);
        }
        let sum = sha1(&out);
        out.extend(sum.0);
        out
    }

    #[test]
    fn forged_indexes() {
        assert!(Index::parse(&forged(2, &[("a", 0), ("b", 0)], None)).is_ok());
        assert!(Index::parse(&forged(3, &[("a", 0)], None)).is_ok());
        assert!(Index::parse(&forged(4, &[("a", 0)], None)).unwrap_err().msg.contains("not supported"));
        // Unsafe paths.
        for bad in ["../x", "a/../b", "/abs", "a//b", ".git/config", "a/.GIT/x", "./x", ""] {
            assert!(Index::parse(&forged(2, &[(bad, 0)], None)).is_err(), "{bad:?}");
        }
        // Out of order and duplicate entries.
        assert!(Index::parse(&forged(2, &[("b", 0), ("a", 0)], None)).is_err());
        assert!(Index::parse(&forged(2, &[("a", 0), ("a", 0)], None)).is_err());
        // Optional extensions are skipped, required ones are refused.
        assert!(Index::parse(&forged(2, &[("a", 0)], Some((b"TREE", b"whatever")))).is_ok());
        assert!(Index::parse(&forged(2, &[("a", 0)], Some((b"link", b"split index"))))
            .unwrap_err()
            .msg
            .contains("required extension"));
        assert!(Index::parse(&forged(2, &[("a", 0)], Some((b"TREE", &[])))).is_ok());
    }

    #[test]
    fn staging_replaces_files_and_directories() {
        let mut idx = Index::new();
        idx.add(entry("a/b", 1));
        idx.add(entry("a/c/d", 2));
        idx.add(entry("x", 3));
        // A file called `a` replaces the directory `a/`.
        idx.add(entry("a", 4));
        assert_eq!(idx.entries().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["a", "x"]);
        // And a path below a file replaces the file.
        idx.add(entry("x/y", 5));
        assert_eq!(idx.entries().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["a", "x/y"]);
    }

    #[test]
    fn conflict_stages() {
        let mut idx = Index::new();
        for stage in 1..=3u8 {
            let mut e = entry("f", stage);
            e.stage = stage;
            idx.add(e);
        }
        assert!(idx.has_conflicts());
        assert_eq!(idx.conflicted_paths(), vec!["f"]);
        assert!(idx.get("f").is_none());
        assert!(idx.get_stage("f", 2).is_some());
        let back = Index::parse(&idx.serialize().unwrap()).unwrap();
        assert_eq!(back.len(), 3);
        // Resolving: adding a stage 0 entry clears the others.
        idx.add(entry("f", 9));
        assert!(!idx.has_conflicts());
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn remove_tree_only_touches_that_directory() {
        let mut idx = Index::new();
        for p in ["d/a", "d/b/c", "d.txt", "dd/x", "e"] {
            idx.add(entry(p, 1));
        }
        assert_eq!(idx.remove_tree("d"), 2);
        assert_eq!(idx.entries().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["d.txt", "dd/x", "e"]);
        assert!(idx.remove("e") && !idx.remove("e"));
    }
}

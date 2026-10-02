//! Git's four object types and their byte formats.

use crate::err;
use crate::error::Result;
use crate::sha1::{Oid, Sha1};

pub const MODE_FILE: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
pub const MODE_DIR: u32 = 0o040000;
pub const MODE_LINK: u32 = 0o120000;
pub const MODE_GITLINK: u32 = 0o160000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "blob" => Kind::Blob,
            "tree" => Kind::Tree,
            "commit" => Kind::Commit,
            "tag" => Kind::Tag,
            _ => return None,
        })
    }
}

/// The id of an object: SHA-1 of `"<kind> <length>\0"` followed by the content.
pub fn object_id(kind: Kind, data: &[u8]) -> Oid {
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", kind.name(), data.len()).as_bytes());
    h.update(data);
    h.finish()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub oid: Oid,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        self.mode == MODE_DIR
    }

    /// Git sorts tree entries by name, comparing a directory as if its name ended in `/`.
    fn sort_key(&self) -> Vec<u8> {
        let mut k = self.name.clone().into_bytes();
        if self.is_dir() {
            k.push(b'/');
        }
        k
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// The tree's bytes: for each entry in order, `"<octal mode> <name>\0"` and the 20-byte id.
    pub fn serialize(&self) -> Vec<u8> {
        let mut entries: Vec<&TreeEntry> = self.entries.iter().collect();
        entries.sort_by_key(|e| e.sort_key());
        let mut out = Vec::new();
        for e in entries {
            out.extend(format!("{:o} {}", e.mode, e.name).bytes());
            out.push(0);
            out.extend(e.oid.0);
        }
        out
    }

    pub fn parse(data: &[u8]) -> Result<Tree> {
        let mut entries = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let sp = data[i..].iter().position(|b| *b == b' ').ok_or_else(|| err!("corrupt tree: no space after mode"))? + i;
            let mode_str = std::str::from_utf8(&data[i..sp]).map_err(|_| err!("corrupt tree: bad mode"))?;
            let mode = u32::from_str_radix(mode_str, 8).map_err(|_| err!("corrupt tree: bad mode {mode_str:?}"))?;
            let nul = data[sp..].iter().position(|b| *b == 0).ok_or_else(|| err!("corrupt tree: no NUL after name"))? + sp;
            let name =
                std::str::from_utf8(&data[sp + 1..nul]).map_err(|_| err!("corrupt tree: file name is not UTF-8"))?.to_string();
            let id_bytes: [u8; 20] =
                data.get(nul + 1..nul + 21).ok_or_else(|| err!("corrupt tree: truncated id"))?.try_into().unwrap();
            if name.is_empty() {
                return Err(err!("corrupt tree: empty file name"));
            }
            entries.push(TreeEntry { mode, name, oid: Oid(id_bytes) });
            i = nul + 21;
        }
        Ok(Tree { entries })
    }

    pub fn find(&self, name: &str) -> Option<&TreeEntry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

/// An author or committer line: `Name <email> 1700000000 +0100`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub when: i64,
    pub tz: String,
}

impl Signature {
    pub fn format(&self) -> String {
        format!("{} <{}> {} {}", self.name, self.email, self.when, self.tz)
    }

    pub fn parse(s: &str) -> Result<Signature> {
        let lt = s.find('<').ok_or_else(|| err!("corrupt signature {s:?}"))?;
        let gt = s.rfind('>').ok_or_else(|| err!("corrupt signature {s:?}"))?;
        if gt < lt {
            return Err(err!("corrupt signature {s:?}"));
        }
        let rest = s[gt + 1..].trim();
        let (when, tz) = rest.split_once(' ').ok_or_else(|| err!("corrupt signature date {s:?}"))?;
        Ok(Signature {
            name: s[..lt].trim_end().to_string(),
            email: s[lt + 1..gt].to_string(),
            when: when.parse().map_err(|_| err!("corrupt signature time {s:?}"))?,
            tz: tz.to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    /// Headers other than the ones above (`gpgsig`, `encoding`, ...), kept so that a commit
    /// written by git can be read and written back to the same bytes.
    pub extra: Vec<(String, String)>,
    /// The message exactly as stored, including the final newline.
    pub message: String,
}

impl Commit {
    pub fn serialize(&self) -> Vec<u8> {
        let mut s = format!("tree {}\n", self.tree);
        for p in &self.parents {
            s.push_str(&format!("parent {p}\n"));
        }
        s.push_str(&format!("author {}\ncommitter {}\n", self.author.format(), self.committer.format()));
        for (k, v) in &self.extra {
            // A continuation line starts with a space.
            s.push_str(&format!("{k} {}\n", v.replace('\n', "\n ")));
        }
        s.push('\n');
        s.push_str(&self.message);
        s.into_bytes()
    }

    pub fn parse(data: &[u8]) -> Result<Commit> {
        let text = String::from_utf8_lossy(data);
        let (head, message) = match text.split_once("\n\n") {
            Some((h, m)) => (h.to_string(), m.to_string()),
            None => (text.trim_end_matches('\n').to_string(), String::new()),
        };
        let (mut tree, mut parents, mut author, mut committer) = (None, Vec::new(), None, None);
        let mut extra: Vec<(String, String)> = Vec::new();
        for line in head.split('\n') {
            if let Some(cont) = line.strip_prefix(' ') {
                let last = extra.last_mut().ok_or_else(|| err!("corrupt commit: continuation line without a header"))?;
                last.1.push('\n');
                last.1.push_str(cont);
                continue;
            }
            let (k, v) = line.split_once(' ').ok_or_else(|| err!("corrupt commit header {line:?}"))?;
            match k {
                "tree" => tree = Some(Oid::from_hex(v).ok_or_else(|| err!("corrupt commit: bad tree id"))?),
                "parent" => parents.push(Oid::from_hex(v).ok_or_else(|| err!("corrupt commit: bad parent id"))?),
                "author" => author = Some(Signature::parse(v)?),
                "committer" => committer = Some(Signature::parse(v)?),
                _ => extra.push((k.to_string(), v.to_string())),
            }
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| err!("corrupt commit: no tree"))?,
            parents,
            author: author.ok_or_else(|| err!("corrupt commit: no author"))?,
            committer: committer.ok_or_else(|| err!("corrupt commit: no committer"))?,
            extra,
            message,
        })
    }

    /// The first line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// An annotated tag object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagObject {
    pub object: Oid,
    pub kind: Kind,
    pub name: String,
    pub tagger: Option<Signature>,
    pub message: String,
}

impl TagObject {
    pub fn parse(data: &[u8]) -> Result<TagObject> {
        let text = String::from_utf8_lossy(data);
        let (head, message) =
            text.split_once("\n\n").map(|(h, m)| (h.to_string(), m.to_string())).unwrap_or((text.to_string(), String::new()));
        let (mut object, mut kind, mut name, mut tagger) = (None, None, None, None);
        for line in head.split('\n') {
            let (k, v) = line.split_once(' ').ok_or_else(|| err!("corrupt tag header {line:?}"))?;
            match k {
                "object" => object = Oid::from_hex(v),
                "type" => kind = Kind::parse(v),
                "tag" => name = Some(v.to_string()),
                "tagger" => tagger = Signature::parse(v).ok(),
                _ => {}
            }
        }
        Ok(TagObject {
            object: object.ok_or_else(|| err!("corrupt tag: no object"))?,
            kind: kind.ok_or_else(|| err!("corrupt tag: no type"))?,
            name: name.ok_or_else(|| err!("corrupt tag: no name"))?,
            tagger,
            message,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut s = format!("object {}\ntype {}\ntag {}\n", self.object, self.kind.name(), self.name);
        if let Some(t) = &self.tagger {
            s.push_str(&format!("tagger {}\n", t.format()));
        }
        s.push('\n');
        s.push_str(&self.message);
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha1::sha1;

    fn id(n: u8) -> Oid {
        sha1(&[n])
    }

    #[test]
    fn well_known_object_ids() {
        // `git hash-object` of these contents.
        assert_eq!(object_id(Kind::Blob, b"").hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(object_id(Kind::Blob, b"hello\n").hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(object_id(Kind::Tree, b"").hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn tree_order_treats_directories_as_if_they_ended_in_a_slash() {
        // "a.b" < "a/" < "a0": a directory `a` sorts between `a.b` and `a0`, though "a" < "a.b" as plain names.
        let t = Tree {
            entries: vec![
                TreeEntry { mode: MODE_FILE, name: "a0".into(), oid: id(1) },
                TreeEntry { mode: MODE_DIR, name: "a".into(), oid: id(2) },
                TreeEntry { mode: MODE_FILE, name: "a.b".into(), oid: id(3) },
                TreeEntry { mode: MODE_FILE, name: "a".into(), oid: id(4) }, // a file and a directory cannot share a name; sorts first
            ],
        };
        let parsed = Tree::parse(&t.serialize()).unwrap();
        let names: Vec<(&str, u32)> = parsed.entries.iter().map(|e| (e.name.as_str(), e.mode)).collect();
        assert_eq!(names, vec![("a", MODE_FILE), ("a.b", MODE_FILE), ("a", MODE_DIR), ("a0", MODE_FILE)]);
    }

    #[test]
    fn directory_mode_has_no_leading_zero() {
        let t = Tree { entries: vec![TreeEntry { mode: MODE_DIR, name: "d".into(), oid: id(1) }] };
        assert!(t.serialize().starts_with(b"40000 d\0"));
    }

    #[test]
    fn tree_round_trip_and_corruption() {
        let t = Tree {
            entries: vec![
                TreeEntry { mode: MODE_EXEC, name: "run.sh".into(), oid: id(5) },
                TreeEntry { mode: MODE_LINK, name: "l".into(), oid: id(6) },
            ],
        };
        let bytes = t.serialize();
        assert_eq!(Tree::parse(&bytes).unwrap().serialize(), bytes);
        for cut in 1..bytes.len() {
            // Any truncation is an error or ends exactly at an entry boundary; never a panic.
            let _ = Tree::parse(&bytes[..cut]);
        }
        assert!(Tree::parse(b"100644 name").is_err());
        assert!(Tree::parse(b"zzz name\0aaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(Tree::parse(b"100644 \0aaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(Tree::parse(b"100644 n\0short").is_err());
    }

    fn commit() -> Commit {
        let sig = Signature { name: "A U Thor".into(), email: "a@example.com".into(), when: 1_700_000_000, tz: "+0100".into() };
        Commit {
            tree: id(1),
            parents: vec![id(2), id(3)],
            author: sig.clone(),
            committer: Signature { when: 1_700_000_100, ..sig },
            extra: vec![],
            message: "subject\n\nbody\n".into(),
        }
    }

    #[test]
    fn commit_round_trip() {
        let c = commit();
        let bytes = c.serialize();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with(&format!(
            "tree {}\nparent {}\nparent {}\nauthor A U Thor <a@example.com> 1700000000 +0100\ncommitter ",
            id(1),
            id(2),
            id(3)
        )));
        assert!(text.ends_with("\n\nsubject\n\nbody\n"));
        assert_eq!(Commit::parse(&bytes).unwrap(), c);
        assert_eq!(c.subject(), "subject");
    }

    #[test]
    fn extra_headers_survive_a_round_trip() {
        let raw = format!(
            "tree {}\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nmsg\n",
            id(1)
        );
        let c = Commit::parse(raw.as_bytes()).unwrap();
        assert_eq!(c.extra.len(), 1);
        assert!(c.extra[0].1.contains("abc"));
        assert_eq!(c.serialize(), raw.as_bytes());
    }

    #[test]
    fn signatures() {
        let s = Signature::parse("Jane Q. Public <jane@example.com> 1234567890 -0830").unwrap();
        assert_eq!(
            (s.name.as_str(), s.email.as_str(), s.when, s.tz.as_str()),
            ("Jane Q. Public", "jane@example.com", 1234567890, "-0830")
        );
        assert_eq!(s.format(), "Jane Q. Public <jane@example.com> 1234567890 -0830");
        let empty_name = Signature::parse("<x@y> 5 +0000").unwrap();
        assert_eq!(empty_name.name, "");
        for bad in ["no email 1 +0000", "A <a@b> notatime +0000", "A <a@b>", "A >a@b< 1 +0000"] {
            assert!(Signature::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bad_commits() {
        assert!(Commit::parse(b"author A <a@b> 1 +0000\n\nm").is_err());
        assert!(Commit::parse(b"tree zz\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nm").is_err());
        assert!(Commit::parse(b"").is_err());
    }

    #[test]
    fn tag_objects() {
        let t = TagObject { object: id(9), kind: Kind::Commit, name: "v1".into(), tagger: None, message: "release\n".into() };
        assert_eq!(TagObject::parse(&t.serialize()).unwrap(), t);
    }
}

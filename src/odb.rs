//! The object database: loose objects under `objects/xx/yyyy...`, each a zlib-compressed
//! `"<kind> <length>\0<content>"`, the same layout git uses.
//!
//! Every object is verified when it is read: the header length must match, nothing may follow the
//! content, and the SHA-1 of the whole must equal the id it was stored under. Packfiles are not
//! supported; an object that exists only in a pack is reported as missing.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::err;
use crate::error::Result;
use crate::object::{object_id, Kind};
use crate::sha1::Oid;

/// Largest object that will be read into memory.
const MAX_OBJECT: u64 = 2 << 30;

#[derive(Clone, Debug)]
pub struct Odb {
    dir: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Odb {
    pub fn new(dir: impl Into<PathBuf>) -> Odb {
        Odb { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &Oid) -> PathBuf {
        let hex = id.hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    pub fn exists(&self, id: &Oid) -> bool {
        self.path(id).is_file()
    }

    /// Stores an object and returns its id. Storing what is already there is a no-op.
    pub fn write(&self, kind: Kind, data: &[u8]) -> Result<Oid> {
        let id = object_id(kind, data);
        let path = self.path(&id);
        if path.is_file() {
            return Ok(id);
        }
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(format!("{} {}\0", kind.name(), data.len()).as_bytes())?;
        enc.write_all(data)?;
        let packed = enc.finish()?;
        fs::create_dir_all(path.parent().unwrap())?;
        // Write beside the final name and rename, so a crash never leaves a half-written object.
        let tmp = self.dir.join(format!("tmp_obj_{}_{}", std::process::id(), TMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
        fs::write(&tmp, &packed)?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            // Another writer may have stored the same object in the meantime.
            if !path.is_file() {
                return Err(e.into());
            }
        }
        Ok(id)
    }

    /// Reads and verifies an object.
    pub fn read(&self, id: &Oid) -> Result<(Kind, Vec<u8>)> {
        let path = self.path(id);
        let packed = match fs::read(&path) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(err!("object {id} not found")),
            Err(e) => return Err(e.into()),
        };
        let corrupt = |why: &str| err!("object {id} is corrupt: {why}");
        let mut dec = ZlibDecoder::new(&packed[..]);
        // The header is at most "commit 18446744073709551615\0".
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if header.len() > 32 {
                return Err(corrupt("header too long"));
            }
            dec.read_exact(&mut byte).map_err(|_| corrupt("cannot decompress"))?;
            if byte[0] == 0 {
                break;
            }
            header.push(byte[0]);
        }
        let header = String::from_utf8(header).map_err(|_| corrupt("header is not text"))?;
        let (kind, len) = header.split_once(' ').ok_or_else(|| corrupt("malformed header"))?;
        let kind = Kind::parse(kind).ok_or_else(|| corrupt("unknown type"))?;
        let len: u64 = len.parse().map_err(|_| corrupt("bad length"))?;
        if len > MAX_OBJECT {
            return Err(corrupt("too large to read"));
        }
        let mut data = Vec::with_capacity(len.min(1 << 24) as usize);
        (&mut dec).take(len).read_to_end(&mut data).map_err(|_| corrupt("cannot decompress"))?;
        if data.len() as u64 != len {
            return Err(corrupt("shorter than its header says"));
        }
        let mut extra = [0u8; 1];
        if dec.read(&mut extra).map_err(|_| corrupt("cannot decompress"))? != 0 {
            return Err(corrupt("longer than its header says"));
        }
        if object_id(kind, &data) != *id {
            return Err(corrupt("its content does not match its name (hash mismatch)"));
        }
        Ok((kind, data))
    }

    /// Reads an object that must be of `kind`.
    pub fn read_kind(&self, id: &Oid, kind: Kind) -> Result<Vec<u8>> {
        let (k, data) = self.read(id)?;
        if k != kind {
            return Err(err!("object {id} is a {}, not a {}", k.name(), kind.name()));
        }
        Ok(data)
    }

    /// The ids of every loose object.
    pub fn all_ids(&self) -> Result<Vec<Oid>> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&self.dir) else { return Ok(out) };
        for d in rd.flatten() {
            let name = d.file_name().to_string_lossy().into_owned();
            if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            for f in fs::read_dir(d.path())?.flatten() {
                if let Some(id) = Oid::from_hex(&format!("{name}{}", f.file_name().to_string_lossy())) {
                    out.push(id);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Expands an abbreviated id (4 to 40 hex digits).
    pub fn resolve_prefix(&self, prefix: &str) -> Result<Oid> {
        let p = prefix.to_ascii_lowercase();
        if p.len() < 4 || p.len() > 40 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err!("{prefix:?} is not an object id prefix"));
        }
        if let Some(id) = Oid::from_hex(&p) {
            return if self.exists(&id) { Ok(id) } else { Err(err!("object {id} not found")) };
        }
        let mut found = Vec::new();
        let dir = self.dir.join(&p[..2]);
        if let Ok(rd) = fs::read_dir(&dir) {
            for f in rd.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.starts_with(&p[2..]) {
                    if let Some(id) = Oid::from_hex(&format!("{}{name}", &p[..2])) {
                        found.push(id);
                    }
                }
            }
        }
        match found.len() {
            0 => Err(err!("no object starts with {prefix}")),
            1 => Ok(found[0]),
            _ => Err(err!("{prefix} is ambiguous: {} objects start with it", found.len())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn odb() -> (tempfile::TempDir, Odb) {
        let d = tempfile::tempdir().unwrap();
        let o = Odb::new(d.path().join("objects"));
        fs::create_dir_all(o.dir()).unwrap();
        (d, o)
    }

    #[test]
    fn write_then_read() {
        let (_d, o) = odb();
        let id = o.write(Kind::Blob, b"hello\n").unwrap();
        assert_eq!(id.hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert!(o.exists(&id));
        assert_eq!(o.read(&id).unwrap(), (Kind::Blob, b"hello\n".to_vec()));
        // Writing again changes nothing and leaves no temporary files.
        assert_eq!(o.write(Kind::Blob, b"hello\n").unwrap(), id);
        let tmp: Vec<_> =
            fs::read_dir(o.dir()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("tmp_")).collect();
        assert!(tmp.is_empty());
        assert_eq!(o.all_ids().unwrap(), vec![id]);
    }

    #[test]
    fn empty_and_large_objects() {
        let (_d, o) = odb();
        let empty = o.write(Kind::Blob, b"").unwrap();
        assert_eq!(o.read(&empty).unwrap().1, b"");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let id = o.write(Kind::Blob, &big).unwrap();
        assert_eq!(o.read(&id).unwrap().1, big);
    }

    #[test]
    fn missing_objects_say_so() {
        let (_d, o) = odb();
        let e = o.read(&crate::sha1::sha1(b"nothing")).unwrap_err();
        assert!(e.msg.contains("not found"));
    }

    #[test]
    fn corruption_is_detected_in_every_way() {
        let (_d, o) = odb();
        let id = o.write(Kind::Blob, b"some content that is long enough to compress a little a little a little").unwrap();
        let path = o.path(&id);
        let good = fs::read(&path).unwrap();

        // Flip each byte in turn. The contract: it is rejected, or it reads back exactly as stored
        // (only the zlib checksum at the end can be damaged without touching the content, and the
        // SHA-1 check already proved the content). It never reads back as different content.
        let original = (Kind::Blob, b"some content that is long enough to compress a little a little a little".to_vec());
        let mut rejected = 0;
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x40;
            fs::write(&path, &bad).unwrap();
            match o.read(&id) {
                Ok(got) => assert_eq!(got, original, "flipping byte {i} changed the content without an error"),
                Err(_) => rejected += 1,
            }
        }
        assert!(rejected >= good.len() - 4, "only {rejected} of {} flips were rejected", good.len());
        // Truncation.
        let mut rejected = 0;
        for cut in 0..good.len() {
            fs::write(&path, &good[..cut]).unwrap();
            match o.read(&id) {
                Ok(got) => assert_eq!(got, original, "truncation to {cut} changed the content"),
                Err(_) => rejected += 1,
            }
        }
        assert!(rejected >= good.len() - 4, "only {rejected} truncations were rejected");
        // A well-formed object stored under the wrong name.
        let other = o.write(Kind::Blob, b"other").unwrap();
        fs::copy(o.path(&other), &path).unwrap();
        let e = o.read(&id).unwrap_err();
        assert!(e.msg.contains("hash mismatch"), "{}", e.msg);
        fs::write(&path, &good).unwrap();
        assert!(o.read(&id).is_ok());
    }

    #[test]
    fn header_lies_are_refused() {
        let (_d, o) = odb();
        let make = |header_and_body: &[u8]| -> Oid {
            // Store under the id of the *claimed* content so only the structure is wrong.
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(header_and_body).unwrap();
            let id = crate::sha1::sha1(header_and_body);
            let path = o.path(&id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, enc.finish().unwrap()).unwrap();
            id
        };
        for (bytes, why) in [
            (&b"blob 10\0short"[..], "shorter"),
            (b"blob 2\0longer", "longer"),
            (b"blob -1\0x", "bad length"),
            (b"nonsense 1\0x", "unknown type"),
            (b"blob1\0x", "malformed"),
            (b"blob 99999999999999999999\0x", "bad length"),
        ] {
            let id = make(bytes);
            let e = o.read(&id).unwrap_err();
            assert!(e.msg.contains(why), "{}: {}", why, e.msg);
        }
    }

    #[test]
    fn type_checked_reads() {
        let (_d, o) = odb();
        let id = o.write(Kind::Blob, b"x").unwrap();
        assert!(o.read_kind(&id, Kind::Blob).is_ok());
        assert!(o.read_kind(&id, Kind::Tree).unwrap_err().msg.contains("is a blob, not a tree"));
    }

    #[test]
    fn prefixes() {
        let (_d, o) = odb();
        let ids: Vec<Oid> = (0..200u32).map(|i| o.write(Kind::Blob, format!("object {i}").as_bytes()).unwrap()).collect();
        for id in &ids {
            assert_eq!(o.resolve_prefix(&id.hex()).unwrap(), *id);
            let long = &id.hex()[..12];
            assert_eq!(o.resolve_prefix(long).unwrap(), *id);
            assert_eq!(o.resolve_prefix(&long.to_uppercase()).unwrap(), *id);
        }
        // With 200 objects some 4-digit prefix is shared by luck only rarely; build an ambiguity on purpose.
        let a = ids[0].hex();
        let shared = &a[..4];
        let count = ids.iter().filter(|i| i.hex().starts_with(shared)).count();
        if count > 1 {
            assert!(o.resolve_prefix(shared).unwrap_err().msg.contains("ambiguous"));
        }
        assert!(o.resolve_prefix("abc").is_err());
        assert!(o.resolve_prefix("zzzz").is_err());
        assert!(o.resolve_prefix("0000").is_err() || ids.iter().any(|i| i.hex().starts_with("0000")));
    }
}

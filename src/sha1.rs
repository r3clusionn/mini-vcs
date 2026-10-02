//! SHA-1, because git names every object by it. Written from the specification (FIPS 180-4) and
//! checked against its test vectors and against `git hash-object`.
//!
//! SHA-1 is broken for adversarial inputs (SHAttered, 2017). Git has the same weakness; real git
//! additionally detects the known attack pattern, which this does not.

use std::fmt;

/// A 20-byte object id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const ZERO: Oid = Oid([0; 20]);

    pub fn from_hex(s: &str) -> Option<Oid> {
        if s.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        // from_str_radix accepts a leading `+`; a hex id never has one.
        s.bytes().all(|c| c.is_ascii_hexdigit()).then_some(Oid(out))
    }

    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn short(&self) -> String {
        self.hex()[..7].to_string()
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.hex())
    }
}

/// Incremental SHA-1.
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    block: [u8; 64],
    filled: usize,
    length: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Sha1::new()
    }
}

impl Sha1 {
    pub fn new() -> Sha1 {
        Sha1 { h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0], block: [0; 64], filled: 0, length: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                let b = self.block;
                self.compress(&b);
                self.filled = 0;
            }
        }
        while data.len() >= 64 {
            let (b, rest) = data.split_at(64);
            self.compress(b.try_into().unwrap());
            data = rest;
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.filled = data.len();
        }
    }

    pub fn finish(mut self) -> Oid {
        let bits = self.length * 8;
        let mut pad = vec![0x80u8];
        let used = (self.filled + 1) % 64;
        let zeros = if used <= 56 { 56 - used } else { 120 - used };
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend(bits.to_be_bytes());
        // `update` would count the padding in the length, which is already captured in `bits`.
        let saved = self.length;
        self.update(&pad);
        self.length = saved;
        let mut out = [0u8; 20];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        Oid(out)
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (h, v) in self.h.iter_mut().zip([a, b, c, d, e]) {
            *h = h.wrapping_add(v);
        }
    }
}

pub fn sha1(data: &[u8]) -> Oid {
    let mut s = Sha1::new();
    s.update(data);
    s.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fips_vectors() {
        assert_eq!(sha1(b"").hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1(b"abc").hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").hex(),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(sha1(&million_a).hex(), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    #[test]
    fn splitting_the_input_changes_nothing() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let whole = sha1(&data);
        for cut in [0, 1, 55, 56, 57, 63, 64, 65, 127, 128, 999, 1000] {
            let mut s = Sha1::new();
            s.update(&data[..cut]);
            s.update(&data[cut..]);
            assert_eq!(s.finish(), whole, "cut at {cut}");
        }
        // One byte at a time.
        let mut s = Sha1::new();
        for b in &data {
            s.update(std::slice::from_ref(b));
        }
        assert_eq!(s.finish(), whole);
    }

    #[test]
    fn padding_boundaries() {
        // Messages of 55, 56, 63, 64 and 65 bytes straddle the padding rules.
        let known = [
            (55usize, "c1c8bbdc22796e28c0e15163d20899b65621d65a"),
            (56, "c2db330f6083854c99d4b5bfb6e8f29f201be699"),
            (63, "03f09f5b158a7a8cdad920bddc29b81c18a551f5"),
            (64, "0098ba824b5c16427bd7a1122a5a442a25ec644d"),
            (65, "11655326c708d70319be2610e8a57d9a5b959d3b"),
        ];
        for (n, want) in known {
            assert_eq!(sha1(&vec![b'a'; n]).hex(), want, "{n} bytes");
        }
    }

    #[test]
    fn hex_round_trip_and_validation() {
        let id = sha1(b"x");
        assert_eq!(Oid::from_hex(&id.hex()), Some(id));
        assert_eq!(Oid::from_hex(&id.hex().to_uppercase()), Some(id));
        assert_eq!(Oid::from_hex("abc"), None);
        assert_eq!(Oid::from_hex(&"g".repeat(40)), None);
        assert_eq!(Oid::from_hex(&format!("+{}", "1".repeat(39))), None);
        assert_eq!(id.short().len(), 7);
    }
}

//! Metadata that `#[xtest::test]`, `#[xtest::fixture]` and
//! `#[xtest::teardown]` embed in test binaries, so cargo-xtest knows which
//! fixtures exist and which tests use them without running anything.
//!
//! Each item becomes a `#[used]` static byte array, built at compile time by
//! [`record`] and referenced from the code that uses it, so the linker keeps
//! it. A record is
//!
//! ```text
//! MAGIC (8 bytes) | payload length (u32, little-endian) | payload
//! ```
//!
//! and the payload is `key=value` pairs, each ended by a NUL byte. Keys may
//! repeat (e.g. one `dep` per dependency). Values can't contain NUL, which
//! file paths and Rust paths never do.
//!
//! The controller finds records by scanning the whole binary for [`MAGIC`].
//! That works for any executable format, and the format has no pointers to
//! resolve.

/// Starts every record. The leading NUL keeps it from matching inside
/// ordinary strings.
pub const MAGIC: [u8; 8] = *b"\0XTREC1\0";

/// Records larger than this are not records.
const MAX_PAYLOAD: usize = 64 * 1024;

/// Size of the record holding `pairs` (`[key, value, key, value, ...]`).
pub const fn record_len(pairs: &[&str]) -> usize {
    let mut n = MAGIC.len() + 4;
    let mut i = 0;
    while i + 1 < pairs.len() {
        n += pairs[i].len() + 1 + pairs[i + 1].len() + 1;
        i += 2;
    }
    n
}

/// Encodes `pairs` (`[key, value, key, value, ...]`) as a record of size
/// `N`, which must be [`record_len(pairs)`](record_len).
pub const fn record<const N: usize>(pairs: &[&str]) -> [u8; N] {
    assert!(N == record_len(pairs), "wrong record length");
    assert!(pairs.len().is_multiple_of(2), "pairs must be key, value, key, value, ...");
    let mut out = [0u8; N];
    let mut at = 0;
    while at < MAGIC.len() {
        out[at] = MAGIC[at];
        at += 1;
    }
    let len = (N - MAGIC.len() - 4) as u32;
    let len = len.to_le_bytes();
    let mut b = 0;
    while b < 4 {
        out[at] = len[b];
        at += 1;
        b += 1;
    }
    let mut i = 0;
    while i < pairs.len() {
        let key = pairs[i].as_bytes();
        let value = pairs[i + 1].as_bytes();
        let mut j = 0;
        while j < key.len() {
            assert!(key[j] != 0 && key[j] != b'=', "bad key");
            out[at] = key[j];
            at += 1;
            j += 1;
        }
        out[at] = b'=';
        at += 1;
        j = 0;
        while j < value.len() {
            assert!(value[j] != 0, "values can't contain NUL");
            out[at] = value[j];
            at += 1;
            j += 1;
        }
        out[at] = 0;
        at += 1;
        i += 2;
    }
    out
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Record {
    pub pairs: Vec<(String, String)>,
}

impl Record {
    /// The first value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Every value of `key`, in order.
    pub fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs.iter().filter(move |(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Finds every record in `data` (a whole executable), without duplicates,
/// in the order they first appear. Bytes that only look like the start of a
/// record are skipped.
pub fn scan(data: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut at = 0;
    while let Some(pos) = find(&data[at..], &MAGIC) {
        let start = at + pos;
        at = start + 1;
        if let Some(r) = decode(&data[start + MAGIC.len()..]) {
            if seen.insert(r.clone()) {
                out.push(r);
            }
        }
    }
    out
}

/// Decodes the length and payload that follow [`MAGIC`].
fn decode(data: &[u8]) -> Option<Record> {
    let len = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if len == 0 || len > MAX_PAYLOAD {
        return None;
    }
    let payload = data.get(4..4 + len)?;
    if *payload.last()? != 0 {
        return None;
    }
    let mut pairs = Vec::new();
    for item in payload[..len - 1].split(|b| *b == 0) {
        let item = std::str::from_utf8(item).ok()?;
        let (k, v) = item.split_once('=')?;
        if k.is_empty() {
            return None;
        }
        pairs.push((k.to_string(), v.to_string()));
    }
    Some(Record { pairs })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        i += haystack[i..haystack.len() - needle.len() + 1].iter().position(|b| *b == first)?;
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAIRS: &[&str] = &["kind", "test", "dep", "a.rs#x", "dep", "b=c"];
    const LEN: usize = record_len(PAIRS);
    static REC: [u8; LEN] = record(PAIRS);

    #[test]
    fn roundtrips_through_a_binary_blob() {
        let mut blob = b"junk\0XTREC1\0\xff\xff\xff\xff more junk".to_vec();
        blob.extend_from_slice(&REC);
        blob.extend_from_slice(b"\0XTREC1\0\x03\0\0\0ab"); // truncated
        blob.extend_from_slice(&REC); // the same record again
        let found = scan(&blob);
        assert_eq!(found.len(), 1);
        let r = &found[0];
        assert_eq!(r.get("kind"), Some("test"));
        assert_eq!(r.all("dep").collect::<Vec<_>>(), ["a.rs#x", "b=c"]);
        assert_eq!(r.get("missing"), None);
    }

    #[test]
    fn rejects_payloads_that_are_not_pairs() {
        let mut blob = MAGIC.to_vec();
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(b"abc\0");
        assert!(scan(&blob).is_empty());
    }
}

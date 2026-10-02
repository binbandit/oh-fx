use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use memchr::memchr2;
use ofx_config::PrivateDir;
use ofx_text::{is_terminal_safe, lowercase_hex};
use sha2::{Digest, Sha256};

use crate::file_index::{Candidate, CandidateKind, MAX_INDEXED_FILES, MAX_PATH_LEN};

const MAGIC: &[u8] = b"fx-file-index-v1\n";
const DIRECTORY: &str = "file-index";
const MAX_BYTES: usize = 64 * 1024 * 1024;
const DIGEST_BYTES: usize = 32;

pub(crate) fn load(cache_dir: &Path, roots: &[&Path]) -> Option<Vec<Candidate>> {
    let roots = utf8_roots(roots)?;
    let directory = PrivateDir::open_existing(&cache_dir.join(DIRECTORY)).ok()??;
    let bytes = directory
        .read_private(&file_name(&roots), MAX_BYTES)
        .ok()??;
    let payload = bytes.strip_prefix(MAGIC)?;
    let (digest, payload) = payload.split_at_checked(DIGEST_BYTES)?;
    if Sha256::digest(payload).as_slice() != digest {
        return None;
    }
    let mut reader = PayloadReader {
        bytes: payload,
        position: 0,
    };
    reader.header()?;
    let cached_roots = reader.roots()?;
    let candidates = reader.entries()?;
    (!cached_roots.is_empty() && cached_roots == roots).then_some(candidates)
}

pub(crate) fn save(cache_dir: &Path, roots: &[&Path], candidates: &[Candidate]) -> Option<()> {
    let roots = utf8_roots(roots)?;
    if roots.is_empty() {
        return None;
    }
    let name = file_name(&roots);
    if candidates.is_empty() {
        let directory = PrivateDir::open_existing(&cache_dir.join(DIRECTORY)).ok()??;
        directory.remove(&name).ok()?;
        return Some(());
    }
    let payload = encode_payload(now_ms(), &roots, candidates)?;
    let mut bytes = Vec::with_capacity(MAGIC.len() + DIGEST_BYTES + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&Sha256::digest(&payload));
    bytes.extend_from_slice(&payload);
    let directory = PrivateDir::open_or_create(cache_dir)
        .and_then(|cache| cache.open_or_create_child(DIRECTORY))
        .ok()?;
    directory.replace(&name, &bytes).ok()
}

fn encode_payload(written_at_ms: i64, roots: &[&str], candidates: &[Candidate]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"{\"written_at_ms\":");
    payload.extend_from_slice(written_at_ms.to_string().as_bytes());
    payload.extend_from_slice(b",\"roots\":[");
    for (index, root) in roots.iter().enumerate() {
        if index > 0 {
            payload.push(b',');
        }
        push_json_string(&mut payload, root);
    }
    payload.extend_from_slice(b"],\"entries\":[");
    let mut first = true;
    for candidate in candidates
        .iter()
        .filter(|candidate| valid_path(&candidate.path))
    {
        if !first {
            payload.push(b',');
        }
        first = false;
        payload.extend_from_slice(b"{\"path\":");
        push_json_string(&mut payload, &candidate.path);
        payload.extend_from_slice(match candidate.kind {
            CandidateKind::File => b",\"kind\":0}",
            CandidateKind::Directory => b",\"kind\":1}",
        });
        if payload.len() > MAX_BYTES {
            return None;
        }
    }
    payload.extend_from_slice(b"]}");
    Some(payload)
}

fn push_json_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for &byte in text.as_bytes() {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0..0x20 => {
                out.extend_from_slice(b"\\u00");
                out.extend_from_slice(lowercase_hex(&[byte]).as_bytes());
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

struct PayloadReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl PayloadReader<'_> {
    fn header(&mut self) -> Option<()> {
        self.literal(b"{\"written_at_ms\":")?;
        let digits = self
            .bytes
            .get(self.position..)?
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let text =
            std::str::from_utf8(self.bytes.get(self.position..self.position + digits)?).ok()?;
        self.position += digits;
        text.parse::<i64>().ok().map(drop)
    }

    fn roots(&mut self) -> Option<Vec<String>> {
        self.literal(b",\"roots\":[")?;
        let mut roots = Vec::new();
        if self.literal(b"]").is_some() {
            return Some(roots);
        }
        loop {
            roots.push(self.string()?);
            if self.literal(b",").is_none() {
                self.literal(b"]")?;
                return Some(roots);
            }
        }
    }

    fn entries(&mut self) -> Option<Vec<Candidate>> {
        self.literal(b",\"entries\":[")?;
        let mut entries = Vec::new();
        if self.literal(b"]").is_none() {
            loop {
                self.literal(b"{\"path\":")?;
                let path = self.string()?;
                self.literal(b",\"kind\":")?;
                let kind = if self.literal(b"0}").is_some() {
                    CandidateKind::File
                } else {
                    self.literal(b"1}")?;
                    CandidateKind::Directory
                };
                if entries.len() == MAX_INDEXED_FILES || !valid_path(&path) {
                    return None;
                }
                entries.push(Candidate { path, kind });
                if self.literal(b",").is_none() {
                    self.literal(b"]")?;
                    break;
                }
            }
        }
        self.literal(b"}")?;
        (self.position == self.bytes.len()).then_some(entries)
    }

    fn literal(&mut self, expected: &[u8]) -> Option<()> {
        let end = self.position.checked_add(expected.len())?;
        (self.bytes.get(self.position..end)? == expected).then(|| self.position = end)
    }

    fn next_byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn string(&mut self) -> Option<String> {
        self.literal(b"\"")?;
        let mut text = Vec::new();
        loop {
            let rest = self.bytes.get(self.position..)?;
            let stop = memchr2(b'"', b'\\', rest)?;
            let plain = rest.get(..stop)?;
            if plain.iter().any(|byte| *byte < 0x20) {
                return None;
            }
            text.extend_from_slice(plain);
            self.position += stop;
            if self.next_byte()? == b'"' {
                return String::from_utf8(text).ok();
            }
            match self.next_byte()? {
                byte @ (b'"' | b'\\' | b'/') => text.push(byte),
                b'b' => text.push(0x08),
                b'f' => text.push(0x0c),
                b'n' => text.push(b'\n'),
                b'r' => text.push(b'\r'),
                b't' => text.push(b'\t'),
                b'u' => {
                    let scalar = self.escaped_scalar()?;
                    text.extend_from_slice(scalar.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => return None,
            }
        }
    }

    fn escaped_scalar(&mut self) -> Option<char> {
        let first = self.hex_unit()?;
        if !(0xd800..0xdc00).contains(&first) {
            return char::from_u32(first);
        }
        self.literal(b"\\u")?;
        let second = self.hex_unit()?;
        if !(0xdc00..0xe000).contains(&second) {
            return None;
        }
        char::from_u32(0x1_0000 + ((first - 0xd800) << 10) + (second - 0xdc00))
    }

    fn hex_unit(&mut self) -> Option<u32> {
        let mut unit = 0;
        for _ in 0..4 {
            unit = unit * 16 + char::from(self.next_byte()?).to_digit(16)?;
        }
        Some(unit)
    }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_PATH_LEN && is_terminal_safe(path.as_bytes())
}

fn utf8_roots<'a>(roots: &[&'a Path]) -> Option<Vec<&'a str>> {
    roots.iter().map(|root| root.to_str()).collect()
}

fn file_name(roots: &[&str]) -> String {
    let mut digest = Sha256::new();
    for root in roots {
        digest.update(root.as_bytes());
        digest.update([0]);
    }
    let mut name = lowercase_hex(&digest.finalize());
    name.push_str(".idx");
    name
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate {
                path: "src/main.rs".to_owned(),
                kind: CandidateKind::File,
            },
            Candidate {
                path: "docs".to_owned(),
                kind: CandidateKind::Directory,
            },
        ]
    }

    #[test]
    fn the_cache_round_trips_its_scope_and_rejects_tampering() {
        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/workspace")];
        assert!(load(cache.path(), &roots).is_none());
        save(cache.path(), &roots, &candidates()).unwrap();
        assert_eq!(load(cache.path(), &roots), Some(candidates()));
        assert!(load(cache.path(), &[Path::new("/elsewhere")]).is_none());

        let path = cache
            .path()
            .join(DIRECTORY)
            .join(file_name(&["/workspace"]));
        let saved = fs::read(&path).unwrap();
        assert!(saved.starts_with(MAGIC));
        let payload = &saved[MAGIC.len() + DIGEST_BYTES..];
        let text = std::str::from_utf8(payload).unwrap();
        assert!(text.starts_with("{\"written_at_ms\":"), "{text}");
        assert!(text.ends_with(",\"roots\":[\"/workspace\"],\"entries\":[{\"path\":\"src/main.rs\",\"kind\":0},{\"path\":\"docs\",\"kind\":1}]}"), "{text}");

        let mut flipped = saved.clone();
        let last = flipped.len() - 4;
        flipped[last] ^= 1;
        fs::write(&path, &flipped).unwrap();
        assert!(load(cache.path(), &roots).is_none());
    }

    #[test]
    fn unsafe_entries_never_reach_disk_and_empty_scans_drop_the_cache() {
        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/workspace")];
        let unsafe_only = [Candidate {
            path: "bad\u{1b}path".to_owned(),
            kind: CandidateKind::File,
        }];
        save(cache.path(), &roots, &unsafe_only).unwrap();
        assert_eq!(load(cache.path(), &roots), Some(Vec::new()));
        save(cache.path(), &roots, &[]).unwrap();
        assert!(load(cache.path(), &roots).is_none());
        let names: Vec<_> = fs::read_dir(cache.path().join(DIRECTORY))
            .unwrap()
            .collect();
        assert!(names.is_empty());
    }

    #[test]
    fn shared_cache_files_are_ignored() {
        use std::os::unix::fs::PermissionsExt;

        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/workspace")];
        save(cache.path(), &roots, &candidates()).unwrap();
        let path = cache
            .path()
            .join(DIRECTORY)
            .join(file_name(&["/workspace"]));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(cache.path(), &roots).is_none());
    }

    fn write_payload(cache: &Path, root: &str, payload: &[u8]) {
        let directory = PrivateDir::open_or_create(cache)
            .and_then(|cache| cache.open_or_create_child(DIRECTORY))
            .unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&Sha256::digest(payload));
        bytes.extend_from_slice(payload);
        directory.replace(&file_name(&[root]), &bytes).unwrap();
    }

    #[test]
    fn escaped_paths_and_roots_round_trip() {
        let cache = tempfile::tempdir().unwrap();
        let root = "/work \"q\" \\ tab\t line\n é";
        let roots = [Path::new(root)];
        let saved = vec![
            Candidate {
                path: "quote \" and \\ back.txt".to_owned(),
                kind: CandidateKind::File,
            },
            Candidate {
                path: "café/文/😀".to_owned(),
                kind: CandidateKind::Directory,
            },
        ];
        save(cache.path(), &roots, &saved).unwrap();
        assert_eq!(load(cache.path(), &roots), Some(saved));
    }

    #[test]
    fn json_escapes_decode_and_anything_else_is_a_miss() {
        let cache = tempfile::tempdir().unwrap();
        let roots = [Path::new("/w")];
        let entries = |entries: &str| {
            format!("{{\"written_at_ms\":7,\"roots\":[\"\\/w\"],\"entries\":[{entries}]}}")
        };
        write_payload(
            cache.path(),
            "/w",
            entries("{\"path\":\"caf\\u00e9\\ud83d\\ude00\\\"\",\"kind\":0}").as_bytes(),
        );
        assert_eq!(
            load(cache.path(), &roots),
            Some(vec![Candidate {
                path: "café😀\"".to_owned(),
                kind: CandidateKind::File,
            }])
        );
        write_payload(cache.path(), "/w", entries("").as_bytes());
        assert_eq!(load(cache.path(), &roots), Some(Vec::new()));
        for payload in [
            entries("{\"path\":\"a\",\"kind\":2}"),
            entries("{\"path\":\"a\",\"kind\":0},"),
            entries("{\"path\":\"\\ud83d\",\"kind\":0}"),
            entries("{\"path\":\"\\x\",\"kind\":0}"),
            entries("{\"path\":\"tab\tinside\",\"kind\":0}"),
            entries("{\"path\":\"\",\"kind\":0}"),
            entries("{\"path\" : \"a\",\"kind\":0}"),
            entries("{\"kind\":0,\"path\":\"a\"}"),
            format!("{} ", entries("")),
            "{\"written_at_ms\":-1,\"roots\":[\"/w\"],\"entries\":[]}".to_owned(),
            "{\"written_at_ms\":1,\"roots\":[],\"entries\":[]}".to_owned(),
            "{\"written_at_ms\":1,\"roots\":[\"/w\",\"/x\"],\"entries\":[]}".to_owned(),
        ] {
            write_payload(cache.path(), "/w", payload.as_bytes());
            assert_eq!(load(cache.path(), &roots), None, "{payload}");
        }
    }

    #[test]
    fn roots_hash_with_a_separator_after_each_root() {
        let name = file_name(&["/workspace"]);
        let expected = lowercase_hex(&Sha256::digest(b"/workspace\0"));
        assert_eq!(name, format!("{expected}.idx"));
        assert_eq!(Path::new(&name).as_os_str().as_bytes().len(), 68);
    }
}

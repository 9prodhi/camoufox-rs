//! Page network cassette storage and replay.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(super) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn absolute(dir: &Path) -> Result<(), String> {
    if !dir.is_absolute() {
        return Err("invalid argument: cassette directory must be absolute".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Exchange {
    pub seq: u64,
    pub method: String,
    pub url: String,
    pub request_body_sha256: Option<String>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_sha256: String,
    pub body_len: u64,
}

impl Exchange {
    /// Whether Firefox would follow this response without a new interception.
    pub fn redirects(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308)
            && self
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("location"))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Index {
    pub format: String,
    pub version: u32,
    pub exchanges: Vec<Exchange>,
}

impl Index {
    fn new(exchanges: Vec<Exchange>) -> Self {
        Self {
            format: "camoufox-cassette".into(),
            version: 1,
            exchanges,
        }
    }
}

pub(super) fn scrub_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .filter(|(name, _)| {
            !["set-cookie", "cookie", "authorization"]
                .iter()
                .any(|secret| name.eq_ignore_ascii_case(secret))
        })
        .collect()
}

pub(super) fn replay_headers(headers: &[(String, String)], len: u64) -> Vec<(String, String)> {
    let mut headers = scrub_headers(headers.to_vec());
    headers.retain(|(name, _)| {
        !["content-encoding", "content-length", "transfer-encoding"]
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h))
    });
    headers.push(("Content-Length".into(), len.to_string()));
    headers
}

type Bodies = HashMap<String, Vec<u8>>;

pub(super) fn load(dir: &Path) -> Result<(Index, Bodies), String> {
    absolute(dir)?;
    let invalid = |e| format!("invalid argument: invalid cassette: {e}");
    let index: Index = serde_json::from_slice(
        &std::fs::read(dir.join("index.json")).map_err(|e| invalid(e.to_string()))?,
    )
    .map_err(|e| invalid(e.to_string()))?;
    if index.format != "camoufox-cassette" {
        return Err(invalid("unsupported format".into()));
    }
    if index.version != 1 {
        return Err(invalid("unsupported version".into()));
    }
    let mut bodies = HashMap::new();
    let mut seqs = HashSet::new();
    for e in &index.exchanges {
        if !seqs.insert(e.seq) {
            return Err(invalid("duplicate sequence number".into()));
        }
        if !valid_hash(&e.body_sha256)
            || e.request_body_sha256
                .as_deref()
                .is_some_and(|h| !valid_hash(h))
        {
            return Err(invalid("invalid sha256".into()));
        }
        if !(100..=599).contains(&e.status) || e.method.is_empty() || e.url.is_empty() {
            return Err(invalid("invalid exchange metadata".into()));
        }
        if e.headers
            .iter()
            .any(|(n, v)| n.is_empty() || n.contains(['\r', '\n']) || v.contains(['\r', '\n']))
        {
            return Err(invalid("invalid response headers".into()));
        }
        if !bodies.contains_key(&e.body_sha256) {
            let body = std::fs::read(dir.join("bodies").join(&e.body_sha256))
                .map_err(|err| invalid(format!("body {}: {err}", e.body_sha256)))?;
            if sha256(&body) != e.body_sha256 {
                return Err(invalid(format!("body {} sha256 mismatch", e.body_sha256)));
            }
            bodies.insert(e.body_sha256.clone(), body);
        }
        if bodies[&e.body_sha256].len() as u64 != e.body_len {
            return Err(invalid(format!("body {} length mismatch", e.body_sha256)));
        }
    }
    Ok((index, bodies))
}

#[derive(Debug)]
pub(super) struct Recorder {
    dir: PathBuf,
    filters: Vec<Regex>,
    exchanges: Vec<Exchange>,
    bodies: HashSet<String>,
}

impl Recorder {
    pub fn new(dir: &Path, filters: &[String]) -> Result<Self, String> {
        absolute(dir)?;
        let filters = filters
            .iter()
            .map(|r| Regex::new(r).map_err(|e| format!("invalid argument: invalid URL regex: {e}")))
            .collect::<Result<_, _>>()?;
        if dir.exists() {
            if !dir.is_dir() {
                return Err("invalid argument: cassette path must be a directory".into());
            }
            if std::fs::read_dir(dir)
                .map_err(|e| format!("io error: {e}"))?
                .next()
                .is_some()
            {
                return Err("invalid argument: refusing non-empty cassette directory".into());
            }
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("io error: {e}"))?;
        // Reserve the directory atomically across independent instance workers.
        std::fs::create_dir(dir.join("bodies")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                "invalid argument: refusing non-empty cassette directory".into()
            } else {
                format!("io error: {e}")
            }
        })?;
        Ok(Self {
            dir: dir.into(),
            filters,
            exchanges: Vec::new(),
            bodies: HashSet::new(),
        })
    }

    pub fn matches(&self, url: &str) -> bool {
        self.filters.is_empty() || self.filters.iter().any(|r| r.is_match(url))
    }

    pub fn store(&mut self, exchange: Exchange, body: &[u8]) -> Result<(), String> {
        if self.bodies.insert(exchange.body_sha256.clone()) {
            std::fs::write(self.dir.join("bodies").join(&exchange.body_sha256), body)
                .map_err(|e| format!("io error: writing cassette body: {e}"))?;
        }
        self.exchanges.push(exchange);
        Ok(())
    }

    pub fn finish(mut self) -> Result<Value, String> {
        self.exchanges.sort_by_key(|e| e.seq);
        let result = json!({"dir": self.dir, "exchanges": self.exchanges.len(), "bodies": self.bodies.len()});
        let index = Index::new(self.exchanges);
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.dir).map_err(|e| format!("io error: {e}"))?;
        serde_json::to_writer(&mut temp, &index)
            .map_err(|e| format!("io error: writing cassette index: {e}"))?;
        temp.flush()
            .and_then(|()| temp.as_file().sync_all())
            .map_err(|e| format!("io error: {e}"))?;
        temp.persist(self.dir.join("index.json"))
            .map_err(|e| format!("io error: finalizing cassette: {e}"))?;
        Ok(result)
    }
}

type Key = (String, String, Option<String>);

pub(super) struct Matcher {
    entries: HashMap<Key, (Vec<Exchange>, usize)>,
    match_body: bool,
}

impl Matcher {
    pub fn new(mut exchanges: Vec<Exchange>, match_body: bool) -> Self {
        exchanges.sort_by_key(|e| e.seq);
        let mut entries: HashMap<Key, (Vec<Exchange>, usize)> = HashMap::new();
        for e in exchanges {
            let body = if match_body {
                e.request_body_sha256.clone()
            } else {
                None
            };
            entries
                .entry((e.method.clone(), e.url.clone(), body))
                .or_default()
                .0
                .push(e);
        }
        Self {
            entries,
            match_body,
        }
    }

    pub fn take(&mut self, method: &str, url: &str, body: Option<&str>) -> Option<Exchange> {
        let body = if self.match_body {
            body.map(str::to_owned)
        } else {
            None
        };
        let (entries, occurrence) = self.entries.get_mut(&(method.into(), url.into(), body))?;
        let entry = entries[(*occurrence).min(entries.len() - 1)].clone();
        *occurrence = occurrence.saturating_add(1);
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(body: &[u8], seq: u64) -> Exchange {
        Exchange {
            seq,
            method: "GET".into(),
            url: "https://example.com/data".into(),
            request_body_sha256: None,
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body_sha256: sha256(body),
            body_len: body.len() as u64,
        }
    }

    #[test]
    fn cassette_index_roundtrip_and_null_body() {
        let index = Index::new(vec![exchange(b"hello", 0)]);
        let json = serde_json::to_value(&index).unwrap();
        assert_eq!(json["format"], "camoufox-cassette");
        assert_eq!(json["version"], 1);
        assert!(json["exchanges"][0]["request_body_sha256"].is_null());
        assert_eq!(
            json["exchanges"][0]["headers"][0],
            serde_json::json!(["Content-Type", "text/plain"])
        );
        assert_eq!(serde_json::from_value::<Index>(json).unwrap(), index);
    }

    #[test]
    fn cassette_headers_scrub_secrets_and_decoded_framing() {
        let headers = vec![
            ("sEt-CoOkIe".into(), "secret".into()),
            ("Cookie".into(), "secret".into()),
            ("AUTHORIZATION".into(), "secret".into()),
            ("Content-Encoding".into(), "gzip".into()),
            ("Content-Length".into(), "999".into()),
            ("X-Test".into(), "ok".into()),
        ];
        let recorded = scrub_headers(headers);
        assert_eq!(recorded.len(), 3);
        let replayed = replay_headers(&recorded, 5);
        assert_eq!(
            replayed,
            vec![
                ("X-Test".into(), "ok".into()),
                ("Content-Length".into(), "5".into())
            ]
        );
    }

    #[test]
    fn cassette_occurrences_sorted_and_repeat_last_and_match_body() {
        let mut a = exchange(b"a", 2);
        let b = exchange(b"b", 1);
        a.request_body_sha256 = Some(sha256(b"input"));
        let mut matcher = Matcher::new(vec![a.clone(), b.clone()], false);
        assert_eq!(
            matcher.take("GET", &a.url, None).unwrap().body_sha256,
            b.body_sha256
        );
        for _ in 0..3 {
            assert_eq!(
                matcher.take("GET", &a.url, None).unwrap().body_sha256,
                a.body_sha256
            );
        }
        assert!(matcher.take("POST", &a.url, None).is_none());
        assert!(matcher
            .take("GET", "https://example.com/data?x=1", None)
            .is_none());
        let mut matcher = Matcher::new(vec![a.clone(), b], true);
        assert!(matcher
            .take("GET", &a.url, Some(&sha256(b"wrong")))
            .is_none());
        assert_eq!(
            matcher
                .take("GET", &a.url, a.request_body_sha256.as_deref())
                .unwrap(),
            a
        );
    }

    #[test]
    fn cassette_validation_missing_hash_length_version_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let e = exchange(b"hello", 0);
        std::fs::create_dir(dir.path().join("bodies")).unwrap();
        let write_index = |index: &Index| {
            std::fs::write(
                dir.path().join("index.json"),
                serde_json::to_vec(index).unwrap(),
            )
            .unwrap()
        };
        write_index(&Index::new(vec![e.clone()]));
        assert!(load(dir.path())
            .unwrap_err()
            .starts_with("invalid argument:"));
        std::fs::write(dir.path().join("bodies").join(&e.body_sha256), b"wrong").unwrap();
        assert!(load(dir.path()).unwrap_err().contains("sha256"));
        std::fs::write(dir.path().join("bodies").join(&e.body_sha256), b"hello").unwrap();
        assert!(load(dir.path()).is_ok());
        let mut bad = e.clone();
        bad.body_len = 10;
        write_index(&Index::new(vec![bad]));
        assert!(load(dir.path()).unwrap_err().contains("length"));
        let mut index = Index::new(vec![e]);
        index.version = 2;
        write_index(&index);
        assert!(load(dir.path()).unwrap_err().contains("version"));
        index.version = 1;
        index.format = "other".into();
        write_index(&index);
        assert!(load(dir.path()).unwrap_err().contains("format"));
        index.format = "camoufox-cassette".into();
        index.exchanges[0].body_sha256 = "../outside".into();
        write_index(&index);
        assert!(load(dir.path()).unwrap_err().contains("sha256"));
        assert!(load(std::path::Path::new("relative"))
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn cassette_record_directory_and_regex_validation() {
        assert!(Recorder::new(std::path::Path::new("relative"), &[]).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(Recorder::new(dir.path(), &["[".into()])
            .unwrap_err()
            .contains("invalid argument:"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        std::fs::write(dir.path().join("keep"), "keep").unwrap();
        assert!(Recorder::new(dir.path(), &[])
            .unwrap_err()
            .contains("non-empty"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn cassette_record_deduplicates_and_filters() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cassette");
        let mut recorder = Recorder::new(&dir, &["/data$".into(), "/other$".into()]).unwrap();
        assert!(recorder.matches("https://example.com/data"));
        assert!(recorder.matches("https://example.com/other"));
        assert!(!recorder.matches("https://example.com/script.js"));
        recorder.store(exchange(b"hello", 0), b"hello").unwrap();
        recorder.store(exchange(b"hello", 1), b"hello").unwrap();
        let result = recorder.finish().unwrap();
        assert_eq!(result["exchanges"], 2);
        assert_eq!(result["bodies"], 1);
        assert_eq!(load(&dir).unwrap().0.exchanges.len(), 2);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    }
}

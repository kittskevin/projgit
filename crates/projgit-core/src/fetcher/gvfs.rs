//! GVFS protocol fetcher.
//!
//! Implements the first, deliberately narrow, projgit integration with
//! the GVFS v1 HTTP protocol. The MVP path downloads a single loose
//! object via `GET /gvfs/objects/{oid}` and uses `POST /gvfs/sizes`
//! to warm blob header metadata for T1 prefetch.

use super::{Coalescer, Fetcher, FetcherError, HeaderProbe};
use crate::object_store::{ObjectKind, ObjectStore};
use gix::ObjectId;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

/// A [`Fetcher`] backed by the GVFS v1 HTTP protocol.
///
/// This fetcher is intended for remotes that explicitly expose GVFS
/// endpoints. It does not replace the default system-Git promisor path
/// for stock Git servers.
pub struct GvfsFetcher {
    store: Arc<ObjectStore>,
    client: GvfsClient,
    coalescer: Coalescer<ObjectId, ()>,
}

impl GvfsFetcher {
    /// Construct a GVFS fetcher without authentication.
    ///
    /// `base_url` should be the repository's GVFS base URL, with or
    /// without a trailing `/gvfs`. The fetcher normalizes either form
    /// and appends GVFS endpoint paths internally.
    pub fn open(
        store: Arc<ObjectStore>,
        base_url: impl Into<String>,
    ) -> Result<Self, GvfsFetcherError> {
        Ok(Self {
            store,
            client: GvfsClient::new(base_url.into(), Auth::None)?,
            coalescer: Coalescer::new(),
        })
    }

    /// Construct a GVFS fetcher that sends a bearer token.
    ///
    /// This is primarily useful for manual validation and CI-style
    /// experiments. A future pass should add Git credential-helper
    /// integration so authenticated GVFS remotes behave like normal
    /// Git remotes.
    pub fn with_bearer_token(
        store: Arc<ObjectStore>,
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, GvfsFetcherError> {
        Ok(Self {
            store,
            client: GvfsClient::new(base_url.into(), Auth::Bearer(token.into()))?,
            coalescer: Coalescer::new(),
        })
    }

    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        if self.store.contains(oid) {
            return Ok(());
        }

        let bytes = self.client.get_loose_object(oid)?;
        write_loose_object(self.store.git_dir(), oid, &bytes)
            .map_err(|e| FetcherError::Backend(oid, format!("write loose object: {e}")))?;

        if !self.store.contains(oid) {
            return Err(FetcherError::NotPresentAfterFetch(oid));
        }
        Ok(())
    }
}

impl Fetcher for GvfsFetcher {
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError> {
        if self.store.contains(oid) {
            return Ok(());
        }
        self.coalescer
            .do_or_join(oid, || self.raw_fetch(oid))
            .map_err(|s| reclassify_coalesced_error(oid, s))
    }

    /// Batch-query GVFS `/sizes` for blob-like OIDs and publish
    /// metadata without hydrating object bytes.
    ///
    /// projgit's T1 producer only posts regular-file, executable-file,
    /// and symlink blob OIDs. GVFS `/sizes` does not report object
    /// kind, so successful probes are reported as `ObjectKind::Blob`.
    fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe> {
        if oids.is_empty() {
            return Vec::new();
        }

        let mut local = HashMap::new();
        let mut remote = Vec::new();
        for &oid in oids {
            if self.store.contains(oid) {
                local.insert(oid, HeaderProbe::Present(oid));
            } else {
                remote.push(oid);
            }
        }

        if remote.is_empty() {
            return reorder_probes(oids, local);
        }

        match self.client.sizes(&remote) {
            Ok(sizes) => {
                for oid in remote {
                    match sizes.get(&oid).copied() {
                        Some(size) => {
                            local.insert(oid, HeaderProbe::HeaderOnly(oid, ObjectKind::Blob, size));
                        }
                        None => {
                            local.insert(
                                oid,
                                HeaderProbe::Error(
                                    oid,
                                    FetcherError::Refused(
                                        oid,
                                        "GVFS /sizes omitted object".to_owned(),
                                    ),
                                ),
                            );
                        }
                    }
                }
            }
            Err(msg) => {
                for oid in remote {
                    local.insert(
                        oid,
                        HeaderProbe::Error(oid, FetcherError::Transport(oid, msg.clone())),
                    );
                }
            }
        }

        reorder_probes(oids, local)
    }
}

/// Construction errors for [`GvfsFetcher`].
#[derive(Debug, thiserror::Error)]
pub enum GvfsFetcherError {
    /// The supplied GVFS base URL was empty or did not use HTTP(S).
    #[error("invalid GVFS base URL `{0}`; expected http(s) URL")]
    InvalidBaseUrl(String),
}

#[derive(Debug, Clone)]
enum Auth {
    None,
    Bearer(String),
}

#[derive(Debug, Clone)]
struct GvfsClient {
    base_url: String,
    auth: Auth,
}

impl GvfsClient {
    fn new(base_url: String, auth: Auth) -> Result<Self, GvfsFetcherError> {
        let base_url = normalize_base_url(&base_url)?;
        Ok(Self { base_url, auth })
    }

    fn object_url(&self, oid: ObjectId) -> String {
        format!("{}/gvfs/objects/{oid}", self.base_url)
    }

    fn sizes_url(&self) -> String {
        format!("{}/gvfs/sizes", self.base_url)
    }

    fn apply_auth(&self, request: ureq::Request) -> ureq::Request {
        match &self.auth {
            Auth::None => request,
            Auth::Bearer(token) => request.set("Authorization", &format!("Bearer {token}")),
        }
    }

    fn get_loose_object(&self, oid: ObjectId) -> Result<Vec<u8>, FetcherError> {
        let request = self.apply_auth(ureq::get(&self.object_url(oid)));
        let response = request.call().map_err(|e| http_error_to_fetcher(oid, e))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| FetcherError::Transport(oid, format!("read GVFS object response: {e}")))?;
        Ok(bytes)
    }

    fn sizes(&self, oids: &[ObjectId]) -> Result<HashMap<ObjectId, u64>, String> {
        let ids: Vec<String> = oids.iter().map(ToString::to_string).collect();
        let body = serde_json::to_string(&ids).map_err(|e| e.to_string())?;
        let request = self
            .apply_auth(ureq::post(&self.sizes_url()))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        let response = request.send_string(&body).map_err(http_error_to_string)?;
        let mut text = String::new();
        response
            .into_reader()
            .read_to_string(&mut text)
            .map_err(|e| format!("read GVFS sizes response: {e}"))?;
        let entries: Vec<SizeEntry> =
            serde_json::from_str(&text).map_err(|e| format!("parse GVFS sizes response: {e}"))?;

        let mut out = HashMap::with_capacity(entries.len());
        for entry in entries {
            let oid = ObjectId::from_hex(entry.id.as_bytes())
                .map_err(|e| format!("parse GVFS sizes object id `{}`: {e}", entry.id))?;
            out.insert(oid, entry.size);
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct SizeEntry {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Size")]
    size: u64,
}

fn normalize_base_url(raw: &str) -> Result<String, GvfsFetcherError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(GvfsFetcherError::InvalidBaseUrl(raw.to_owned()));
    }
    let without_gvfs = trimmed.strip_suffix("/gvfs").unwrap_or(trimmed);
    Ok(without_gvfs.to_owned())
}

fn write_loose_object(git_dir: &Path, oid: ObjectId, compressed: &[u8]) -> std::io::Result<()> {
    let hex = oid.to_string();
    let (dir, file) = hex.split_at(2);
    let object_dir = git_dir.join("objects").join(dir);
    let object_path = object_dir.join(file);
    if object_path.exists() {
        return Ok(());
    }

    std::fs::create_dir_all(&object_dir)?;
    let tmp = object_dir.join(format!(
        "tmp_projgit_gvfs_{}_{}",
        std::process::id(),
        unique_suffix()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(compressed)?;
    }

    if object_path.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(());
    }
    std::fs::rename(&tmp, object_path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

fn http_error_to_fetcher(oid: ObjectId, err: ureq::Error) -> FetcherError {
    match err {
        ureq::Error::Status(404, _) => {
            FetcherError::Refused(oid, "GVFS object endpoint returned 404".to_owned())
        }
        ureq::Error::Status(code, _) => {
            FetcherError::Transport(oid, format!("GVFS object endpoint returned HTTP {code}"))
        }
        ureq::Error::Transport(e) => FetcherError::Transport(oid, e.to_string()),
    }
}

fn http_error_to_string(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, _) => format!("GVFS endpoint returned HTTP {code}"),
        ureq::Error::Transport(e) => e.to_string(),
    }
}

fn reclassify_coalesced_error(oid: ObjectId, message: String) -> FetcherError {
    if message.contains("remote refused object") || message.contains("returned 404") {
        FetcherError::Refused(oid, message)
    } else if message.contains("transport error") || message.contains("GVFS object endpoint") {
        FetcherError::Transport(oid, message)
    } else if message.contains("post-fetch verification failed") {
        FetcherError::NotPresentAfterFetch(oid)
    } else {
        FetcherError::Backend(oid, message)
    }
}

fn reorder_probes(
    oids: &[ObjectId],
    mut by_oid: HashMap<ObjectId, HeaderProbe>,
) -> Vec<HeaderProbe> {
    let mut out = Vec::with_capacity(oids.len());
    for oid in oids {
        match by_oid.remove(oid) {
            Some(probe) => out.push(probe),
            None => out.push(HeaderProbe::Error(
                *oid,
                FetcherError::Backend(*oid, "gvfs prefetch_headers: missing result".to_owned()),
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::Mutex;

    #[test]
    fn normalizes_base_url() {
        assert_eq!(
            normalize_base_url("https://example.com/repo/gvfs/").unwrap(),
            "https://example.com/repo"
        );
        assert!(normalize_base_url("file:///tmp/repo").is_err());
    }

    #[test]
    fn fetch_object_writes_loose_object() {
        let (oid, loose_bytes) = fixture_loose_blob(b"hello from gvfs\n");
        let server = MockServer::spawn(vec![MockResponse::ok(loose_bytes)]);
        let dest = init_repo("dest-fetch");
        let store = Arc::new(ObjectStore::open(dest.join(".git")).unwrap());

        let fetcher = GvfsFetcher::open(store.clone(), server.base_url()).unwrap();
        fetcher.fetch_object(oid).unwrap();

        assert_eq!(store.read_blob(oid).unwrap(), b"hello from gvfs\n");
        let seen = server.seen();
        assert!(seen[0]
            .request_line
            .starts_with(&format!("GET /gvfs/objects/{oid} ")));
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn prefetch_headers_uses_sizes_endpoint() {
        let oid = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let body = format!(r#"[{{"Id":"{oid}","Size":123}}]"#).into_bytes();
        let server = MockServer::spawn(vec![MockResponse::json(body)]);
        let dest = init_repo("dest-sizes");
        let store = Arc::new(ObjectStore::open(dest.join(".git")).unwrap());

        let fetcher = GvfsFetcher::open(store, server.base_url()).unwrap();
        let probes = fetcher.prefetch_headers(&[oid]);

        assert_eq!(probes.len(), 1);
        match &probes[0] {
            HeaderProbe::HeaderOnly(probe_oid, ObjectKind::Blob, 123) => {
                assert_eq!(*probe_oid, oid);
            }
            other => panic!("unexpected probe: {other:?}"),
        }
        let seen = server.seen();
        assert!(seen[0].request_line.starts_with("POST /gvfs/sizes "));
        assert!(seen[0].body.contains(&oid.to_string()));
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn fetch_object_404_is_refused() {
        let oid = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let server = MockServer::spawn(vec![MockResponse::status(404, Vec::new())]);
        let dest = init_repo("dest-404");
        let store = Arc::new(ObjectStore::open(dest.join(".git")).unwrap());

        let fetcher = GvfsFetcher::open(store, server.base_url()).unwrap();
        let err = fetcher.fetch_object(oid).unwrap_err();
        assert!(matches!(err, FetcherError::Refused(refused_oid, _) if refused_oid == oid));
        let _ = std::fs::remove_dir_all(dest);
    }

    fn fixture_loose_blob(contents: &[u8]) -> (ObjectId, Vec<u8>) {
        let repo = init_repo("source");
        let path = repo.join("blob.txt");
        std::fs::write(&path, contents).unwrap();
        let oid_hex =
            String::from_utf8(git(&repo, &["hash-object", "-w", path.to_str().unwrap()])).unwrap();
        let oid_hex = oid_hex.trim();
        let oid = ObjectId::from_hex(oid_hex.as_bytes()).unwrap();
        let (dir, file) = oid_hex.split_at(2);
        let bytes = std::fs::read(repo.join(".git").join("objects").join(dir).join(file)).unwrap();
        let _ = std::fs::remove_dir_all(repo);
        (oid, bytes)
    }

    fn init_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "projgit-gvfs-{name}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        dir
    }

    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[derive(Clone)]
    struct SeenRequest {
        request_line: String,
        body: String,
    }

    struct MockResponse {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
    }

    impl MockResponse {
        fn ok(body: Vec<u8>) -> Self {
            Self::status_with_type(200, "application/octet-stream", body)
        }

        fn json(body: Vec<u8>) -> Self {
            Self::status_with_type(200, "application/json", body)
        }

        fn status(status: u16, body: Vec<u8>) -> Self {
            Self::status_with_type(status, "text/plain", body)
        }

        fn status_with_type(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
            Self {
                status,
                content_type,
                body,
            }
        }
    }

    struct MockServer {
        base_url: String,
        seen: Arc<Mutex<Vec<SeenRequest>>>,
    }

    impl MockServer {
        fn spawn(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let thread_seen = seen.clone();
            std::thread::spawn(move || {
                let mut responses: VecDeque<MockResponse> = VecDeque::from(responses);
                while let Some(response) = responses.pop_front() {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(value) = lower.strip_prefix("content-length:") {
                            content_length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; content_length];
                    if content_length > 0 {
                        reader.read_exact(&mut body).unwrap();
                    }
                    thread_seen.lock().unwrap().push(SeenRequest {
                        request_line: request_line.trim_end().to_owned(),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });

                    let reason = if response.status == 200 { "OK" } else { "ERR" };
                    let header = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                        response.status,
                        reason,
                        response.body.len(),
                        response.content_type
                    );
                    stream.write_all(header.as_bytes()).unwrap();
                    stream.write_all(&response.body).unwrap();
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                seen,
            }
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        fn seen(&self) -> Vec<SeenRequest> {
            self.seen.lock().unwrap().clone()
        }
    }
}

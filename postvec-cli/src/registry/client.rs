//! Registry HTTP: index fetching and verified archive downloads.
//!
//! Transport rules, encoded rather than documented:
//!
//! - a fresh archive request must answer 200 with the exact advertised
//!   `Content-Length`; a resume sends `Range` and accepts only a 206 whose
//!   `Content-Range` starts at the requested byte — a mirror that ignores
//!   Range and answers 200 causes a truncate-and-restart, never an append;
//! - the overall `--timeout` bounds index requests, but an archive body gets
//!   a connect timeout plus a no-progress timeout that resets on every byte,
//!   because a fixed 30s deadline makes a multi-gigabyte pull impossible;
//! - 429/503 honour `Retry-After` within a bounded budget, then the next
//!   source; a digest mismatch deletes the file and tries the next public
//!   source once; 401/403 on a private source surfaces as
//!   [`DownloadError::AuthExpired`] so the caller can refetch the
//!   authenticated index exactly once and resume;
//! - HTTPS only, ≤5 redirects, HTTPS→HTTPS only, no URL credentials, no
//!   loopback/link-local/private destinations — except under a test-only
//!   index override, which relaxes transport so fixtures can serve from
//!   127.0.0.1.

use crate::error::{CliError, Result};
use crate::registry::index::{self, Index, IndexModel};
use reqwest::header;
use reqwest::StatusCode;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// Chunk-to-chunk stall deadline for archive bodies.
const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest single `Retry-After` wait honoured, and how many are honoured per
/// source, so a hostile mirror cannot park a pull forever.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_THROTTLE_WAITS: u32 = 2;
const MAX_REDIRECTS: usize = 5;

/// Aggregate byte progress shared by every concurrent download of a pull.
#[derive(Debug, Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
}

impl Progress {
    pub fn add_total(&self, bytes: u64) {
        self.total.fetch_add(bytes, Ordering::Relaxed);
    }
    pub fn add_done(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

/// Why a download failed, at the granularity `pull` reacts to.
#[derive(Debug)]
pub enum DownloadError {
    /// A **private** entry's source answered 401/403 — the presigned URL
    /// likely expired. The caller refetches the authenticated index once and
    /// retries. Public entries never produce this: a gated or misconfigured
    /// public mirror's 401/403 is an ordinary per-source failure and the next
    /// mirror is tried.
    AuthExpired,
    /// Every source failed; the message aggregates per-source detail.
    Failed(CliError),
}

impl From<CliError> for DownloadError {
    fn from(e: CliError) -> Self {
        DownloadError::Failed(e)
    }
}

pub struct RegistryClient {
    client: reqwest::Client,
    timeout: Duration,
    /// Set only when a test-only index override is active.
    allow_insecure: bool,
}

impl RegistryClient {
    pub fn new(timeout: Duration, allow_insecure: bool) -> Result<Self> {
        let redirect = if allow_insecure {
            reqwest::redirect::Policy::limited(MAX_REDIRECTS)
        } else {
            reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("too many redirects");
                }
                if attempt.url().scheme() != "https" {
                    return attempt.error("redirect leaves HTTPS");
                }
                if let Some(host) = attempt.url().host() {
                    // Literal-IP redirect targets are checkable synchronously;
                    // hostname targets were vetted when the source was.
                    if let Ok(ip) = host.to_string().parse::<IpAddr>() {
                        if !ip_is_public(ip) {
                            return attempt.error("redirect targets a private address");
                        }
                    }
                }
                attempt.follow()
            })
        };
        let client = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(30)))
            .redirect(redirect)
            .user_agent(format!("postvec-cli/{}", crate::CLI_VERSION))
            .build()
            .map_err(|e| CliError::internal(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            client,
            timeout,
            allow_insecure,
        })
    }

    /// Fetch and validate an index. `bearer` selects the authenticated
    /// channel; a bad credential is a hard error here, never a downgrade.
    pub async fn fetch_index(&self, url: &str, bearer: Option<&str>) -> Result<Index> {
        self.check_url(url).await?;
        let mut request = self.client.get(url).timeout(self.timeout);
        if let Some(key) = bearer {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|e| {
            CliError::precondition(format!(
                "cannot reach the registry index at {}: {}",
                strip_query(url),
                redact_reqwest(&e)
            ))
            .with_fix("check network access to the registry, or retry later")
        })?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(CliError::precondition(format!(
                "the registry rejected the credential ({status})"
            ))
            .with_fix(
                "the key may be revoked, expired, or the account unverified; run \
                 `postvec login` with a current key, or `postvec logout` to use the \
                 public catalogue",
            ));
        }
        if !status.is_success() {
            return Err(CliError::precondition(format!(
                "registry index request failed: HTTP {status}"
            )));
        }

        let mut body: Vec<u8> = Vec::new();
        let mut response = response;
        loop {
            let chunk = tokio::time::timeout(self.timeout, response.chunk())
                .await
                .map_err(|_| CliError::precondition("registry index download timed out"))?
                .map_err(|e| {
                    CliError::precondition(format!(
                        "registry index download failed: {}",
                        redact_reqwest(&e)
                    ))
                })?;
            let Some(chunk) = chunk else { break };
            if body.len() + chunk.len() > index::MAX_INDEX_BYTES {
                return Err(CliError::precondition(format!(
                    "registry index exceeds the {} byte limit",
                    index::MAX_INDEX_BYTES
                )));
            }
            body.extend_from_slice(&chunk);
        }
        index::parse_and_validate(&body)
    }

    /// Download `model`'s archive into `part_path` (a `.part` file that may
    /// hold a resumable prefix), verify its digest, and return the source
    /// host that served it.
    ///
    /// The caller registers this model's bytes with `progress` once,
    /// before the first attempt. This method may be re-entered after an
    /// auth-expiry index refresh, and re-registering the total or the
    /// already-counted resumable prefix would double-count them. Bytes
    /// discarded by a restart are rolled back here.
    pub async fn download(
        &self,
        model: &IndexModel,
        part_path: &Path,
        progress: &Progress,
    ) -> std::result::Result<String, DownloadError> {
        let digest_hex = model
            .archive
            .digest_hex()
            .map_err(CliError::precondition)?
            .to_string();
        let size = model.archive.size;

        let mut failures: Vec<String> = Vec::new();
        let mut digest_retry_used = false;
        for source in &model.archive.sources {
            if let Err(e) = self.check_url(source).await {
                failures.push(format!("{}: {}", strip_query(source), e));
                continue;
            }
            match self
                .download_one(source, part_path, size, &digest_hex, progress)
                .await
            {
                Ok(()) => {
                    let host = reqwest::Url::parse(source)
                        .ok()
                        .and_then(|u| u.host_str().map(|h| h.to_string()))
                        .unwrap_or_else(|| "unknown".to_string());
                    return Ok(host);
                }
                Err(OneSourceError::AuthExpired) => {
                    // Only a private entry's 401/403 means "credential/presign
                    // expired". For a public model it is a mirror problem:
                    // record it and fall through to the next source.
                    if !model.is_public() {
                        return Err(DownloadError::AuthExpired);
                    }
                    failures.push(format!(
                        "{}: denied access (401/403) to a public archive",
                        strip_query(source)
                    ));
                }
                Err(OneSourceError::DigestMismatch) => {
                    failures.push(format!("{}: digest mismatch", strip_query(source)));
                    // One further public source after a mismatch, then fail.
                    if digest_retry_used {
                        break;
                    }
                    digest_retry_used = true;
                }
                Err(OneSourceError::Other(detail)) => {
                    failures.push(format!("{}: {detail}", strip_query(source)));
                }
            }
        }
        Err(DownloadError::Failed(
            CliError::apply(format!(
                "cannot download {}: {}",
                model.name,
                failures.join("; ")
            ))
            .with_fix("check network access, then rerun — completed bytes resume"),
        ))
    }

    async fn download_one(
        &self,
        source: &str,
        part_path: &Path,
        size: u64,
        digest_hex: &str,
        progress: &Progress,
    ) -> std::result::Result<(), OneSourceError> {
        let mut throttle_waits = 0u32;
        loop {
            // A resumable part must be a plain, singly-linked regular file.
            // A pre-planted symlink or hard link would otherwise let a
            // lower-privileged writer aim a privileged pull's truncate/write
            // at an arbitrary file.
            check_part_is_plain(part_path)?;
            let have = tokio::fs::metadata(part_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            if have > size {
                // A local partial larger than the advertised entity can never
                // verify; start over.
                remove_part(part_path, size, progress).await;
            }
            let have = tokio::fs::metadata(part_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);

            let mut request = self
                .client
                .get(source)
                .header(header::ACCEPT_ENCODING, "identity");
            if have > 0 {
                request = request.header(header::RANGE, format!("bytes={have}-"));
            }
            let response = request.send().await.map_err(|e| {
                OneSourceError::Other(format!("request failed: {}", redact_reqwest(&e)))
            })?;

            match response.status() {
                StatusCode::OK => {
                    if have > 0 {
                        // Mirror ignored Range: truncate and restart, never append.
                        remove_part(part_path, size, progress).await;
                        progress.add_done(0);
                    }
                    let advertised = content_length(&response);
                    if advertised != Some(size) {
                        return Err(OneSourceError::Other(format!(
                            "Content-Length {advertised:?}, index says {size}"
                        )));
                    }
                    self.stream_body(response, part_path, 0, size, progress)
                        .await?;
                }
                StatusCode::PARTIAL_CONTENT => {
                    let start = parse_content_range_start(&response);
                    if start != Some(have) {
                        return Err(OneSourceError::Other(format!(
                            "206 Content-Range starts at {start:?}, requested {have}"
                        )));
                    }
                    self.stream_body(response, part_path, have, size, progress)
                        .await?;
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    return Err(OneSourceError::AuthExpired);
                }
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    if throttle_waits >= MAX_THROTTLE_WAITS {
                        return Err(OneSourceError::Other(format!(
                            "still throttled after {MAX_THROTTLE_WAITS} waits"
                        )));
                    }
                    throttle_waits += 1;
                    let wait = retry_after(&response).unwrap_or(Duration::from_secs(5));
                    tokio::time::sleep(wait.min(MAX_RETRY_AFTER)).await;
                    continue;
                }
                // Range not satisfiable usually means the partial is stale
                // relative to a corrected mirror; restart clean.
                StatusCode::RANGE_NOT_SATISFIABLE => {
                    remove_part(part_path, size, progress).await;
                    continue;
                }
                other => {
                    return Err(OneSourceError::Other(format!("HTTP {other}")));
                }
            }

            // The exact advertised byte count arrived; hash from byte zero.
            let actual = hash_part(part_path).await.map_err(OneSourceError::Other)?;
            if actual != digest_hex {
                remove_part(part_path, size, progress).await;
                return Err(OneSourceError::DigestMismatch);
            }
            return Ok(());
        }
    }

    async fn stream_body(
        &self,
        mut response: reqwest::Response,
        part_path: &Path,
        offset: u64,
        size: u64,
        progress: &Progress,
    ) -> std::result::Result<(), OneSourceError> {
        // O_NOFOLLOW closes the window between the plain-file check and this
        // open: a symlink planted in between fails the open instead of being
        // followed.
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(offset == 0)
            .custom_flags(libc::O_NOFOLLOW)
            .open(part_path)
            .await
            .map_err(|e| {
                OneSourceError::Other(format!("cannot open {}: {e}", part_path.display()))
            })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| OneSourceError::Other(format!("seek failed: {e}")))?;

        let mut written = offset;
        loop {
            let chunk = tokio::time::timeout(NO_PROGRESS_TIMEOUT, response.chunk())
                .await
                .map_err(|_| OneSourceError::Other("no progress for 30s".to_string()))?
                .map_err(|e| {
                    OneSourceError::Other(format!("read failed: {}", redact_reqwest(&e)))
                })?;
            let Some(chunk) = chunk else { break };
            written += chunk.len() as u64;
            if written > size {
                return Err(OneSourceError::Other(format!(
                    "body exceeds the advertised {size} bytes"
                )));
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| OneSourceError::Other(format!("write failed: {e}")))?;
            progress.add_done(chunk.len() as u64);
        }
        if written != size {
            return Err(OneSourceError::Other(format!(
                "connection closed at {written} of {size} bytes"
            )));
        }
        file.sync_all()
            .await
            .map_err(|e| OneSourceError::Other(format!("sync failed: {e}")))?;
        Ok(())
    }

    /// Source policy. DNS resolution runs on a blocking thread. The check
    /// is best-effort against rebinding (the connect re-resolves) but
    /// turns an index that names a private destination into a refusal, not
    /// a probe.
    async fn check_url(&self, raw: &str) -> Result<()> {
        let url = reqwest::Url::parse(raw)
            .map_err(|e| CliError::precondition(format!("invalid registry URL: {e}")))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(CliError::precondition(
                "registry URL carries credentials; refusing it".to_string(),
            ));
        }
        if self.allow_insecure {
            return Ok(());
        }
        if url.scheme() != "https" {
            return Err(CliError::precondition(format!(
                "registry URL {} is not HTTPS",
                strip_query(raw)
            )));
        }
        let Some(host) = url.host_str().map(|h| h.to_string()) else {
            return Err(CliError::precondition(
                "registry URL has no host".to_string(),
            ));
        };
        let port = url.port_or_known_default().unwrap_or(443);
        if let Ok(ip) = host.parse::<IpAddr>() {
            if !ip_is_public(ip) {
                return Err(private_destination(&host));
            }
            return Ok(());
        }
        let addrs = tokio::task::spawn_blocking(move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(|iter| iter.map(|a| a.ip()).collect::<Vec<_>>())
        })
        .await
        .map_err(|e| CliError::internal(format!("DNS task failed: {e}")))?
        .map_err(|e| CliError::precondition(format!("cannot resolve registry host: {e}")))?;
        if let Some(private) = addrs.iter().find(|ip| !ip_is_public(**ip)) {
            let _ = private;
            return Err(private_destination(&strip_query(raw)));
        }
        Ok(())
    }
}

enum OneSourceError {
    AuthExpired,
    DigestMismatch,
    Other(String),
}

/// Refuse to resume into anything that is not a plain, singly-linked regular
/// file. Absent is fine (a fresh download creates it).
fn check_part_is_plain(part_path: &Path) -> std::result::Result<(), OneSourceError> {
    match std::fs::symlink_metadata(part_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OneSourceError::Other(format!(
            "cannot stat {}: {e}",
            part_path.display()
        ))),
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            if meta.file_type().is_symlink() {
                return Err(OneSourceError::Other(format!(
                    "{} is a symlink; refusing to write through it",
                    part_path.display()
                )));
            }
            if !meta.is_file() {
                return Err(OneSourceError::Other(format!(
                    "{} is not a regular file",
                    part_path.display()
                )));
            }
            if meta.nlink() != 1 {
                return Err(OneSourceError::Other(format!(
                    "{} has {} hard links; refusing to resume into it",
                    part_path.display(),
                    meta.nlink()
                )));
            }
            Ok(())
        }
    }
}

fn private_destination(what: &str) -> CliError {
    CliError::precondition(format!(
        "registry source {what} resolves to a loopback/link-local/private address; refusing it"
    ))
}

fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
            // CGNAT
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00   // unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80) // link local
        }
    }
}

fn content_length(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// `Content-Range: bytes <start>-<end>/<total>` → start.
fn parse_content_range_start(response: &reqwest::Response) -> Option<u64> {
    let value = response
        .headers()
        .get(header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    let rest = value.trim().strip_prefix("bytes ")?;
    let (start, _) = rest.split_once('-')?;
    start.trim().parse().ok()
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(header::RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

async fn remove_part(part_path: &Path, size: u64, progress: &Progress) {
    if let Ok(meta) = tokio::fs::metadata(part_path).await {
        // Roll the progress bar back by what we discard.
        let counted = meta.len().min(size);
        progress.done.fetch_sub(counted, Ordering::Relaxed);
    }
    let _ = tokio::fs::remove_file(part_path).await;
}

async fn hash_part(part_path: &Path) -> std::result::Result<String, String> {
    let path = part_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut file = std::fs::File::open(&path).map_err(|e| format!("cannot open: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 128 * 1024];
        loop {
            let got = file
                .read(&mut buf)
                .map_err(|e| format!("read failed: {e}"))?;
            if got == 0 {
                break;
            }
            hasher.update(&buf[..got]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|e| format!("hash task failed: {e}"))?
}

/// A URL with its query string removed — the only form that may appear in
/// messages, because presigned queries are credentials.
pub fn strip_query(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => base.to_string(),
        None => url.to_string(),
    }
}

/// reqwest errors can embed the full URL, query string included.
fn redact_reqwest(e: &reqwest::Error) -> String {
    let mut message = e.to_string();
    if let Some(url) = e.url() {
        if let Some(query) = url.query() {
            message = message.replace(query, "REDACTED");
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_special_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.9",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(!ip_is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"] {
            assert!(ip_is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn query_strings_never_survive_into_messages() {
        assert_eq!(
            strip_query("https://b.s3.amazonaws.com/k?X-Amz-Signature=SECRET"),
            "https://b.s3.amazonaws.com/k"
        );
        assert_eq!(strip_query("https://plain/x"), "https://plain/x");
    }
}

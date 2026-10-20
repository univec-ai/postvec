//! End-to-end `postvec model pull` against a local registry fixture, running the real binary.
//! The index-URL override exists only with `registry-test-overrides`. A default build compiles this file to nothing.
#![cfg(feature = "registry-test-overrides")]

use postvec_cli::registry::archive::{write_deterministic, ArchiveInput};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("postvec")
}

/// One model, one archive and one index, plus switches for a hostile mirror.
struct Fixture {
    address: String,
    index_body: Vec<u8>,
    archive: Vec<u8>,
    digest_hex: String,
    /// Answer 200 with the full body even when the request carries Range.
    ignore_range: Arc<AtomicBool>,
    /// Serve an archive whose bytes do not match the advertised digest.
    corrupt: Arc<AtomicBool>,
    /// The index currently served. A new revision swaps it. Previously published archives stay served at their own digest.
    index: Arc<Mutex<Vec<u8>>>,
    /// Digest hex to archive bytes. Entries are added and never replaced or removed.
    archives: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    /// The document at `/auth/v1/index.json`, the authenticated channel. `None` answers 404.
    auth_index: Arc<Mutex<Option<Vec<u8>>>>,
    /// Answer 401 on the authenticated index, as aphex does for a revoked
    /// key: the client must fail, never fall back to the public catalogue.
    reject_auth: Arc<AtomicBool>,
    /// Count of archive GETs, so a refusal can be shown to happen before any download.
    archive_requests: Arc<std::sync::atomic::AtomicUsize>,
    _thread: std::thread::JoinHandle<()>,
}

const MODEL: &str = "fixture-model";

fn build_archive(dir: &Path) -> (Vec<u8>, u64) {
    build_archive_with(dir, 42)
}

/// `weight` distinguishes revisions: it changes the file the descriptor
/// references, so the archive digest changes between revisions.
fn build_archive_with(dir: &Path, weight: u8) -> (Vec<u8>, u64) {
    let src = dir.join("fixture-src");
    std::fs::create_dir_all(src.join("onnx")).unwrap();
    std::fs::write(
        src.join("descriptor.json"),
        format!(
            r#"{{"name":"{MODEL}","backend":"onnx-runtime","enabled":true,
                "file_path":"onnx/model.onnx",
                "params":{{"model_type":"embed","target_dim":8}}}}"#
        ),
    )
    .unwrap();
    // Big enough to span several read chunks.
    std::fs::write(src.join("onnx/model.onnx"), vec![weight; 300_000]).unwrap();
    std::fs::write(src.join("LICENSE"), b"MIT").unwrap();
    let inputs = vec![
        ArchiveInput {
            relative_path: "ninference.hub.json".into(),
            source: src.join("descriptor.json"),
        },
        ArchiveInput {
            relative_path: "onnx/model.onnx".into(),
            source: src.join("onnx/model.onnx"),
        },
        ArchiveInput {
            relative_path: "LICENSE".into(),
            source: src.join("LICENSE"),
        },
    ];
    let mut bytes = Vec::new();
    let written = write_deterministic(MODEL, &inputs, &mut bytes).unwrap();
    assert_eq!(written.size, bytes.len() as u64);
    (bytes, written.installed_size)
}

impl Fixture {
    fn start(work: &Path) -> Self {
        let (archive, installed_size) = build_archive(work);
        let digest_hex = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&archive))
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let index_body = index_json(&address, &digest_hex, archive.len(), installed_size, 1);
        let index = Arc::new(Mutex::new(index_body.clone()));
        let archives = Arc::new(Mutex::new(BTreeMap::from([(
            digest_hex.clone(),
            archive.clone(),
        )])));

        let ignore_range = Arc::new(AtomicBool::new(false));
        let corrupt = Arc::new(AtomicBool::new(false));
        let auth_index: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let reject_auth = Arc::new(AtomicBool::new(false));
        let archive_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread = {
            let index = index.clone();
            let archives = archives.clone();
            let ignore_range = ignore_range.clone();
            let corrupt = corrupt.clone();
            let auth_index = auth_index.clone();
            let reject_auth = reject_auth.clone();
            let archive_requests = archive_requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut buf = [0u8; 8192];
                    let mut request = Vec::new();
                    // Read until the header terminator (requests have no body).
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                request.extend_from_slice(&buf[..n]);
                                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let range_start: Option<u64> = text
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split('=').nth(1))
                        .and_then(|spec| spec.trim().trim_end_matches('-').parse().ok());

                    let respond = |stream: &mut std::net::TcpStream,
                                   status: &str,
                                   extra: &str,
                                   body: &[u8]| {
                        let _ = write!(
                                stream,
                                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                                body.len()
                            );
                        let _ = stream.write_all(body);
                    };

                    if path.starts_with("/auth/") {
                        if reject_auth.load(Ordering::SeqCst) {
                            respond(&mut stream, "401 Unauthorized", "", b"revoked");
                        } else if let Some(body) = auth_index.lock().unwrap().clone() {
                            respond(&mut stream, "200 OK", "", &body);
                        } else {
                            respond(&mut stream, "404 Not Found", "", b"no auth index");
                        }
                    } else if path.ends_with("/index.json") {
                        let body = index.lock().unwrap().clone();
                        respond(&mut stream, "200 OK", "", &body);
                    } else if let Some(wanted) = path
                        .rsplit("/archives/sha256/")
                        .next()
                        .filter(|_| path.contains("/archives/sha256/"))
                    {
                        archive_requests.fetch_add(1, Ordering::SeqCst);
                        let Some(mut body) = archives.lock().unwrap().get(wanted).cloned() else {
                            respond(&mut stream, "404 Not Found", "", b"no such archive");
                            continue;
                        };
                        if corrupt.load(Ordering::SeqCst) {
                            let last = body.len() - 1;
                            body[last] ^= 0xff;
                        }
                        match range_start {
                            Some(start)
                                if !ignore_range.load(Ordering::SeqCst)
                                    && start < body.len() as u64 =>
                            {
                                let total = body.len();
                                let piece = body[start as usize..].to_vec();
                                let content_range = format!(
                                    "Content-Range: bytes {start}-{}/{total}\r\n",
                                    total - 1
                                );
                                respond(&mut stream, "206 Partial Content", &content_range, &piece);
                            }
                            _ => respond(&mut stream, "200 OK", "", &body),
                        }
                    } else {
                        respond(&mut stream, "404 Not Found", "", b"nope");
                    }
                }
            })
        };
        Fixture {
            address,
            index_body,
            archive,
            digest_hex,
            ignore_range,
            corrupt,
            index,
            archives,
            auth_index,
            reject_auth,
            archive_requests,
            _thread: thread,
        }
    }

    fn archive_request_count(&self) -> usize {
        self.archive_requests.load(Ordering::SeqCst)
    }

    /// Publish a revision that carries a versioned notice-policy document. The head moves as in `publish_revision`.
    fn publish_notice_revision(
        &self,
        work: &Path,
        revision: u64,
        weight: u8,
        doc_version: &str,
    ) -> String {
        let (archive, installed_size) =
            build_archive_with(&work.join(format!("notice-rev{revision}")), weight);
        let digest_hex = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&archive))
        };
        self.archives
            .lock()
            .unwrap()
            .insert(digest_hex.clone(), archive.clone());
        let mut body: serde_json::Value = serde_json::from_slice(&index_json(
            &self.address,
            &digest_hex,
            archive.len(),
            installed_size,
            revision,
        ))
        .unwrap();
        let entry = &mut body["models"][0];
        entry["license"] = serde_json::json!("univec-commercial");
        entry["license_version"] = serde_json::json!(doc_version);
        entry["license_url"] = serde_json::json!(format!(
            "https://univec.ai/legal/models/univec-commercial/{doc_version}"
        ));
        entry["license_acceptance"] = serde_json::json!("notice");
        *self.index.lock().unwrap() = body.to_string().into_bytes();
        digest_hex
    }

    fn index_url(&self) -> String {
        format!("http://{}/v1/index.json", self.address)
    }

    fn auth_index_url(&self) -> String {
        format!("http://{}/auth/v1/index.json", self.address)
    }

    /// Serve the authenticated index: private channel, `authenticated: true` and the viewer block for this caller.
    fn serve_authenticated_index(&self, viewer: serde_json::Value) {
        let mut body: serde_json::Value =
            serde_json::from_slice(&self.index.lock().unwrap().clone()).unwrap();
        body["channel"] = serde_json::json!("private");
        body["authenticated"] = serde_json::json!(true);
        body["viewer"] = viewer;
        *self.auth_index.lock().unwrap() = Some(body.to_string().into_bytes());
    }

    /// Publish a new revision of the SAME name: the head moves, and every
    /// previously published archive stays served at its own digest.
    fn publish_revision(&self, work: &Path, revision: u64, weight: u8) -> String {
        let (archive, installed_size) =
            build_archive_with(&work.join(format!("rev{revision}")), weight);
        let digest_hex = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&archive))
        };
        self.archives
            .lock()
            .unwrap()
            .insert(digest_hex.clone(), archive.clone());
        *self.index.lock().unwrap() = index_json(
            &self.address,
            &digest_hex,
            archive.len(),
            installed_size,
            revision,
        );
        digest_hex
    }
}

impl Fixture {
    /// Serve an archive whose descriptor disagrees with the index entry that advertises it.
    fn publish_lying_archive(&self, work: &Path) {
        let src = work.join("lying").join("fixture-src");
        std::fs::create_dir_all(src.join("onnx")).unwrap();
        std::fs::write(
            src.join("descriptor.json"),
            format!(
                r#"{{"name":"{MODEL}","backend":"onnx-runtime","enabled":true,
                    "file_path":"onnx/model.onnx",
                    "params":{{"model_type":"embed","target_dim":16}}}}"#
            ),
        )
        .unwrap();
        std::fs::write(src.join("onnx/model.onnx"), vec![3u8; 1000]).unwrap();
        std::fs::write(src.join("LICENSE"), b"MIT").unwrap();
        let inputs = vec![
            ArchiveInput {
                relative_path: "ninference.hub.json".into(),
                source: src.join("descriptor.json"),
            },
            ArchiveInput {
                relative_path: "onnx/model.onnx".into(),
                source: src.join("onnx/model.onnx"),
            },
            ArchiveInput {
                relative_path: "LICENSE".into(),
                source: src.join("LICENSE"),
            },
        ];
        let mut archive = Vec::new();
        let written = write_deterministic(MODEL, &inputs, &mut archive).unwrap();
        let digest_hex = written.digest.strip_prefix("sha256:").unwrap().to_string();
        self.archives
            .lock()
            .unwrap()
            .insert(digest_hex.clone(), archive.clone());
        // The index keeps advertising 8 dimensions.
        *self.index.lock().unwrap() = index_json(
            &self.address,
            &digest_hex,
            archive.len(),
            written.installed_size,
            1,
        );
    }
}

fn index_json(
    address: &str,
    digest_hex: &str,
    size: usize,
    installed_size: u64,
    revision: u64,
) -> Vec<u8> {
    serde_json::json!({
        "schema_version": 1,
        "channel": "public",
        "models": [{
            "name": MODEL,
            "access": "public",
            "model_type": "embed",
            "backend": "onnx-runtime",
            "target_dim": 8,
            "license": "mit",
            "revision": revision,
            "dependencies": [],
            "postvec_requires": [],
            "archive": {
                "digest": format!("sha256:{digest_hex}"),
                "size": size,
                "installed_size": installed_size,
                "sources": [format!("http://{address}/archives/sha256/{digest_hex}")]
            }
        }]
    })
    .to_string()
    .into_bytes()
}

/// Minimal HTTP GET, enough to prove an old archive is still retrievable.
fn http_status(url: &str) -> u16 {
    let rest = url.strip_prefix("http://").expect("http url");
    let (authority, path) = rest.split_once('/').expect("path");
    let mut stream = TcpStream::connect(authority).expect("connect");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn engine_root(work: &Path) -> PathBuf {
    let root = work.join("engine-root");
    std::fs::create_dir_all(root.join("models/onnx-runtime")).unwrap();
    // Trust gates refuse a group-writable root or ancestor, including this tempdir, and want a canonical path.
    for dir in [work, root.as_path(), root.join("models").as_path()] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    root.canonicalize().unwrap()
}

fn run_pull(fixture: &Fixture, root: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "model",
        "pull",
        MODEL,
        "--path",
        root.to_str().unwrap(),
        "--yes",
    ];
    args.extend_from_slice(extra);
    Command::new(binary())
        .args(&args)
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env_remove("POSTVEC_API_KEY")
        .env("XDG_CONFIG_HOME", "/nonexistent-config-home")
        .env("POSTVEC_REGISTRY_PUBLIC_INDEX_URL", fixture.index_url())
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_fresh_server_root_supports_listing_preview_and_pull() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = work.path().join("empty-root");
    std::fs::create_dir(&root).unwrap();
    let cli = |args: &[&str]| {
        Command::new(binary())
            .args(args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_API_KEY")
            .env("XDG_CONFIG_HOME", work.path().join("auth"))
            .env("POSTVEC_PATH", &root)
            .env("POSTVEC_REGISTRY_PUBLIC_INDEX_URL", fixture.index_url())
            .output()
            .unwrap()
    };

    let out = cli(&["model", "ls", "--format", "json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["models"], serde_json::json!([]));
    let out = cli(&["model", "pull", MODEL, "--dry-run", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!root.join("models").exists());
    assert_eq!(fixture.archive_request_count(), 0);

    let out = cli(&["model", "pull", MODEL, "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let other = work.path().join("other-root");
    std::fs::create_dir(&other).unwrap();
    let out = cli(&[
        "model",
        "pull",
        "unknown-model",
        "--path",
        other.to_str().unwrap(),
        "--yes",
    ]);
    assert!(!out.status.success());
    assert!(!other.join("models").exists());
    for (path, installed) in [(&root, true), (&other, false)] {
        let out = cli(&[
            "model",
            "ls",
            "--available",
            "--path",
            path.to_str().unwrap(),
            "--format",
            "json",
        ]);
        assert!(out.status.success(), "{}", stderr(&out));
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["models"][0]["installed"], installed);
    }
    let out = cli(&[
        "model",
        "ls",
        "--available",
        "--path",
        "/nonexistent-postvec-root",
    ]);
    assert!(!out.status.success());
}

#[test]
fn an_anonymous_pull_installs_verifies_and_is_idempotent() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());

    let output = run_pull(&fixture, &root, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let installed = root.join("models/onnx-runtime").join(MODEL);
    assert!(installed.join("ninference.hub.json").is_file());
    assert!(installed.join("onnx/model.onnx").is_file());
    let receipt: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(installed.join(".postvec-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["name"], MODEL);
    assert_eq!(
        receipt["archive_digest"],
        format!("sha256:{}", fixture.digest_hex)
    );
    // No staging leftovers.
    assert_eq!(
        std::fs::read_dir(root.join("models/.staging"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );

    // Second pull: a no-op, still successful.
    let output = run_pull(&fixture, &root, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("already installed"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_partial_download_resumes_with_a_range_request() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());

    // Leave a genuine prefix in staging, as an interrupted pull would.
    let staging = root.join("models/.staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        staging.join(format!("{}.part", fixture.digest_hex)),
        &fixture.archive[..100_000],
    )
    .unwrap();

    let output = run_pull(&fixture, &root, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(root.join("models/onnx-runtime").join(MODEL).is_dir());
}

#[test]
fn a_mirror_that_ignores_range_causes_a_clean_restart_not_appending() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    fixture.ignore_range.store(true, Ordering::SeqCst);
    let root = engine_root(work.path());

    let staging = root.join("models/.staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        staging.join(format!("{}.part", fixture.digest_hex)),
        &fixture.archive[..50_000],
    )
    .unwrap();

    // Success proves truncate-and-restart. Appending the 200 body would fail the digest.
    let output = run_pull(&fixture, &root, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn a_digest_mismatch_fails_closed_with_no_extraction() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    fixture.corrupt.store(true, Ordering::SeqCst);
    let root = engine_root(work.path());

    let output = run_pull(&fixture, &root, &[]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("digest mismatch"),
        "{}",
        stderr(&output)
    );
    assert!(!root.join("models/onnx-runtime").join(MODEL).exists());
}

#[test]
fn dry_run_downloads_nothing_and_changes_nothing() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());

    let output = run_pull(&fixture, &root, &["--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!root.join("models/onnx-runtime").join(MODEL).exists());
    assert!(!root.join("models/.staging").exists());
    let _ = &fixture.index_body;
}

#[test]
fn a_first_pull_creates_models_inside_an_existing_root() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());
    std::fs::remove_dir_all(root.join("models")).unwrap();

    let output = run_pull(&fixture, &root, &["--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!root.join("models").exists());

    let output = run_pull(&fixture, &root, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(root.join("models/onnx-runtime").join(MODEL).is_dir());

    let missing = root.join("typo");
    let output = run_pull(&fixture, &missing, &[]);
    assert_ne!(output.status.code(), Some(0));
    assert!(!missing.exists());
}

/// `pull` will not replace an installed revision. `upgrade` does, in place, and the old archive stays retrievable.
#[test]
fn a_published_revision_is_installed_then_upgraded_in_place() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());
    let model_dir = root.join("models/onnx-runtime").join(MODEL);
    let weight = model_dir.join("onnx/model.onnx");

    // Revision 1 installs.
    let output = run_pull(&fixture, &root, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_revision(&model_dir), 1);
    assert_eq!(std::fs::read(&weight).unwrap()[0], 42);
    let first_digest = fixture.digest_hex.clone();
    let first_archive_url = format!("http://{}/archives/sha256/{first_digest}", fixture.address);
    assert_eq!(http_status(&first_archive_url), 200);

    // The registry publishes different bytes under the same name.
    let second_digest = fixture.publish_revision(work.path(), 2, 7);
    assert_ne!(second_digest, first_digest);

    // `pull` will not replace them: it says what to run instead.
    let output = run_pull(&fixture, &root, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("postvec model upgrade"),
        "{}",
        stderr(&output)
    );
    assert_eq!(installed_revision(&model_dir), 1, "pull must not replace");
    assert_eq!(std::fs::read(&weight).unwrap()[0], 42);

    // The explicit verb does, in place, keeping the name.
    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_revision(&model_dir), 2);
    assert_eq!(std::fs::read(&weight).unwrap()[0], 7);

    // One directory holds this name. Upgrade replaces in place.
    assert_eq!(
        std::fs::read_dir(root.join("models/onnx-runtime"))
            .unwrap()
            .count(),
        1
    );

    // The install verifies against its own receipt, offline.
    let output = Command::new(binary())
        .args([
            "model",
            "show",
            MODEL,
            "--path",
            root.to_str().unwrap(),
            "--verify",
        ])
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec");
    assert!(output.status.success(), "{}", stderr(&output));

    // Revision 1's bytes are still there: archives are never rebound.
    assert_eq!(http_status(&first_archive_url), 200);

    // The head is already installed, so a second upgrade has nothing to do.
    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let reported = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(reported.contains("head revision"), "{reported}");
}

fn descriptor_enabled(model_dir: &Path) -> bool {
    let body = std::fs::read_to_string(model_dir.join("ninference.hub.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["enabled"]
        .as_bool()
        .expect("the descriptor states enabled explicitly")
}

fn run_model(root: &Path, args: &[&str]) -> Output {
    let mut full = vec!["model"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--path", root.to_str().unwrap()]);
    Command::new(binary())
        .args(&full)
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec")
}

/// A pull lands `enabled: false` even though the archive is `enabled: true`.
/// Leaving the published bit set would activate the model at the next restart.
#[test]
fn a_pull_installs_deactivated_and_activate_is_what_turns_it_on() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());
    let model_dir = root.join("models/onnx-runtime").join(MODEL);

    let output = run_pull(&fixture, &root, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !descriptor_enabled(&model_dir),
        "a fresh install must land deactivated"
    );
    // The receipt covers the installed descriptor, so a deactivated model still verifies.
    let output = run_model(&root, &["show", MODEL, "--verify"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let reported = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(reported.contains("deactivated"), "{reported}");

    // `ls` says the same word.
    let listed = run_model(&root, &["ls"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("deactivated"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );

    // Pull points at the command that finishes the job.
    assert!(
        stderr(&output).contains("activate") || reported.contains("activate"),
        "{reported}"
    );

    // Activation flips the persistent bit. `--path` has no engine to load into.
    let output = run_model(&root, &["activate", MODEL, "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(descriptor_enabled(&model_dir));
    assert!(
        run_model(&root, &["show", MODEL, "--verify"])
            .status
            .success(),
        "the receipt must stay verifiable across a flip"
    );

    // Deactivation flips it back, and the receipt still verifies.
    let output = run_model(&root, &["deactivate", MODEL, "--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!descriptor_enabled(&model_dir));
    assert!(run_model(&root, &["show", MODEL, "--verify"])
        .status
        .success());

    // Idempotent in both directions.
    assert!(run_model(&root, &["deactivate", MODEL, "--yes"])
        .status
        .success());
    assert!(!descriptor_enabled(&model_dir));
}

/// An upgrade keeps the previous activation state. Replacing bytes does not decide whether the model should serve.
#[test]
fn an_upgrade_preserves_the_activation_state() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());
    let model_dir = root.join("models/onnx-runtime").join(MODEL);
    let weight = model_dir.join("onnx/model.onnx");

    assert!(run_pull(&fixture, &root, &[]).status.success());
    assert!(!descriptor_enabled(&model_dir));

    // A deactivated model stays deactivated across the upgrade.
    fixture.publish_revision(work.path(), 2, 7);
    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_revision(&model_dir), 2);
    assert_eq!(std::fs::read(&weight).unwrap()[0], 7);
    assert!(
        !descriptor_enabled(&model_dir),
        "an upgrade must not activate a deactivated model"
    );
    assert!(run_model(&root, &["show", MODEL, "--verify"])
        .status
        .success());

    // An activated model stays activated across the upgrade.
    assert!(run_model(&root, &["activate", MODEL, "--yes"])
        .status
        .success());
    fixture.publish_revision(work.path(), 3, 9);
    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_revision(&model_dir), 3);
    assert_eq!(std::fs::read(&weight).unwrap()[0], 9);
    assert!(
        descriptor_enabled(&model_dir),
        "an upgrade must not deactivate a serving model"
    );
    assert!(run_model(&root, &["show", MODEL, "--verify"])
        .status
        .success());
}

/// Activation only rewrites an install this CLI made.
/// A directory with no receipt cannot keep `--verify` true, and a package upgrade would overwrite the bit.
#[test]
fn activation_refuses_a_directory_this_cli_did_not_install() {
    let work = tempfile::tempdir().unwrap();
    let root = engine_root(work.path());
    let manual = root.join("models/onnx-runtime/hand-placed");
    std::fs::create_dir_all(&manual).unwrap();
    std::fs::set_permissions(&manual, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        manual.join("ninference.hub.json"),
        r#"{"name":"hand-placed","backend":"onnx-runtime","enabled":true}"#,
    )
    .unwrap();

    for verb in ["activate", "deactivate"] {
        let output = run_model(&root, &[verb, "hand-placed", "--yes"]);
        assert!(!output.status.success(), "{verb} must refuse");
        assert!(
            stderr(&output).contains("not installed by this CLI"),
            "{}",
            stderr(&output)
        );
    }
    // Untouched.
    let body = std::fs::read_to_string(manual.join("ninference.hub.json")).unwrap();
    assert!(body.contains(r#""enabled":true"#), "{body}");
}

/// An upgrade never invents an install, and a stale index never walks one backwards.
#[test]
fn upgrade_refuses_absent_installs_and_older_heads() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());

    let output = run_upgrade(&fixture, &root, &[MODEL]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("postvec model pull"),
        "{}",
        stderr(&output)
    );

    // Install revision 2, then serve revision 1 again (a stale cache or an index rolled back by hand).
    fixture.publish_revision(work.path(), 2, 7);
    assert!(run_pull(&fixture, &root, &[]).status.success());
    fixture.publish_revision(work.path(), 1, 42);

    let output = run_upgrade(&fixture, &root, &[MODEL]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("revision 2 is installed"), "{message}");
    assert!(message.contains("no downgrade"), "{message}");
}

fn run_upgrade(fixture: &Fixture, root: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "model",
        "upgrade",
        "--path",
        root.to_str().unwrap(),
        "--yes",
    ];
    args.extend_from_slice(extra);
    Command::new(binary())
        .args(&args)
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env_remove("POSTVEC_API_KEY")
        .env("XDG_CONFIG_HOME", "/nonexistent-config-home")
        .env("POSTVEC_REGISTRY_PUBLIC_INDEX_URL", fixture.index_url())
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec")
}

fn installed_revision(model_dir: &Path) -> u64 {
    let body = std::fs::read_to_string(model_dir.join(".postvec-install.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["revision"]
        .as_u64()
        .expect("receipts record their revision")
}

/// `upgrade --all` takes the lock and recovers before it decides what is outdated.
/// A pre-lock scan of an interrupted upgrade would see the head already installed and leave the transaction open.
#[test]
fn upgrade_all_recovers_before_deciding_nothing_is_outdated() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());
    let model_dir = root.join("models/onnx-runtime").join(MODEL);

    assert!(run_pull(&fixture, &root, &[]).status.success());
    fixture.publish_revision(work.path(), 2, 7);
    assert!(run_upgrade(&fixture, &root, &["--all"]).status.success());
    assert_eq!(installed_revision(&model_dir), 2);

    // Crash after the swap: revision 2 is installed, revision 1 is parked and the record says "swapping".
    // A pre-lock scan would see the head already installed and do nothing.
    let swap = root.join("models/.swap/onnx-runtime").join(MODEL);
    std::fs::create_dir_all(&swap).unwrap();
    std::fs::write(
        swap.join("ninference.hub.json"),
        std::fs::read(model_dir.join("ninference.hub.json")).unwrap(),
    )
    .unwrap();
    std::fs::write(swap.join("marker"), b"predecessor").unwrap();
    std::fs::write(
        root.join("models/.swap/txn.json"),
        serde_json::json!({
            "schema_version": 3,
            "phase": "swapping",
            "models": [{
                "name": MODEL,
                "backend": "onnx-runtime",
                "role": "replace",
                "reload_on_rollback": false
            }]
        })
        .to_string(),
    )
    .unwrap();

    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("recovering an interrupted model batch"),
        "{}",
        stderr(&output)
    );
    // The predecessor is back and the record is gone.
    assert_eq!(
        std::fs::read_to_string(model_dir.join("marker")).unwrap(),
        "predecessor"
    );
    assert!(!root.join("models/.swap").exists());
}

const TEST_KEY: &str = "uv_integration_test_key_000000";

/// Run against the authenticated channel: key from the environment, auth index pointed at the fixture.
fn run_authenticated(fixture: &Fixture, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env("XDG_CONFIG_HOME", "/nonexistent-config-home")
        .env("POSTVEC_API_KEY", TEST_KEY)
        .env("POSTVEC_REGISTRY_PUBLIC_INDEX_URL", fixture.index_url())
        .env("POSTVEC_REGISTRY_AUTH_INDEX_URL", fixture.auth_index_url())
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec")
}

/// `whoami` reports each served entitlement's id, source and expiry. `ls --available` shows only the served view.
#[test]
fn whoami_reports_served_entitlements_and_ls_shows_the_served_view() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    fixture.serve_authenticated_index(serde_json::json!({
        "entitlements": [
            { "id": "postvec-catalog", "source": "default", "expires_at": null }
        ]
    }));

    let output = run_authenticated(&fixture, &["whoami"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let human = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(human.contains("postvec-catalog (default)"), "{human}");

    let output = run_authenticated(&fixture, &["whoami", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("whoami --format json emits JSON");
    assert_eq!(
        document["entitlements"],
        serde_json::json!([{ "id": "postvec-catalog", "source": "default" }])
    );
    assert_eq!(document["channel"], "private");

    // `ls --available` renders the served view and nothing more.
    let output = run_authenticated(&fixture, &["model", "ls", "--available"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let listing = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(listing.contains(MODEL), "{listing}");
    assert!(!listing.contains("postvec-catalog"), "{listing}");
}

/// An anonymous `whoami` invents no viewer and no entitlement. The public document has none, and the report omits the field.
#[test]
fn an_anonymous_whoami_reports_no_entitlements() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());

    let output = Command::new(binary())
        .args(["whoami", "--format", "json"])
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env_remove("POSTVEC_API_KEY")
        .env("XDG_CONFIG_HOME", "/nonexistent-config-home")
        .env("POSTVEC_REGISTRY_PUBLIC_INDEX_URL", fixture.index_url())
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["signed_in"], false);
    assert!(document.get("entitlements").is_none(), "{document}");
}

/// A failing credential fails the command and never falls back to the public catalogue. `logout` is the only path to anonymous.
#[test]
fn a_failing_credential_never_falls_back_to_the_public_catalogue() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    fixture.reject_auth.store(true, Ordering::SeqCst);
    let root = engine_root(work.path());

    // The public fixture would serve this model. The pull must still fail on the rejected credential.
    let output = run_authenticated(
        &fixture,
        &[
            "model",
            "pull",
            MODEL,
            "--path",
            root.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("rejected the credential"),
        "{}",
        stderr(&output)
    );
    assert!(!root.join("models/onnx-runtime").join(MODEL).exists());

    // `whoami` reports the failing credential and exits non-zero, not as an anonymous session.
    let output = run_authenticated(&fixture, &["whoami"]);
    assert_ne!(output.status.code(), Some(0));
    let human = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(human.contains("FAILS authentication"), "{human}");
}

/// An authenticated pull of a name absent from the served view gets one non-leaking message pointing at `whoami` and the dashboard.
#[test]
fn an_absent_name_on_the_authenticated_view_is_reported_without_leaking() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    fixture.serve_authenticated_index(serde_json::json!({ "entitlements": [] }));
    let root = engine_root(work.path());

    let output = run_authenticated(
        &fixture,
        &[
            "model",
            "pull",
            "ghost-model",
            "--path",
            root.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_ne!(output.status.code(), Some(0));
    let message = stderr(&output);
    assert!(
        message.contains("ghost-model is not in your catalogue"),
        "{message}"
    );
    assert!(message.contains("postvec whoami"), "{message}");
    assert!(message.contains("univec.ai/dashboard"), "{message}");
    assert!(!message.contains("does not offer"), "{message}");
}

fn receipt_json(model_dir: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(model_dir.join(".postvec-install.json")).unwrap())
        .unwrap()
}

/// `--yes` is not a notice acknowledgement. A stale flag refuses, the same document stays quiet and a new document asks again.
#[test]
fn a_notice_document_gates_pull_and_upgrade_end_to_end() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());
    let model_dir = root.join("models/onnx-runtime").join(MODEL);

    // Revision 1 under a versioned notice document.
    fixture.publish_notice_revision(work.path(), 1, 42, "2026-08-09");

    // Dry run names the document and the exact flag, and changes nothing.
    let output = run_pull(&fixture, &root, &["--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let plan = stderr(&output);
    assert!(plan.contains("terms in this plan:"), "{plan}");
    assert!(
        plan.contains("univec-commercial 2026-08-09 (notice)"),
        "{plan}"
    );
    assert!(
        plan.contains("--accept-license univec-commercial@2026-08-09"),
        "{plan}"
    );
    assert!(!model_dir.exists());
    assert_eq!(fixture.archive_request_count(), 0);

    // `--yes`, which `run_pull` always passes, is not the notice acknowledgement, and nothing is downloaded.
    let output = run_pull(&fixture, &root, &[]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("--accept-license univec-commercial@2026-08-09"),
        "{message}"
    );
    assert!(message.contains("--yes"), "{message}");
    assert!(!model_dir.exists());
    assert_eq!(
        fixture.archive_request_count(),
        0,
        "the refusal must precede any download"
    );

    // A wrong-version flag is stale, and still nothing is downloaded.
    let output = run_pull(
        &fixture,
        &root,
        &["--accept-license", "univec-commercial@1999-01-01"],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("stale"), "{}", stderr(&output));
    assert_eq!(fixture.archive_request_count(), 0);

    // The exact flag acknowledges, downloads, installs and records the evidence in the receipt.
    let output = run_pull(
        &fixture,
        &root,
        &["--accept-license", "univec-commercial@2026-08-09"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(fixture.archive_request_count() > 0);
    let receipt = receipt_json(&model_dir);
    assert_eq!(receipt["license"], "univec-commercial");
    assert_eq!(receipt["license_version"], "2026-08-09");
    assert_eq!(
        receipt["license_url"],
        "https://univec.ai/legal/models/univec-commercial/2026-08-09"
    );
    assert_eq!(receipt["license_acceptance_method"], "flag");
    let first_accepted_at = receipt["license_accepted_at"]
        .as_str()
        .expect("acknowledgement recorded")
        .to_string();

    // Same document, newer revision: the installed evidence satisfies the notice, so `--yes` alone upgrades.
    // The replacement receipt keeps the original acknowledgement.
    fixture.publish_notice_revision(work.path(), 2, 7, "2026-08-09");
    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_revision(&model_dir), 2);
    let receipt = receipt_json(&model_dir);
    assert_eq!(receipt["license_accepted_at"], first_accepted_at.as_str());
    assert_eq!(receipt["license_acceptance_method"], "flag");

    // The exact document changed: the old evidence does not carry.
    fixture.publish_notice_revision(work.path(), 3, 9, "2026-09-01");
    let output = run_upgrade(&fixture, &root, &["--all"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--accept-license univec-commercial@2026-09-01"),
        "{}",
        stderr(&output)
    );
    assert_eq!(installed_revision(&model_dir), 2, "nothing was replaced");

    let output = run_upgrade(
        &fixture,
        &root,
        &["--all", "--accept-license", "univec-commercial@2026-09-01"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_revision(&model_dir), 3);
    let receipt = receipt_json(&model_dir);
    assert_eq!(receipt["license_version"], "2026-09-01");
    assert_eq!(receipt["license_acceptance_method"], "flag");
    assert!(receipt["license_accepted_at"].is_string());
}

/// The install refuses an archive whose descriptor disagrees with the stable index entry. The engine would believe the descriptor.
#[test]
fn an_archive_whose_descriptor_contradicts_the_index_is_refused() {
    let work = tempfile::tempdir().unwrap();
    let fixture = Fixture::start(work.path());
    let root = engine_root(work.path());

    // Same advertised entry, but the archive declares 16 dimensions.
    fixture.publish_lying_archive(work.path());
    let output = run_pull(&fixture, &root, &[]);
    assert!(!output.status.success(), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("target_dim"), "{message}");
    assert!(message.contains("different shape"), "{message}");
    assert!(!root.join("models/onnx-runtime").join(MODEL).exists());
}

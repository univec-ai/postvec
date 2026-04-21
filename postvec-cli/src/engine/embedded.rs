//! Probing an embedded-mode installation: the engine root on disk, and the
//! launcher's loopback listeners.
//!
//! Everything here mirrors what the `engine` crate actually does, because a
//! diagnosis that disagrees with the loader is worse than no diagnosis:
//!
//! - models live at exactly `<root>/models/<backend>/<model>/ninference.hub.json`
//!   (two directory levels, no deeper — `engine/src/lib.rs`);
//! - scan-loading takes descriptors whose `enabled` is true and registers them
//!   under `configuration.name`;
//! - an explicitly requested model is resolved by matching the *directory*
//!   name, which is why both names are recorded;
//! - the ONNX Runtime library is found by an exact filename match
//!   (`libonnxruntime.so` on Linux), so a versioned `libonnxruntime.so.1.22.0`
//!   alone will not satisfy the loader.

use super::{parse_config_body, HttpProbes, ProbeOutcome};
use crate::cli::TlsPolicy;
use crate::error::Result;
use crate::facts::{DescriptorError, DiskModel, EmbeddedProbe, ListenerProbe, RootSource};
use crate::proc::{self, Cmd, OsAccount};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Exactly the filename `engine::initialize_onnx` looks for.
#[cfg(target_os = "macos")]
const ORT_LIBRARY: &str = "libonnxruntime.dylib";
#[cfg(not(target_os = "macos"))]
const ORT_LIBRARY: &str = "libonnxruntime.so";

const DESCRIPTOR_FILE: &str = "ninference.hub.json";

/// A deliberately minimal descriptor schema.
///
/// Not the `engine` crate's `ModelConfiguration`: depending on it would drag
/// the engine (and ONNX Runtime) into a diagnostic tool, and would make an
/// engine-side schema change break the CLI's ability to report the problem.
#[derive(Debug, Deserialize)]
struct DescriptorFile {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    backend: Option<serde_json::Value>,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// Inspect an engine root. Never fails: every problem becomes a recorded fact.
pub fn inspect_root(
    root: Option<PathBuf>,
    source: RootSource,
    owner: Option<&OsAccount>,
) -> EmbeddedProbe {
    let mut probe = EmbeddedProbe::new(root.clone(), source);
    let Some(root) = root else {
        probe.root_error = Some("no engine root is configured: postvec.path is unset".to_string());
        return probe;
    };
    if !root.is_absolute() {
        probe.root_error = Some(format!(
            "{} is not absolute; the server resolves it from its own working directory",
            root.display()
        ));
        return probe;
    }
    if !root.is_dir() {
        probe.root_error = Some(format!("{} is not a directory", root.display()));
        return probe;
    }
    let _ = owner; // readability is checked separately, as it needs a subprocess

    let models_dir = root.join("models");
    if models_dir.is_dir() {
        probe.models_dir = Some(models_dir.clone());
        scan_descriptors(&models_dir, &mut probe);
    } else {
        probe.models_dir_error = Some(format!("{} does not exist", models_dir.display()));
    }

    probe.ort_libraries = find_ort_libraries(&root.join("libs"));
    probe
}

/// Walk `<root>/models/<backend>/<model>/ninference.hub.json`, exactly two
/// levels deep — skipping dot-directories at both levels, exactly as the
/// engine's own scan does. `models/.staging`, `.trash` and `.swap` are the
/// CLI's private transaction state, and reporting a model out of one of them
/// would describe an inventory the engine will never load.
fn is_scannable(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| !name.starts_with('.'))
}

fn scan_descriptors(models_dir: &Path, probe: &mut EmbeddedProbe) {
    let Ok(backends) = std::fs::read_dir(models_dir) else {
        probe.models_dir_error = Some(format!("{} is not readable", models_dir.display()));
        return;
    };
    let mut backend_dirs: Vec<PathBuf> = backends
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && is_scannable(p))
        .collect();
    backend_dirs.sort();
    for backend_dir in backend_dirs {
        let Ok(models) = std::fs::read_dir(&backend_dir) else {
            probe.descriptor_errors.push(DescriptorError {
                path: backend_dir.clone(),
                error: "directory is not readable".to_string(),
            });
            continue;
        };
        let mut model_dirs: Vec<PathBuf> = models
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && is_scannable(p))
            .collect();
        model_dirs.sort();
        for model_dir in model_dirs {
            let descriptor = model_dir.join(DESCRIPTOR_FILE);
            if !descriptor.is_file() {
                // Not an error: a model directory may legitimately hold only
                // weights while the descriptor lives elsewhere in the tree the
                // operator is assembling. The engine simply skips it, and so do we.
                continue;
            }
            let dir_name = model_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let backend_name = backend_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string());
            match read_descriptor(&descriptor) {
                Ok(parsed) => probe.descriptors.push(DiskModel {
                    name: parsed.name,
                    dir_name,
                    enabled: parsed.enabled,
                    backend: parsed.backend.or(backend_name),
                    dependencies: parsed.dependencies,
                    path: descriptor,
                }),
                // Recorded rather than returned, so one run reports every
                // broken descriptor instead of only the first.
                Err(error) => probe.descriptor_errors.push(DescriptorError {
                    path: descriptor,
                    error,
                }),
            }
        }
    }
    probe.descriptors.sort_by(|a, b| a.name.cmp(&b.name));
}

struct ParsedDescriptor {
    name: String,
    enabled: bool,
    backend: Option<String>,
    dependencies: Vec<String>,
}

fn read_descriptor(path: &Path) -> std::result::Result<ParsedDescriptor, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{e}"))?;
    let parsed: DescriptorFile = serde_json::from_str(&raw).map_err(|e| format!("{e}"))?;
    if parsed.name.trim().is_empty() {
        return Err("field \"name\" is empty".to_string());
    }
    Ok(ParsedDescriptor {
        name: parsed.name.trim().to_string(),
        enabled: parsed.enabled,
        // `backend` may be a string or a richer value depending on version;
        // render whatever is there without pretending to understand it.
        backend: parsed.backend.and_then(|value| match value {
            serde_json::Value::String(s) => Some(s),
            serde_json::Value::Null => None,
            other => Some(other.to_string()),
        }),
        dependencies: parsed.dependencies,
    })
}

/// Exact-name matches (what the loader accepts) plus, separately, versioned
/// near-misses — a root that has only `libonnxruntime.so.1.22.0` fails to load
/// with a message that does not obviously mean "make a symlink".
pub fn find_ort_libraries(libs_dir: &Path) -> Vec<PathBuf> {
    if !libs_dir.is_dir() {
        return Vec::new();
    }
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(libs_dir)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| resolves_to_file(entry.path()))
        .filter(|entry| entry.file_name().to_string_lossy() == ORT_LIBRARY)
        .map(|entry| entry.into_path())
        .collect();
    found.sort();
    found
}

/// Whether a path is, or points at, a regular file.
///
/// The unversioned `libonnxruntime.so` is a **symlink** in every normal
/// layout — it is how upstream ships the runtime and how every distribution
/// packages a shared library. `dlopen` follows it without noticing, so a check
/// that only counts regular files rejects a perfectly good engine root and
/// tells the operator to create a symlink that is already there.
///
/// `Path::is_file` follows the link, which also makes a dangling symlink
/// correctly *not* count.
fn resolves_to_file(path: &Path) -> bool {
    path.is_file()
}

/// Versioned ONNX Runtime libraries that the loader's exact-name match will not
/// accept. Reported as remediation, never as success.
pub fn find_versioned_ort_libraries(libs_dir: &Path) -> Vec<PathBuf> {
    if !libs_dir.is_dir() {
        return Vec::new();
    }
    let prefix = format!("{ORT_LIBRARY}.");
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(libs_dir)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| resolves_to_file(entry.path()))
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.into_path())
        .collect();
    found.sort();
    found
}

/// Can the PostgreSQL account traverse and read the engine root?
///
/// Answered by asking the kernel as that account rather than by reimplementing
/// permission resolution (which would get ACLs and supplementary groups subtly
/// wrong). `None` means the question could not be asked.
pub async fn readable_by(
    path: &Path,
    owner: Option<&OsAccount>,
    timeout: Duration,
) -> Option<bool> {
    let test = ["/usr/bin/test", "/bin/test"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())?;
    // Requires both read and traverse: the engine lists the directory and opens
    // files below it.
    let cmd = Cmd::new(test)
        .arg("-r")
        .arg(path.display().to_string())
        .arg("-a")
        .arg("-x")
        .arg(path.display().to_string())
        .run_as(owner);
    match proc::run(&cmd, timeout).await {
        Ok(output) => Some(output.ok()),
        Err(_) => None,
    }
}

/// Probe the launcher's loopback listeners and read the engine's own inventory.
pub async fn probe_listeners(
    probe: &mut EmbeddedProbe,
    grpc_listen: &str,
    http_listen: &str,
    timeout: Duration,
) -> Result<()> {
    probe.grpc_listener = Some(tcp_probe(grpc_listen, timeout).await);
    let http = tcp_probe(http_listen, timeout).await;
    let http_reachable = http.connected;
    probe.http_listener = Some(http);

    if !http_reachable {
        probe.loaded_error = Some(format!(
            "the engine's /config listener at {http_listen} is not accepting connections"
        ));
        return Ok(());
    }
    // The listener is loopback and plaintext by construction, so no TLS
    // question arises here.
    let clients = HttpProbes::new(TlsPolicy::ExtensionCompatible, timeout)?;
    let url = format!("http://{http_listen}/config");
    match clients.get(&url, false).await {
        ProbeOutcome::Answered { status, body, .. } => {
            if (200..=299).contains(&status) {
                match parse_config_body(&body) {
                    Ok(inventory) => probe.loaded = Some(inventory),
                    Err(e) => probe.loaded_error = Some(e.to_string()),
                }
            } else {
                probe.loaded_error = Some(format!("GET {url} returned HTTP {status}"));
            }
        }
        ProbeOutcome::Failed { detail } => probe.loaded_error = Some(detail),
    }
    Ok(())
}

async fn tcp_probe(address: &str, timeout: Duration) -> ListenerProbe {
    let mut probe = ListenerProbe {
        address: address.to_string(),
        connected: false,
        error: None,
    };
    let parsed: std::net::SocketAddr = match address.parse() {
        Ok(addr) => addr,
        Err(e) => {
            probe.error = Some(format!("{address:?} is not a valid host:port ({e})"));
            return probe;
        }
    };
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(parsed)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            probe.connected = true;
        }
        Ok(Err(e)) => probe.error = Some(format!("{e}")),
        Err(_) => {
            probe.error = Some(format!(
                "no answer within {}",
                humantime::format_duration(timeout)
            ))
        }
    }
    probe
}

/// Is something already listening on an address we are about to configure?
/// Used before a restart to warn about a port an unrelated service owns.
pub async fn port_is_taken(address: &str, timeout: Duration) -> bool {
    tcp_probe(address, timeout).await.connected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn model_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root
                .join("models/onnx-runtime/baai-bge-m3")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"baai-bge-m3","enabled":true,"backend":"onnx",
                "dependencies":[],"params":{"model_type":"embed"},"future":42}"#,
        );
        write(
            &root
                .join("models/generic/embed-bridge")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"embed-bridge","enabled":true,"dependencies":["baai-bge-m3"]}"#,
        );
        write(
            &root
                .join("models/onnx-runtime/turned-off")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"turned-off","enabled":false}"#,
        );
        write(
            &root
                .join("models/onnx-runtime/broken")
                .join(DESCRIPTOR_FILE),
            "{not json",
        );
        write(
            &root
                .join("models/onnx-runtime/nameless")
                .join(DESCRIPTOR_FILE),
            r#"{"enabled":true}"#,
        );
        // Too deep: the engine never looks here, so neither do we.
        write(
            &root
                .join("models/onnx-runtime/nested/deeper")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"too-deep","enabled":true}"#,
        );
        write(&root.join("libs/linux/x64").join(ORT_LIBRARY), "");
        dir
    }

    #[test]
    fn scans_descriptors_two_levels_deep_and_records_errors() {
        let dir = model_root();
        let probe = inspect_root(Some(dir.path().to_path_buf()), RootSource::Guc, None);
        assert!(probe.root_error.is_none());
        assert!(probe.models_dir.is_some());

        let names: Vec<&str> = probe.descriptors.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["baai-bge-m3", "embed-bridge", "turned-off"]);
        assert!(
            !names.contains(&"too-deep"),
            "a descriptor three levels down is invisible to the engine"
        );

        // Both broken descriptors are reported, not just the first.
        assert_eq!(probe.descriptor_errors.len(), 2);
        let messages: Vec<&str> = probe
            .descriptor_errors
            .iter()
            .map(|e| e.error.as_str())
            .collect();
        assert!(messages.iter().any(|m| m.contains("name")));

        assert_eq!(probe.ort_libraries.len(), 1);
    }

    /// `doctor` describes what the engine will load, so it must apply the
    /// engine's own directory rule: dot-directories are the CLI's private
    /// transaction state, and reporting a model out of one would describe an
    /// inventory that can never become resident.
    #[test]
    fn hidden_directories_are_not_part_of_the_inventory() {
        let dir = model_root();
        let root = dir.path();
        // An uncommitted extraction and an unfinished removal, at exactly the
        // depth the scan walks.
        write(
            &root
                .join("models/.staging/uncommitted.1234")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"uncommitted","enabled":true}"#,
        );
        write(
            &root
                .join("models/.trash/removed.5678")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"removed","enabled":true}"#,
        );
        // A hidden model directory under a real backend.
        write(
            &root
                .join("models/onnx-runtime/.partial")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"partial","enabled":true}"#,
        );

        let probe = inspect_root(Some(root.to_path_buf()), RootSource::Guc, None);
        let names: Vec<&str> = probe.descriptors.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["baai-bge-m3", "embed-bridge", "turned-off"]);
        assert!(
            !probe.expected_names(&[]).contains("uncommitted"),
            "an uncommitted model must never be reported as expected-to-load"
        );
        assert!(!probe.expected_names(&[]).contains("removed"));
    }

    #[test]
    fn expected_models_use_descriptor_names_for_scan_load() {
        let dir = model_root();
        let probe = inspect_root(Some(dir.path().to_path_buf()), RootSource::Guc, None);
        assert_eq!(
            probe.expected_names(&[]),
            ["baai-bge-m3".to_string(), "embed-bridge".to_string()].into(),
            "scan-load takes enabled descriptors only"
        );
    }

    #[test]
    fn explicit_requests_are_resolved_by_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        // Directory `bge` holding a descriptor that calls itself `baai-bge-m3`:
        // the engine finds it by directory but registers the descriptor name.
        write(
            &dir.path()
                .join("models/onnx-runtime/bge")
                .join(DESCRIPTOR_FILE),
            r#"{"name":"baai-bge-m3","enabled":true}"#,
        );
        let probe = inspect_root(Some(dir.path().to_path_buf()), RootSource::Guc, None);
        assert!(probe.descriptors[0].name_mismatch());
        assert_eq!(probe.directory_names(), ["bge".to_string()].into());
        assert_eq!(
            probe.expected_names(&["bge".to_string()]),
            ["baai-bge-m3".to_string()].into(),
            "a requested directory maps to the name the engine will load it under"
        );
    }

    #[test]
    fn a_missing_or_relative_root_is_reported_precisely() {
        let missing = inspect_root(
            Some(PathBuf::from("/nonexistent/engine")),
            RootSource::Guc,
            None,
        );
        assert!(missing.root_error.unwrap().contains("not a directory"));

        let relative = inspect_root(Some(PathBuf::from("relative")), RootSource::Guc, None);
        assert!(relative.root_error.unwrap().contains("not absolute"));

        let unset = inspect_root(None, RootSource::Unknown, None);
        assert!(unset.root_error.unwrap().contains("postvec.path"));
    }

    #[test]
    fn an_empty_root_is_valid_but_has_nothing_loadable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        let probe = inspect_root(Some(dir.path().to_path_buf()), RootSource::Guc, None);
        assert!(probe.root_error.is_none());
        assert!(probe.descriptors.is_empty());
        assert!(probe.ort_libraries.is_empty());
        assert!(probe.expected_names(&[]).is_empty());
    }

    #[test]
    fn versioned_ort_libraries_do_not_count_as_the_loadable_one() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path()
                .join("libs")
                .join(format!("{ORT_LIBRARY}.1.22.0")),
            "",
        );
        assert!(find_ort_libraries(&dir.path().join("libs")).is_empty());
        assert_eq!(
            find_versioned_ort_libraries(&dir.path().join("libs")).len(),
            1,
            "the near-miss is reported so the fix is obvious"
        );
    }

    /// The layout every real package has: one versioned file and an
    /// unversioned **symlink** pointing at it. `dlopen` follows the link, so a
    /// check that ignores symlinks rejects a working engine root and tells the
    /// operator to create a link that already exists.
    #[test]
    fn the_unversioned_library_may_be_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let libs = dir.path().join("libs").join("onnxruntime").join("lib");
        std::fs::create_dir_all(&libs).unwrap();
        write(&libs.join(format!("{ORT_LIBRARY}.1.22.0")), "");
        // The chain upstream actually ships: .so -> .so.1 -> .so.1.22.0.
        std::os::unix::fs::symlink(
            format!("{ORT_LIBRARY}.1.22.0"),
            libs.join(format!("{ORT_LIBRARY}.1")),
        )
        .unwrap();
        std::os::unix::fs::symlink(format!("{ORT_LIBRARY}.1"), libs.join(ORT_LIBRARY)).unwrap();

        let found = find_ort_libraries(&dir.path().join("libs"));
        assert_eq!(
            found.len(),
            1,
            "the symlink is a loadable library: {found:?}"
        );
        assert!(found[0].ends_with(ORT_LIBRARY));
    }

    /// …but a link pointing at nothing is not a library.
    #[test]
    fn a_dangling_symlink_is_not_a_library() {
        let dir = tempfile::tempdir().unwrap();
        let libs = dir.path().join("libs");
        std::fs::create_dir_all(&libs).unwrap();
        std::os::unix::fs::symlink("gone.so.1", libs.join(ORT_LIBRARY)).unwrap();
        assert!(find_ort_libraries(&libs).is_empty());
    }

    /// A loopback port nothing in this process will ever bind.
    ///
    /// The obvious way to get a closed port — bind an ephemeral one and drop
    /// the listener — is a race, not a fact: the kernel is free to hand that
    /// exact port to the next `127.0.0.1:0` bind, and this suite makes a lot
    /// of those in parallel. It failed that way. Port 1 is in the privileged
    /// range, so no test (and no unprivileged process) can take it, which
    /// makes "connection refused" the only possible outcome.
    const CLOSED_PORT: &str = "127.0.0.1:1";

    #[tokio::test]
    async fn listener_probes_distinguish_closed_ports_from_bad_addresses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let open = tcp_probe(&address, Duration::from_secs(1)).await;
        assert!(open.connected);
        drop(listener);

        let closed = tcp_probe(CLOSED_PORT, Duration::from_secs(1)).await;
        assert!(!closed.connected);
        assert!(closed.error.is_some());

        let malformed = tcp_probe("not-an-address", Duration::from_secs(1)).await;
        assert!(!malformed.connected);
        assert!(malformed.error.unwrap().contains("not a valid host:port"));
    }

    #[tokio::test]
    async fn loopback_config_is_read_from_the_engine_listener() {
        let server = crate::testing::HttpFixture::start(|path| match path {
            "/config" => (
                200,
                r#"{"success":true,"data":{"models":[
                    {"name":"loaded","configuration":{"enabled":true,"params":{}}}]}}"#
                    .to_string(),
            ),
            _ => (404, String::new()),
        })
        .await;
        let mut probe = EmbeddedProbe::new(Some(PathBuf::from("/opt/nin")), RootSource::Guc);
        let http = server.address();
        probe_listeners(&mut probe, &http, &http, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(probe.grpc_listener.as_ref().unwrap().connected);
        assert!(probe.http_listener.as_ref().unwrap().connected);
        assert_eq!(
            probe.loaded.as_ref().unwrap().enabled_names(),
            ["loaded".to_string()].into()
        );
        assert!(probe.loaded_error.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_listener_yields_an_explanation_not_an_error() {
        // Same reasoning as CLOSED_PORT: bind-and-drop is a race under a
        // parallel suite, a privileged port is a fact.
        let address = CLOSED_PORT;
        let mut probe = EmbeddedProbe::new(Some(PathBuf::from("/opt/nin")), RootSource::Guc);
        probe_listeners(&mut probe, address, address, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(probe.loaded.is_none());
        assert!(probe.loaded_error.unwrap().contains("not accepting"));
    }

    #[tokio::test]
    async fn readability_is_answered_by_the_kernel() {
        let dir = tempfile::tempdir().unwrap();
        // As ourselves, our own temp directory is readable.
        assert_eq!(
            readable_by(dir.path(), None, Duration::from_secs(5)).await,
            Some(true)
        );
        assert_eq!(
            readable_by(
                Path::new("/nonexistent/postvec-root"),
                None,
                Duration::from_secs(5)
            )
            .await,
            Some(false)
        );
    }
}

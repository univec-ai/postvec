//! The model registry client, shared by `postvec` (the CLI) and
//! `postvec-server` (the node): index, credentials, verified download,
//! receipts, engine-root install/swap and the filesystem discipline around
//! them. Only PostgreSQL-free code lives here; the CLI adds the cluster.
//!
//! Archive, descriptor and identity types come from `registry-schema` (same
//! crate the publisher links). This file turns a verified archive into a
//! staged, receipted tree; [`root::ModelRoot`] publishes it.

pub mod auth;
pub mod client;
pub mod error;
pub mod fs;
pub mod index;
pub mod receipt;
pub mod root;
pub mod urls;

// Archive, descriptor and identity rules live in `registry-schema`, which
// the publisher also links: one implementation of the format the client
// reads and the publisher writes. Re-exported here so
// `registry::archive::...` still names them.
pub use registry_schema::{archive, descriptor, identity};

use crate::error::{Error as CliError, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
use crate::index::IndexModel;
use crate::receipt::Receipt;
use crate::root::ModelRoot;
use std::path::{Component, Path, PathBuf};

/// A model extracted, validated and receipted in staging, not yet visible to
/// the engine. A fresh install renames it into place; an upgrade swaps it
/// over the existing copy (`root::ModelRoot::swap_in`).
#[derive(Debug)]
pub struct StagedModel {
    pub path: PathBuf,
    pub warnings: Vec<String>,
    /// The identity the **staged descriptor** declares — what the engine
    /// will believe once this directory is installed. An upgrade compares it
    /// against the install receipt before mutating anything.
    pub identity: identity::Identity,
}

/// Strict-extract a verified archive into staging, validate the descriptor
/// against the index entry and write the receipt. The tree is complete
/// and durable-on-rename after this; nothing is visible to the engine yet.
///
/// `license_evidence` is the local terms acknowledgement to record for a
/// notice-policy document, freshly obtained this run or preserved from
/// the receipt being replaced. `None` for none-policy documents.
///
/// `enabled` is the serving state the installed copy must land in. A published
/// archive is always `enabled: true` (`Descriptor::validate_against_index`
/// refuses anything else) and immutable; **installing is not activating**, so
/// a fresh install lands `false` and an upgrade carries the previous copy's
/// bit forward. The flip happens here, before the receipt is written, so the
/// receipt hashes the descriptor that will actually be installed — there is no
/// window in which a staged tree describes bytes it does not contain.
///
/// The caller holds the root's exclusive lock.
pub fn stage_from_archive(
    root: &ModelRoot,
    model: &IndexModel,
    archive_path: &Path,
    source_host: Option<String>,
    license_evidence: Option<&receipt::LicenseEvidence>,
    enabled: bool,
) -> Result<StagedModel> {
    root.ensure_staging()?;
    let staged = root.extract_dir(&model.name);
    // 0755 regardless of umask: the receipt writer refuses group-writable
    // parents, and the tree is renamed into place with these modes.
    crate::fs::create_dir_all_checked(&staged, 0o755)?;

    let result = (|| -> Result<StagedModel> {
        let mut files = archive::extract_strict(
            archive_path,
            &model.name,
            model.archive.installed_size,
            &staged,
        )?;
        let (warnings, descriptor) = validate_staged_descriptor(&staged, model)?;
        let identity = descriptor.identity();
        if !enabled {
            set_staged_enabled(&staged, false)?;
            remeasure(&mut files, &staged, DESCRIPTOR_FILE)?;
        }
        Receipt::new(model, source_host, &files, &identity, license_evidence).write(&staged)?;
        Ok(StagedModel {
            path: staged.clone(),
            warnings,
            identity,
        })
    })();

    if result.is_err() {
        // Interrupted staging is also swept by the next lock holder; cleaning
        // here just does it sooner.
        let _ = std::fs::remove_dir_all(&staged);
    }
    result
}

const DESCRIPTOR_FILE: &str = "ninference.hub.json";

/// Rewrite `enabled` in a staged (not yet installed) descriptor, preserving
/// every other key. Nothing is visible to the engine yet, so this needs none
/// of the durability ceremony the installed-copy flip in
/// [`root::ModelRoot::set_enabled`] carries — the whole tree is fsynced and
/// renamed into place afterwards.
fn set_staged_enabled(staged: &Path, enabled: bool) -> Result<()> {
    let path = staged.join(DESCRIPTOR_FILE);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| CliError::apply(format!("cannot read {}: {e}", path.display())))?;
    let mut value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| CliError::precondition(format!("{} is malformed: {e}", path.display())))?;
    value
        .as_object_mut()
        .ok_or_else(|| CliError::precondition(format!("{} is not a JSON object", path.display())))?
        .insert("enabled".to_string(), serde_json::Value::Bool(enabled));
    let mut body = serde_json::to_vec_pretty(&value)
        .map_err(|e| CliError::internal(format!("cannot serialize the descriptor: {e}")))?;
    body.push(b'\n');
    crate::fs::write_atomic(&path, &body, 0o644)
}

/// Re-measure one extracted file after it was rewritten in staging, so the
/// receipt records the bytes that will actually be installed.
fn remeasure(files: &mut [archive::ExtractedFile], staged: &Path, relative: &str) -> Result<()> {
    let path = staged.join(relative);
    let entry = files
        .iter_mut()
        .find(|file| file.path == relative)
        .ok_or_else(|| CliError::internal(format!("{relative} was not part of the extraction")))?;
    let content = std::fs::read(&path)
        .map_err(|e| CliError::apply(format!("cannot read {}: {e}", path.display())))?;
    entry.size = content.len() as u64;
    entry.sha256 = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&content))
    };
    Ok(())
}

/// Descriptor gate through the shared [`descriptor`] module: the staged
/// descriptor must agree with the index entry on the fields install
/// correctness depends on (name, backend, model type), and every runtime
/// asset it references must exist inside the staged tree. Disagreements
/// in the advertising fields come back as warnings. The parsed descriptor
/// comes back too; the staged identity is derived from it.
fn validate_staged_descriptor(
    staged: &Path,
    model: &IndexModel,
) -> Result<(Vec<String>, descriptor::Descriptor)> {
    let descriptor_path = staged.join("ninference.hub.json");
    let content = std::fs::read_to_string(&descriptor_path).map_err(|_| {
        CliError::precondition(format!(
            "{}: archive has no ninference.hub.json at the model root",
            model.name
        ))
    })?;
    let descriptor = descriptor::Descriptor::parse(&content)
        .map_err(|e| CliError::precondition(format!("{}: {e}", model.name)))?;
    let warnings = descriptor
        .validate_against_index(model)
        .map_err(|e| CliError::precondition(format!("{}: {e}", model.name)))?;

    // Every referenced runtime asset must resolve inside the staged
    // directory; everything else in the archive is covered by the digest and
    // the receipt's per-file hashes.
    for asset in &descriptor.required_assets {
        require_staged_file(staged, &asset.path, &model.name, &asset.what)?;
    }
    Ok((warnings, descriptor))
}

fn require_staged_file(staged: &Path, relative: &str, model: &str, what: &str) -> Result<()> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(CliError::precondition(format!(
            "{model}: descriptor {what} {relative:?} is not a plain relative path"
        )));
    }
    let resolved = staged.join(path);
    if !resolved.is_file() {
        return Err(CliError::precondition(format!(
            "{model}: descriptor {what} {relative:?} does not exist in the archive"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{write_deterministic, ArchiveInput};
    use crate::index::{ArchiveInfo, IndexModel};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn secure_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn build_archive(dir: &Path, name: &str, descriptor: &str) -> (PathBuf, u64, String) {
        let src = dir.join("src");
        fs::create_dir_all(src.join("onnx")).unwrap();
        fs::write(src.join("descriptor.json"), descriptor).unwrap();
        fs::write(src.join("weights.onnx"), vec![1u8; 100]).unwrap();
        let inputs = vec![
            ArchiveInput {
                relative_path: "ninference.hub.json".into(),
                source: src.join("descriptor.json"),
            },
            ArchiveInput {
                relative_path: "onnx/model.onnx".into(),
                source: src.join("weights.onnx"),
            },
        ];
        let path = dir.join("a.tar");
        let mut out = fs::File::create(&path).unwrap();
        let written = write_deterministic(name, &inputs, &mut out).unwrap();
        (path, written.installed_size, written.digest)
    }

    fn model(name: &str, installed_size: u64, digest: String) -> IndexModel {
        IndexModel {
            name: name.into(),
            access: "public".into(),
            model_type: "embed".into(),
            backend: "onnx-runtime".into(),
            quantization: None,
            source_model: None,
            target_model: None,
            source_dim: None,
            target_dim: Some(384),
            sequence_len: None,
            license: Some("mit".into()),
            license_version: None,
            license_url: None,
            license_acceptance: None,
            source: None,
            dependencies: vec![],
            postvec_requires: vec![],
            min_postvec_version: None,
            published_at: None,
            summary: None,
            eval: None,
            withdrawn: false,
            revision: None,
            required_entitlements: vec![],
            archive: ArchiveInfo {
                digest,
                size: 1,
                installed_size,
                sources: vec!["https://example.invalid/a".into()],
            },
        }
    }

    #[test]
    fn a_valid_archive_installs_atomically_with_a_receipt() {
        let dir = secure_tempdir();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        let root = ModelRoot::new(dir.path().to_path_buf());
        let descriptor = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
                             "file_path":"onnx/model.onnx",
                             "params":{"model_type":"embed","target_dim":384}}"#;
        let (archive, installed_size, digest) = build_archive(dir.path(), "m", descriptor);
        let entry = model("m", installed_size, digest);
        let staged = stage_from_archive(
            &root,
            &entry,
            &archive,
            Some("example.invalid".into()),
            None,
            false,
        )
        .unwrap();
        assert!(staged.warnings.is_empty(), "{:?}", staged.warnings);
        let path = root
            .install_staged(&staged.path, &entry.backend, &entry.name)
            .unwrap();
        assert!(path.ends_with("models/onnx-runtime/m"));
        let receipt = Receipt::read(&path).unwrap().expect("receipt written");
        assert_eq!(receipt.name, "m");
        assert_eq!(receipt.files.len(), 2);
        // Staging left nothing behind.
        assert_eq!(fs::read_dir(root.staging_dir()).unwrap().count(), 0);
    }

    #[test]
    fn a_descriptor_name_mismatch_is_refused_before_install() {
        let dir = secure_tempdir();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        let root = ModelRoot::new(dir.path().to_path_buf());
        let descriptor = r#"{"name":"other","backend":"onnx-runtime"}"#;
        let (archive, installed_size, digest) = build_archive(dir.path(), "m", descriptor);
        let err = stage_from_archive(
            &root,
            &model("m", installed_size, digest),
            &archive,
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("names itself"), "{err}");
        assert!(!dir.path().join("models/onnx-runtime/m").exists());
    }

    #[test]
    fn a_dangling_weight_reference_is_refused() {
        let dir = secure_tempdir();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        let root = ModelRoot::new(dir.path().to_path_buf());
        let descriptor = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
                             "file_path":"missing/weights.onnx",
                             "params":{"model_type":"embed","target_dim":384}}"#;
        let (archive, installed_size, digest) = build_archive(dir.path(), "m", descriptor);
        let err = stage_from_archive(
            &root,
            &model("m", installed_size, digest),
            &archive,
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn an_escaping_descriptor_reference_is_refused() {
        let dir = secure_tempdir();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        let root = ModelRoot::new(dir.path().to_path_buf());
        let descriptor = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
                             "file_path":"../../etc/passwd",
                             "params":{"model_type":"embed","target_dim":384}}"#;
        let (archive, installed_size, digest) = build_archive(dir.path(), "m", descriptor);
        let err = stage_from_archive(
            &root,
            &model("m", installed_size, digest),
            &archive,
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("plain relative path"), "{err}");
    }
}

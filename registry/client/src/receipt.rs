//! `.postvec-install.json`: the per-model install receipt.
//!
//! The receipt marks CLI ownership. `model rm` never infers ownership from
//! a directory name, offline `model show --verify` needs per-file hashes
//! and dependency/removal knowledge must survive registry withdrawal and
//! air-gapped copying. It sits beside `ninference.hub.json` inside the
//! model directory, so it travels with the model when a directory is
//! copied to an isolated host.

use crate::archive::ExtractedFile;
use crate::error::{Error as CliError, Result};
use crate::fs as owned;
use crate::index::IndexModel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;

pub const RECEIPT_FILE: &str = ".postvec-install.json";
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub schema_version: u32,
    pub name: String,
    pub backend: String,
    pub model_type: String,
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The exact terms-document version and canonical URL the index carried
    /// at install time. Absent for unversioned documents and in every
    /// receipt written before versioned terms existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    /// RFC 3339 time a notice-policy document was locally acknowledged —
    /// written only when an acknowledgement actually happened, never for a
    /// none-policy document. Local evidence that this host was shown the
    /// document; not an organization acceptance and not proof of assent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_accepted_at: Option<String>,
    /// How the acknowledgement was given: `"interactive"` (a prompt was
    /// answered) or `"flag"` (`--accept-license <id>@<version>`). The CLI
    /// knows the mechanism, never the human or organization principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_acceptance_method: Option<String>,
    /// The index's free-text provenance string ("where did these bytes come
    /// from"), kept so `model show` can answer offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Host the archive came from — never the full URL, which may carry a
    /// signed query string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_host: Option<String>,
    pub archive_digest: String,
    pub archive_size: u64,
    /// The installed revision of this name. Absent in a receipt written
    /// before updatable names existed, which reads as revision 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// The identity contract, recorded at install time **from the staged
    /// descriptor** — the bytes the engine will actually consume, never the
    /// index's claims about them. An upgrade
    /// re-checks the next revision's descriptor against these, so a registry
    /// can never move an installed column's vector space.
    ///
    /// The whole block is one presence marker, deliberately: **absent means
    /// this receipt predates the block and the values are unknown**, while a
    /// present block's inner `None`s are exact — a model that declared no
    /// target space then and declares one now has changed identity. Spelling
    /// "unknown" as a field-level `None` would make those two cases
    /// indistinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ReceiptIdentity>,
    pub dependencies: Vec<String>,
    pub postvec_requires: Vec<String>,
    pub registry_schema_version: u32,
    pub installed_at: String,
    pub cli_version: String,
    pub files: Vec<ReceiptFile>,
}

/// The vector-space half of the identity contract. `model_type` and
/// `backend` are not repeated here: every receipt ever written has recorded
/// them at the top level, so they are always exactly checkable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_dim: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dim: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Local acknowledgement evidence for a notice-policy terms document:
/// when the exact document was acknowledged on this host, and through which
/// mechanism. Preserved across a same-document upgrade; replaced when the
/// exact document changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseEvidence {
    /// RFC 3339.
    pub accepted_at: String,
    /// `"interactive"` | `"flag"` — the mechanism, never a principal.
    pub method: String,
}

impl Receipt {
    /// Build a receipt from an index entry and the extraction evidence.
    /// `identity` comes from the **staged descriptor**, not from `model`.
    /// `license_evidence` is present only when a notice-policy document was
    /// actually acknowledged (this run, or preserved from the receipt being
    /// replaced); a none-policy document records no timestamp or method.
    pub fn new(
        model: &IndexModel,
        source_host: Option<String>,
        files: &[ExtractedFile],
        identity: &crate::identity::Identity,
        license_evidence: Option<&LicenseEvidence>,
    ) -> Self {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            name: model.name.clone(),
            backend: model.backend.clone(),
            model_type: model.model_type.clone(),
            access: model.access.clone(),
            license: model.license.clone(),
            license_version: model.license_version.clone(),
            license_url: model.license_url.clone(),
            license_accepted_at: license_evidence.map(|e| e.accepted_at.clone()),
            license_acceptance_method: license_evidence.map(|e| e.method.clone()),
            source: model.source.clone(),
            published_at: model.published_at.clone(),
            source_host,
            archive_digest: model.archive.digest.clone(),
            archive_size: model.archive.size,
            revision: Some(model.revision()),
            identity: Some(ReceiptIdentity {
                source_model: identity.source_model.clone(),
                target_model: identity.target_model.clone(),
                source_dim: identity.source_dim,
                target_dim: identity.target_dim,
            }),
            dependencies: model.dependencies.clone(),
            postvec_requires: model.postvec_requires.clone(),
            registry_schema_version: crate::index::SCHEMA_VERSION,
            installed_at: crate::receipt::timestamp_now(),
            cli_version: crate::VERSION.to_string(),
            files: files
                .iter()
                .map(|f| ReceiptFile {
                    path: f.path.clone(),
                    size: f.size,
                    sha256: f.sha256.clone(),
                })
                .collect(),
        }
    }

    /// The installed revision; an older receipt without the field is 1.
    pub fn revision(&self) -> u64 {
        self.revision.unwrap_or(1)
    }

    /// The identity contract as this install recorded it. `None` when
    /// the receipt predates the identity block. The caller must then fall
    /// back to [`crate::identity::check_identity_core`] and say
    /// what it could not verify, never assume agreement.
    pub fn identity(&self) -> Option<crate::identity::Identity> {
        let block = self.identity.as_ref()?;
        Some(crate::identity::Identity {
            model_type: self.model_type.clone(),
            backend: self.backend.clone(),
            source_model: block.source_model.clone(),
            target_model: block.target_model.clone(),
            source_dim: block.source_dim,
            target_dim: block.target_dim,
        })
    }

    /// Re-measure one recorded file and update its size and hash.
    ///
    /// The only legitimate caller is the persistent activate/deactivate flip,
    /// which rewrites `enabled` in the installed `ninference.hub.json`. Without
    /// this the descriptor would no longer match the receipt and every
    /// deactivated model would read as corrupt under `model show --verify` and
    /// `doctor --deep`. `archive_digest` is deliberately untouched: it names
    /// the published bytes, not the operator's power switch.
    ///
    /// Errors when `relative` is not a recorded file, so a typo cannot quietly
    /// leave the receipt describing the pre-flip descriptor.
    pub fn update_file_hash(&mut self, relative: &str, model_dir: &Path) -> Result<()> {
        let path = model_dir.join(relative);
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| CliError::apply(format!("cannot stat {}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(CliError::apply(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let sha256 = hash_file(&path)
            .map_err(|e| CliError::apply(format!("cannot hash {}: {e}", path.display())))?;
        let entry = self
            .files
            .iter_mut()
            .find(|file| file.path == relative)
            .ok_or_else(|| {
                CliError::internal(format!("{relative} is not recorded in the receipt"))
            })?;
        entry.size = meta.len();
        entry.sha256 = sha256;
        Ok(())
    }

    /// Write into `model_dir` (world-readable: it holds no secrets, and the
    /// PostgreSQL service account must be able to read the directory anyway).
    pub fn write(&self, model_dir: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| CliError::internal(format!("cannot serialize receipt: {e}")))?;
        owned::write_atomic(&model_dir.join(RECEIPT_FILE), &body, 0o644)
    }

    /// Read a receipt from `model_dir`. `Ok(None)` when absent (a package or
    /// manual install); an unreadable or malformed receipt is an error the
    /// caller reports rather than silently treating the model as manual.
    pub fn read(model_dir: &Path) -> Result<Option<Receipt>> {
        let path = model_dir.join(RECEIPT_FILE);
        let Some(content) = owned::read_regular_file(&path)? else {
            return Ok(None);
        };
        let receipt: Receipt = serde_json::from_str(&content).map_err(|e| {
            CliError::precondition(format!("{} is malformed: {e}", path.display())).with_fix(
                "the directory is not a valid CLI install; remove it manually if unwanted",
            )
        })?;
        if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err(CliError::precondition(format!(
                "{} has schema_version {}, this postvec-cli understands {RECEIPT_SCHEMA_VERSION}",
                path.display(),
                receipt.schema_version
            ))
            .with_fix("upgrade postvec-cli"));
        }
        receipt.validate_shape().map_err(|e| {
            CliError::precondition(format!("{} is malformed: {e}", path.display())).with_fix(
                "the directory is not a valid CLI install; remove it manually if unwanted",
            )
        })?;
        Ok(Some(receipt))
    }

    /// Structural validation of a loaded receipt. A receipt names
    /// filesystem locations, so hostile or corrupted content must fail
    /// here, before any caller joins its paths.
    fn validate_shape(&self) -> std::result::Result<(), String> {
        crate::index::valid_model_name(&self.name)?;
        crate::index::valid_model_name(&self.backend).map_err(|e| format!("backend: {e}"))?;
        let hex = self
            .archive_digest
            .strip_prefix("sha256:")
            .ok_or_else(|| format!("archive_digest {:?} is not sha256:", self.archive_digest))?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(format!(
                "archive_digest {:?} is not 64 lowercase hex characters",
                self.archive_digest
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for file in &self.files {
            let path = Path::new(&file.path);
            if path.is_absolute()
                || file.path.is_empty()
                || path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(format!(
                    "file path {:?} is not a plain relative path",
                    file.path
                ));
            }
            if !seen.insert(file.path.as_str()) {
                return Err(format!("file path {:?} is recorded twice", file.path));
            }
        }
        Ok(())
    }

    /// Hash every recorded file against the receipt and walk the
    /// installed tree for anything the receipt does not record. Extra
    /// regular files, symlinks, hard links and special files all make the
    /// integrity claim false and are reported. Returns the list of
    /// problems (empty = verified). This reads gigabytes for large
    /// models; callers gate it behind `--verify` / `--deep`.
    pub fn verify_files(&self, model_dir: &Path) -> Vec<String> {
        let mut problems = Vec::new();

        // The reverse direction first: everything on disk must be recorded.
        let recorded: std::collections::BTreeSet<&str> =
            self.files.iter().map(|f| f.path.as_str()).collect();
        for entry in walkdir::WalkDir::new(model_dir).into_iter().flatten() {
            let relative = match entry.path().strip_prefix(model_dir) {
                Ok(p) if !p.as_os_str().is_empty() => p.to_string_lossy().to_string(),
                _ => continue,
            };
            let file_type = entry.file_type();
            if file_type.is_symlink() {
                problems.push(format!("{relative}: unexpected symlink"));
                continue;
            }
            if file_type.is_dir() {
                continue;
            }
            if !file_type.is_file() {
                problems.push(format!("{relative}: unexpected special file"));
                continue;
            }
            if relative == RECEIPT_FILE {
                continue;
            }
            if !recorded.contains(relative.as_str()) {
                problems.push(format!("{relative}: not recorded in the receipt"));
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    problems.push(format!("{relative}: has {} hard links", meta.nlink()));
                }
            }
        }

        for file in &self.files {
            let path = model_dir.join(&file.path);
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    problems.push(format!("{}: missing", file.path));
                    continue;
                }
            };
            if !meta.is_file() {
                problems.push(format!("{}: not a regular file", file.path));
                continue;
            }
            if meta.len() != file.size {
                problems.push(format!(
                    "{}: size {} (receipt says {})",
                    file.path,
                    meta.len(),
                    file.size
                ));
                continue;
            }
            match hash_file(&path) {
                Ok(hash) if hash == file.sha256 => {}
                Ok(hash) => problems.push(format!(
                    "{}: sha256 {}… (receipt says {}…)",
                    file.path,
                    &hash[..12],
                    &file.sha256[..12.min(file.sha256.len())]
                )),
                Err(e) => problems.push(format!("{}: {e}", file.path)),
            }
        }
        problems
    }
}

fn hash_file(path: &Path) -> std::result::Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
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
}

/// RFC 3339, seconds, UTC — what receipts and evidence record.
pub fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ArchiveInfo, IndexModel};
    use std::os::unix::fs::PermissionsExt;

    fn model() -> IndexModel {
        IndexModel {
            name: "m".into(),
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
                digest: format!("sha256:{}", "cd".repeat(32)),
                size: 10,
                installed_size: 5,
                sources: vec!["https://example.invalid/x".into()],
            },
        }
    }

    fn identity() -> crate::identity::Identity {
        crate::identity::Identity {
            model_type: "embed".into(),
            backend: "onnx-runtime".into(),
            source_model: None,
            target_model: None,
            source_dim: None,
            target_dim: Some(384),
        }
    }

    fn secure_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn a_receipt_round_trips_and_verifies_its_files() {
        let dir = secure_tempdir();
        std::fs::write(dir.path().join("weights.bin"), b"hello").unwrap();
        let files = vec![ExtractedFile {
            path: "weights.bin".into(),
            size: 5,
            sha256: hex::encode(Sha256::digest(b"hello")),
        }];
        let receipt = Receipt::new(
            &model(),
            Some("example.invalid".into()),
            &files,
            &identity(),
            None,
        );
        receipt.write(dir.path()).unwrap();

        let read = Receipt::read(dir.path()).unwrap().expect("receipt present");
        assert_eq!(read.name, "m");
        assert!(read.verify_files(dir.path()).is_empty());

        // Tampering is detected.
        std::fs::write(dir.path().join("weights.bin"), b"HELLO").unwrap();
        let problems = read.verify_files(dir.path());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("sha256"), "{problems:?}");
    }

    /// Schema stays at version 1. Missing terms fields parse as an
    /// unversioned, unacknowledged document and are omitted when empty.
    #[test]
    fn a_pre_m2_receipt_parses_and_terms_fields_are_omitted_when_absent() {
        assert_eq!(RECEIPT_SCHEMA_VERSION, 1);
        let dir = secure_tempdir();

        // Serialize a receipt through JSON, strip the fields entirely, and
        // parse it back — the shape every existing installation carries.
        let receipt = Receipt::new(&model(), None, &[], &identity(), None);
        let mut value = serde_json::to_value(&receipt).unwrap();
        let body = serde_json::to_string(&value).unwrap();
        for absent in [
            "license_version",
            "license_url",
            "license_accepted_at",
            "license_acceptance_method",
        ] {
            assert!(!body.contains(absent), "{absent} must be omitted");
            value.as_object_mut().unwrap().remove(absent);
        }
        crate::fs::write_atomic(
            &dir.path().join(RECEIPT_FILE),
            &serde_json::to_vec(&value).unwrap(),
            0o644,
        )
        .unwrap();
        let read = Receipt::read(dir.path()).unwrap().expect("parses");
        assert!(read.license_version.is_none());
        assert!(read.license_accepted_at.is_none());
    }

    /// A notice document actually acknowledged records the exact document
    /// and the evidence; the URL recorded is the canonical document URL,
    /// never an archive source.
    #[test]
    fn acknowledged_terms_evidence_is_recorded_exactly() {
        let dir = secure_tempdir();
        let mut noticed = model();
        noticed.license_version = Some("2026-08-09".into());
        noticed.license_url = Some("https://univec.ai/legal/models/mit/2026-08-09".into());
        noticed.license_acceptance = Some("notice".into());
        noticed.archive.sources =
            vec!["https://bucket.s3/archives/x?X-Amz-Signature=secret".into()];
        let evidence = LicenseEvidence {
            accepted_at: "2026-08-10T12:00:00Z".into(),
            method: "flag".into(),
        };
        let receipt = Receipt::new(&noticed, None, &[], &identity(), Some(&evidence));
        receipt.write(dir.path()).unwrap();
        let read = Receipt::read(dir.path()).unwrap().expect("receipt present");
        assert_eq!(read.license_version.as_deref(), Some("2026-08-09"));
        assert_eq!(
            read.license_url.as_deref(),
            Some("https://univec.ai/legal/models/mit/2026-08-09")
        );
        assert_eq!(
            read.license_accepted_at.as_deref(),
            Some("2026-08-10T12:00:00Z")
        );
        assert_eq!(read.license_acceptance_method.as_deref(), Some("flag"));
        // No presigned URL enters the receipt.
        let body = serde_json::to_string(&read).unwrap();
        assert!(!body.contains("X-Amz"), "{body}");
    }

    /// Re-measuring one file updates that file's record and nothing else —
    /// in particular not `archive_digest`, which names the published bytes
    /// rather than the operator's power switch.
    #[test]
    fn re_measuring_a_file_updates_only_that_record() {
        let dir = secure_tempdir();
        std::fs::write(dir.path().join("ninference.hub.json"), b"before").unwrap();
        std::fs::write(dir.path().join("weights.bin"), b"hello").unwrap();
        let files = vec![
            ExtractedFile {
                path: "ninference.hub.json".into(),
                size: 6,
                sha256: hex::encode(Sha256::digest(b"before")),
            },
            ExtractedFile {
                path: "weights.bin".into(),
                size: 5,
                sha256: hex::encode(Sha256::digest(b"hello")),
            },
        ];
        let mut receipt = Receipt::new(&model(), None, &files, &identity(), None);
        let digest_before = receipt.archive_digest.clone();
        let weights_before = receipt.files[1].sha256.clone();

        std::fs::write(dir.path().join("ninference.hub.json"), b"after the flip").unwrap();
        assert_eq!(
            receipt.verify_files(dir.path()).len(),
            1,
            "the fixture must be stale before the update"
        );
        receipt
            .update_file_hash("ninference.hub.json", dir.path())
            .unwrap();

        assert!(receipt.verify_files(dir.path()).is_empty());
        assert_eq!(receipt.files[0].size, 14);
        assert_eq!(receipt.files[1].sha256, weights_before);
        assert_eq!(receipt.archive_digest, digest_before);
        assert_eq!(receipt.revision(), 1);

        // A file the receipt does not record is a programming error, not a
        // silent no-op that would leave the receipt describing stale bytes.
        assert!(receipt
            .update_file_hash("not-recorded", dir.path())
            .is_err());
    }

    #[test]
    fn a_missing_receipt_reads_as_none_and_a_future_schema_is_refused() {
        let dir = secure_tempdir();
        assert!(Receipt::read(dir.path()).unwrap().is_none());

        let mut receipt = Receipt::new(&model(), None, &[], &identity(), None);
        receipt.schema_version = 99;
        let body = serde_json::to_vec(&receipt).unwrap();
        crate::fs::write_atomic(&dir.path().join(RECEIPT_FILE), &body, 0o644).unwrap();
        let err = Receipt::read(dir.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version 99"), "{err}");
    }
}

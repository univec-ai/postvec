//! Deterministic tar writing and strict, bounded tar reading.
//!
//! The framing is the `tar` crate's (cargo's own tar implementation); every
//! policy rule stays ours, applied explicitly over its entry iterator:
//!
//! - single top-level `<name>/` root, no traversal, no absolute paths;
//! - regular files and directories only — links, devices, FIFOs, sparse
//!   entries and PAX/GNU control entries are rejected by class;
//! - plain ustar headers only, and an entry whose resolved path differs from
//!   its raw header path (a long-name/PAX override the library applied) is
//!   rejected outright;
//! - archive-recorded owners and modes are never honoured: directories
//!   become 0755 and files 0644;
//! - bounded entry count, path length, and cumulative payload, which must
//!   equal the index's `installed_size` exactly.
//!
//! The trust chain: the archive digest is verified from byte zero before
//! extraction, so this reader only ever parses bytes we published. Trailing
//! garbage after the two-zero-block terminator is not detected here: it
//! cannot appear in a digest-verified archive, because the digest covers
//! the whole entity.
//!
//! The writer is byte-deterministic: sorted paths, zero mtime, uid/gid zero,
//! normalised modes, plain ustar, dirs first. Determinism is proven by the
//! round-trip test at the bottom.

use serde::Serialize;
use std::fmt;

/// Why an archive operation failed, in the two classes callers act on
/// differently.
///
/// The distinction is the CLI's exit-code boundary, kept here so the reader
/// and writer stay usable by any caller: `Precondition` is a refusal about
/// the archive's own content or shape (nothing was changed), `Apply` is an
/// I/O failure while touching the filesystem (something may have been).
/// `postvec-cli` converts these into its own error variants of the same
/// names; the publisher wraps them with `anyhow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    Precondition {
        message: String,
        remediation: Option<String>,
    },
    Apply {
        message: String,
        remediation: Option<String>,
    },
}

impl ArchiveError {
    pub fn precondition(message: impl Into<String>) -> Self {
        ArchiveError::Precondition {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn apply(message: impl Into<String>) -> Self {
        ArchiveError::Apply {
            message: message.into(),
            remediation: None,
        }
    }

    /// Attach the operator-facing fix for this failure.
    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        let fix = fix.into();
        match &mut self {
            ArchiveError::Precondition { remediation, .. }
            | ArchiveError::Apply { remediation, .. } => *remediation = Some(fix),
        }
        self
    }

    /// The message without its class, for a caller rendering its own prefix.
    pub fn message(&self) -> &str {
        match self {
            ArchiveError::Precondition { message, .. } | ArchiveError::Apply { message, .. } => {
                message
            }
        }
    }

    pub fn remediation(&self) -> Option<&str> {
        match self {
            ArchiveError::Precondition { remediation, .. }
            | ArchiveError::Apply { remediation, .. } => remediation.as_deref(),
        }
    }
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ArchiveError {}

pub type Result<T> = std::result::Result<T, ArchiveError>;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// Bounds: excessive path length, entry count, individual file size or
/// cumulative bytes.
pub const MAX_ENTRIES: usize = 65_536;
pub const MAX_PATH_BYTES: usize = 512;
pub const MAX_COMPONENT_BYTES: usize = 255;

/// One extracted regular file, with the identity evidence the receipt keeps.
#[derive(Debug, Clone, Serialize)]
pub struct ExtractedFile {
    /// Path relative to the model directory, e.g. `onnx/model.onnx`.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

fn reject(detail: &str) -> ArchiveError {
    ArchiveError::precondition(format!("archive rejected: {detail}"))
        .with_fix("the download verified but its content is not a valid model archive; report this")
}

/// Strictly extract `archive` into `dest_dir`.
///
/// The caller has already verified the archive digest; this still applies
/// the full policy above. `installed_size` must equal the cumulative
/// regular-file payload exactly. Returns the per-file evidence for the
/// receipt.
pub fn extract_strict(
    archive: &Path,
    expected_root: &str,
    installed_size: u64,
    dest_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    let file = fs::File::open(archive)
        .map_err(|e| ArchiveError::apply(format!("cannot open {}: {e}", archive.display())))?;
    let mut tar = tar::Archive::new(std::io::BufReader::new(file));

    let mut entries = 0usize;
    let mut total_payload = 0u64;
    let mut seen_paths: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<ExtractedFile> = Vec::new();

    let iterator = tar
        .entries()
        .map_err(|e| reject(&format!("cannot read archive: {e}")))?;
    for entry in iterator {
        // The library validates header checksums and framing; any of its
        // errors (checksum mismatch, truncation, malformed PAX records) is a
        // rejection, not a pass-through.
        let mut entry = entry.map_err(|e| reject(&e.to_string()))?;

        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(reject(&format!("archive exceeds {MAX_ENTRIES} entries")));
        }

        let header = entry.header();
        if header.as_ustar().is_none() {
            return Err(reject("entry is not plain ustar (POSIX magic missing)"));
        }
        use tar::EntryType;
        let is_dir = match header.entry_type() {
            EntryType::Regular => false,
            EntryType::Directory => true,
            EntryType::Symlink => return Err(reject("symlink entry")),
            EntryType::Link => return Err(reject("hard link entry")),
            EntryType::Char | EntryType::Block => return Err(reject("device entry")),
            EntryType::Fifo => return Err(reject("FIFO entry")),
            EntryType::Continuous => return Err(reject("contiguous-file entry")),
            EntryType::XHeader | EntryType::XGlobalHeader => {
                return Err(reject("PAX extension entry"))
            }
            EntryType::GNULongName | EntryType::GNULongLink => {
                return Err(reject("GNU long-name/long-link entry"))
            }
            EntryType::GNUSparse => return Err(reject("GNU sparse entry")),
            other => {
                return Err(reject(&format!(
                    "unknown entry type {:#x}",
                    other.as_byte()
                )))
            }
        };
        if header.link_name_bytes().is_some() {
            return Err(reject("entry carries a link target"));
        }

        // An entry whose resolved path differs from the raw header path means
        // the library applied a long-name/PAX override — a construct the
        // publisher never emits.
        let resolved = entry.path_bytes().into_owned();
        if resolved != header.path_bytes().into_owned() {
            return Err(reject("entry path comes from an extension record"));
        }
        let path = String::from_utf8(resolved).map_err(|_| reject("path is not UTF-8"))?;
        if path.is_empty() {
            return Err(reject("entry has an empty path"));
        }

        let rel = validate_entry_path(&path, expected_root)?;
        if !seen_paths.insert(path.clone()) {
            return Err(reject(&format!("duplicate path {path:?}")));
        }

        let size = header
            .entry_size()
            .map_err(|e| reject(&format!("entry size: {e}")))?;
        if is_dir {
            if size != 0 {
                return Err(reject(&format!(
                    "directory entry {path:?} carries payload bytes"
                )));
            }
            if !rel.as_os_str().is_empty() {
                let dir = dest_dir.join(&rel);
                fs::create_dir_all(&dir).map_err(|e| {
                    ArchiveError::apply(format!("cannot create {}: {e}", dir.display()))
                })?;
                set_mode(&dir, 0o755)?;
            }
        } else {
            if rel.as_os_str().is_empty() {
                return Err(reject("a regular file entry names the archive root"));
            }
            total_payload = total_payload
                .checked_add(size)
                .ok_or_else(|| reject("cumulative payload size overflows"))?;
            if total_payload > installed_size {
                return Err(reject(&format!(
                    "cumulative payload exceeds the advertised installed size {installed_size}"
                )));
            }
            let out_path = dest_dir.join(&rel);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    ArchiveError::apply(format!("cannot create {}: {e}", parent.display()))
                })?;
                // Implicit parents get the same normalised mode explicit
                // directory entries do.
                normalize_dirs(dest_dir, parent)?;
            }
            let sha256 = write_payload(&mut entry, &out_path, size)?;
            set_mode(&out_path, 0o644)?;
            files.push(ExtractedFile {
                path: rel.to_str().expect("validated as UTF-8").to_string(),
                size,
                sha256,
            });
        }
    }

    if total_payload != installed_size {
        return Err(reject(&format!(
            "archive payload is {total_payload} bytes; the index promised exactly {installed_size}"
        )));
    }
    if files.is_empty() {
        return Err(reject("archive contains no regular files"));
    }
    Ok(files)
}

/// Enforce the path rules and return the path relative to the model
/// directory (empty for the root directory entry itself).
fn validate_entry_path(path: &str, expected_root: &str) -> Result<PathBuf> {
    if path.len() > MAX_PATH_BYTES {
        return Err(reject(&format!("path exceeds {MAX_PATH_BYTES} bytes")));
    }
    if path.starts_with('/') {
        return Err(reject(&format!("absolute path {path:?}")));
    }
    if path.contains('\\') {
        return Err(reject(&format!("backslash in path {path:?}")));
    }
    if path.contains('\0') {
        return Err(reject("NUL in path"));
    }
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    let mut components = trimmed.split('/');
    let root = components.next().unwrap_or_default();
    if root != expected_root {
        return Err(reject(&format!(
            "path {path:?} is outside the expected root {expected_root:?}"
        )));
    }
    let mut rel = PathBuf::new();
    for component in components {
        if component.is_empty() || component == "." || component == ".." {
            return Err(reject(&format!("path {path:?} has a forbidden component")));
        }
        if component.len() > MAX_COMPONENT_BYTES {
            return Err(reject(&format!(
                "path component exceeds {MAX_COMPONENT_BYTES} bytes"
            )));
        }
        if component.bytes().any(|b| b.is_ascii_control()) {
            return Err(reject(&format!("control character in path {path:?}")));
        }
        rel.push(component);
    }
    // Belt and braces: the assembled relative path must still be plain.
    debug_assert!(rel.components().all(|c| matches!(c, Component::Normal(_))));
    Ok(rel)
}

fn write_payload(reader: &mut impl Read, out_path: &Path, size: u64) -> Result<String> {
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out_path)
        .map_err(|e| ArchiveError::apply(format!("cannot create {}: {e}", out_path.display())))?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let got = reader
            .read(&mut buf)
            .map_err(|e| ArchiveError::apply(format!("archive read failed: {e}")))?;
        if got == 0 {
            break;
        }
        written += got as u64;
        if written > size {
            return Err(reject("entry payload exceeds its declared size"));
        }
        hasher.update(&buf[..got]);
        out.write_all(&buf[..got]).map_err(|e| {
            ArchiveError::apply(format!("cannot write {}: {e}", out_path.display()))
        })?;
    }
    if written != size {
        return Err(reject("archive is truncated inside a file payload"));
    }
    out.sync_all()
        .map_err(|e| ArchiveError::apply(format!("cannot sync {}: {e}", out_path.display())))?;
    Ok(hex::encode(hasher.finalize()))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| ArchiveError::apply(format!("cannot chmod {}: {e}", path.display())))
}

/// Give every already-created ancestor of `dir` (up to but excluding `stop`)
/// the normalised directory mode.
fn normalize_dirs(stop: &Path, dir: &Path) -> Result<()> {
    let mut current = dir;
    while current != stop {
        set_mode(current, 0o755)?;
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writer (publisher side)
// ---------------------------------------------------------------------------

/// A file to include in a deterministic archive.
pub struct ArchiveInput {
    /// Path relative to the model root, forward slashes.
    pub relative_path: String,
    /// Where the bytes live on disk.
    pub source: PathBuf,
}

/// Result of writing a deterministic archive.
pub struct WrittenArchive {
    /// `sha256:<hex>` over the whole archive.
    pub digest: String,
    /// Total archive bytes (the HTTP entity size).
    pub size: u64,
    /// Sum of regular-file payload bytes.
    pub installed_size: u64,
}

/// Write a deterministic plain-ustar archive: one top-level `<root_name>/`,
/// sorted paths (directories first), zero mtime, uid/gid 0, dirs 0755, files
/// 0644, no links or special entries. Returns the digest and both sizes.
pub fn write_deterministic(
    root_name: &str,
    inputs: &[ArchiveInput],
    out: &mut (impl Write + ?Sized),
) -> Result<WrittenArchive> {
    let mut inputs: Vec<&ArchiveInput> = inputs.iter().collect();
    inputs.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    for pair in inputs.windows(2) {
        if pair[0].relative_path == pair[1].relative_path {
            return Err(ArchiveError::precondition(format!(
                "archive input lists {:?} twice",
                pair[0].relative_path
            )));
        }
    }

    // Collect the directory set: the root itself plus every ancestor of every
    // file, sorted, so the archive is identical however the inputs were found.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    dirs.insert(format!("{root_name}/"));
    for input in &inputs {
        validate_relative_input(&input.relative_path)?;
        let mut ancestor = String::new();
        let components: Vec<&str> = input.relative_path.split('/').collect();
        for component in &components[..components.len() - 1] {
            ancestor.push_str(component);
            ancestor.push('/');
            dirs.insert(format!("{root_name}/{ancestor}"));
        }
    }

    let mut counting = CountingWriter {
        inner: out,
        written: 0,
        hasher: Sha256::new(),
        error: None,
    };
    let mut installed_size = 0u64;

    let io_fail =
        |e: std::io::Error, counting: &mut CountingWriter<'_, _>| match counting.error.take() {
            Some(inner) => ArchiveError::apply(format!("archive write failed: {inner}")),
            None => ArchiveError::precondition(format!("archive build failed: {e}")),
        };

    {
        let mut builder = tar::Builder::new(&mut counting);
        for dir in &dirs {
            let header = pack_header(dir, 0, 0o755, tar::EntryType::Directory)?;
            builder
                .append(&header, std::io::empty())
                .map_err(|e| io_fail(e, builder.get_mut()))?;
        }
        for input in inputs {
            let meta = fs::symlink_metadata(&input.source).map_err(|e| {
                ArchiveError::precondition(format!("cannot stat {}: {e}", input.source.display()))
            })?;
            if !meta.is_file() {
                return Err(ArchiveError::precondition(format!(
                    "{} is not a regular file",
                    input.source.display()
                )));
            }
            let size = meta.len();
            installed_size = installed_size.checked_add(size).ok_or_else(|| {
                ArchiveError::precondition("cumulative archive payload overflows u64".to_string())
            })?;
            let path = format!("{root_name}/{}", input.relative_path);
            let header = pack_header(&path, size, 0o644, tar::EntryType::Regular)?;

            let file = fs::File::open(&input.source).map_err(|e| {
                ArchiveError::precondition(format!("cannot open {}: {e}", input.source.display()))
            })?;
            // The header promises exactly `size` bytes; a file that changes
            // size while being archived must fail the build, not corrupt the
            // archive. `ExactReader` errors on both directions.
            let reader = ExactReader {
                inner: file,
                remaining: size,
                path: input.source.clone(),
            };
            builder
                .append(&header, reader)
                .map_err(|e| io_fail(e, builder.get_mut()))?;
        }
        builder
            .finish()
            .map_err(|e| io_fail(e, builder.get_mut()))?;
    }

    let size = counting.written;
    let digest = format!("sha256:{}", hex::encode(counting.hasher.finalize()));
    Ok(WrittenArchive {
        digest,
        size,
        installed_size,
    })
}

fn validate_relative_input(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c.len() > MAX_COMPONENT_BYTES)
        || path.len() > MAX_PATH_BYTES
        || path.contains('\\')
        || path.bytes().any(|b| b.is_ascii_control())
    {
        return Err(ArchiveError::precondition(format!(
            "archive input path {path:?} is not a plain relative path"
        )));
    }
    Ok(())
}

/// A deterministic plain-ustar header. `set_path` splits long paths into the
/// ustar name/prefix pair and errors when a path does not fit — the writer
/// never falls back to GNU long-name extensions, which the reader rejects.
fn pack_header(path: &str, size: u64, mode: u32, kind: tar::EntryType) -> Result<tar::Header> {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(kind);
    header.set_path(path).map_err(|e| {
        ArchiveError::precondition(format!("path {path:?} does not fit a ustar header: {e}"))
    })?;
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(size);
    header.set_cksum();
    Ok(header)
}

/// Reads exactly `remaining` bytes from the underlying file; fewer or more is
/// an error surfaced through the io::Error the tar builder propagates.
struct ExactReader {
    inner: fs::File,
    remaining: u64,
    path: PathBuf,
}

impl Read for ExactReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let got = self.inner.read(buf)?;
        if got as u64 > self.remaining {
            return Err(std::io::Error::other(format!(
                "{} grew while being archived",
                self.path.display()
            )));
        }
        self.remaining -= got as u64;
        if got == 0 && self.remaining > 0 {
            return Err(std::io::Error::other(format!(
                "{} shrank while being archived",
                self.path.display()
            )));
        }
        Ok(got)
    }
}

struct CountingWriter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    written: u64,
    hasher: Sha256,
    /// The first underlying write error, kept so the caller's message names
    /// the real cause instead of the tar builder's wrapper.
    error: Option<String>,
}

impl<W: Write + ?Sized> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.inner.write_all(buf) {
            Ok(()) => {
                self.hasher.update(buf);
                self.written += buf.len() as u64;
                Ok(buf.len())
            }
            Err(e) => {
                if self.error.is_none() {
                    self.error = Some(e.to_string());
                }
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: u64 = 512;

    fn temp_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// Build a valid archive for model `m` with two files, returning
    /// (archive path, digest, size, installed_size, tempdir guard).
    fn sample_archive(dir: &Path) -> (PathBuf, WrittenArchive) {
        let src = dir.join("src");
        fs::create_dir_all(src.join("onnx")).unwrap();
        fs::write(src.join("ninference.hub.json"), b"{\"name\":\"m\"}").unwrap();
        fs::write(src.join("onnx/model.onnx"), vec![7u8; 1500]).unwrap();
        let inputs = vec![
            ArchiveInput {
                relative_path: "ninference.hub.json".into(),
                source: src.join("ninference.hub.json"),
            },
            ArchiveInput {
                relative_path: "onnx/model.onnx".into(),
                source: src.join("onnx/model.onnx"),
            },
        ];
        let out_path = dir.join("m.tar");
        let mut out = fs::File::create(&out_path).unwrap();
        let written = write_deterministic("m", &inputs, &mut out).unwrap();
        (out_path, written)
    }

    #[test]
    fn write_then_extract_round_trips_with_exact_sizes_and_hashes() {
        let dir = temp_root();
        let (archive, written) = sample_archive(dir.path());
        assert_eq!(written.installed_size, 12 + 1500);
        assert_eq!(
            fs::metadata(&archive).unwrap().len(),
            written.size,
            "advertised size must be the exact entity size"
        );

        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let files = extract_strict(&archive, "m", written.installed_size, &dest).unwrap();
        assert_eq!(files.len(), 2);
        let model = files.iter().find(|f| f.path == "onnx/model.onnx").unwrap();
        assert_eq!(model.size, 1500);
        let mode = fs::metadata(dest.join("onnx/model.onnx"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o644);
        let dir_mode = fs::metadata(dest.join("onnx"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(dir_mode, 0o755);
    }

    #[test]
    fn writing_twice_is_byte_identical() {
        let dir = temp_root();
        let (a, wa) = sample_archive(dir.path());
        let bytes_a = fs::read(&a).unwrap();
        fs::remove_file(&a).unwrap();
        let (b, wb) = sample_archive(dir.path());
        assert_eq!(bytes_a, fs::read(&b).unwrap());
        assert_eq!(wa.digest, wb.digest);
    }

    /// Corrupt one header field of a valid archive and expect the given
    /// rejection. `patch` gets the raw archive bytes.
    fn expect_rejection(patch: impl Fn(&mut Vec<u8>), needle: &str) {
        let dir = temp_root();
        let (archive, written) = sample_archive(dir.path());
        let mut bytes = fs::read(&archive).unwrap();
        patch(&mut bytes);
        // Recompute nothing: hostile bytes don't fix their checksums either,
        // unless the patcher did.
        fs::write(&archive, &bytes).unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let err = extract_strict(&archive, "m", written.installed_size, &dest)
            .expect_err(needle)
            .to_string();
        assert!(err.contains(needle), "wanted {needle:?} in {err:?}");
    }

    /// Patch a header field and fix up its checksum so the rejection under
    /// test is the one that fires.
    fn patch_header(bytes: &mut [u8], header_offset: usize, patch: impl Fn(&mut [u8])) {
        let header = &mut bytes[header_offset..header_offset + BLOCK as usize];
        patch(header);
        header[148..156].fill(b' ');
        let sum: u64 = header.iter().map(|b| *b as u64).sum();
        let chk = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(chk.as_bytes());
    }

    // Entry order is dirs first (sorted), then files (sorted). Offsets:
    // 0 = m/, 512 = m/onnx/, 1024 = m/ninference.hub.json.
    const FILE_HEADER: usize = 1024;

    #[test]
    fn symlink_entries_are_rejected() {
        expect_rejection(
            |b| patch_header(b, FILE_HEADER, |h| h[156] = b'2'),
            "symlink",
        );
    }

    #[test]
    fn hard_link_entries_are_rejected() {
        expect_rejection(
            |b| patch_header(b, FILE_HEADER, |h| h[156] = b'1'),
            "hard link",
        );
    }

    #[test]
    fn device_and_fifo_entries_are_rejected() {
        expect_rejection(
            |b| patch_header(b, FILE_HEADER, |h| h[156] = b'3'),
            "device",
        );
        expect_rejection(
            |b| patch_header(b, FILE_HEADER, |h| h[156] = b'4'),
            "device",
        );
        expect_rejection(|b| patch_header(b, FILE_HEADER, |h| h[156] = b'6'), "FIFO");
    }

    /// PAX/GNU control entries never pass. The exact failure differs by
    /// class — the library consumes some (their payload then breaks the
    /// installed-size equality), our policy layer rejects the rest by type —
    /// but every one of them is a rejection, never a silent extraction.
    #[test]
    fn pax_and_gnu_control_entries_are_rejected() {
        for typeflag in *b"xgLKS" {
            let dir = temp_root();
            let (archive, written) = sample_archive(dir.path());
            let mut bytes = fs::read(&archive).unwrap();
            patch_header(&mut bytes, FILE_HEADER, |h| h[156] = typeflag);
            fs::write(&archive, &bytes).unwrap();
            let dest = dir.path().join(format!("dest-{typeflag}"));
            fs::create_dir_all(&dest).unwrap();
            assert!(
                extract_strict(&archive, "m", written.installed_size, &dest).is_err(),
                "typeflag {:?} must be rejected",
                char::from(typeflag)
            );
        }
    }

    #[test]
    fn traversal_paths_are_rejected() {
        expect_rejection(
            |b| {
                patch_header(b, FILE_HEADER, |h| {
                    h[0..100].fill(0);
                    h[0..7].copy_from_slice(b"m/../x\0");
                })
            },
            "forbidden component",
        );
    }

    #[test]
    fn absolute_paths_are_rejected() {
        expect_rejection(
            |b| {
                patch_header(b, FILE_HEADER, |h| {
                    h[0..100].fill(0);
                    h[0..7].copy_from_slice(b"/etc/pw");
                })
            },
            "absolute path",
        );
    }

    #[test]
    fn a_second_top_level_root_is_rejected() {
        expect_rejection(
            |b| {
                patch_header(b, FILE_HEADER, |h| {
                    h[0..100].fill(0);
                    h[0..10].copy_from_slice(b"other/file");
                })
            },
            "outside the expected root",
        );
    }

    #[test]
    fn link_targets_are_rejected_even_on_regular_entries() {
        expect_rejection(
            |b| {
                patch_header(b, FILE_HEADER, |h| {
                    h[157..161].copy_from_slice(b"/tmp");
                })
            },
            "link target",
        );
    }

    #[test]
    fn a_corrupted_checksum_is_rejected() {
        expect_rejection(
            |b| {
                // No checksum fix-up here: the corruption *is* the test.
                b[FILE_HEADER] ^= 0xff;
            },
            "archive rejected",
        );
    }

    #[test]
    fn installed_size_must_match_exactly() {
        let dir = temp_root();
        let (archive, written) = sample_archive(dir.path());
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let err = extract_strict(&archive, "m", written.installed_size + 1, &dest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("promised exactly"), "{err}");

        let dest2 = dir.path().join("dest2");
        fs::create_dir_all(&dest2).unwrap();
        let err = extract_strict(&archive, "m", written.installed_size - 1, &dest2)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("exceeds the advertised installed size"),
            "{err}"
        );
    }

    #[test]
    fn truncated_archives_are_rejected() {
        let dir = temp_root();
        let (archive, written) = sample_archive(dir.path());
        let mut bytes = fs::read(&archive).unwrap();
        bytes.truncate(bytes.len() - (BLOCK as usize) * 3);
        fs::write(&archive, &bytes).unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        assert!(extract_strict(&archive, "m", written.installed_size, &dest).is_err());
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        // Duplicate the file header + payload blocks verbatim: same path
        // twice.
        let dir = temp_root();
        let (archive, written) = sample_archive(dir.path());
        let mut bytes = fs::read(&archive).unwrap();
        // File entry: header at 1024, 12-byte payload padded to one block.
        let entry = bytes[FILE_HEADER..FILE_HEADER + 2 * BLOCK as usize].to_vec();
        let insert_at = FILE_HEADER + 2 * BLOCK as usize;
        bytes.splice(insert_at..insert_at, entry);
        fs::write(&archive, &bytes).unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let err = extract_strict(&archive, "m", written.installed_size + 12, &dest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate path"), "{err}");
    }
}

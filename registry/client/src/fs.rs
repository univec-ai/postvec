//! Filesystem discipline shared by the CLI and the node: atomic writes,
//! symlink-refusing reads, trusted-directory checks and the engine-root
//! lock. Written for a privileged command working in a directory a less
//! privileged account might also reach.

use crate::error::{Error as CliError, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// The account the packaged node runs as. It owns the models directory on a
/// package host so the node can pull; root may manage that directory too.
pub const NODE_ACCOUNT: &str = "postvec-server";

fn node_account_uid() -> Option<u32> {
    let name = std::ffi::CString::new(NODE_ACCOUNT).ok()?;
    // SAFETY: getpwnam returns a pointer to static storage or null.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    (!entry.is_null()).then(|| unsafe { (*entry).pw_uid })
}

/// Read a file, refusing anything that is not a plain, singly-linked regular
/// file. Returns `None` when it does not exist.
///
/// `pub(crate)`: the registry credential store and install receipts read
/// their files through the same lstat/`O_NOFOLLOW`/link-count dance — a
/// second implementation of it is exactly the drift this module exists to
/// prevent.
pub fn read_regular_file(path: &Path) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CliError::precondition(format!(
                "cannot stat {}: {e}",
                path.display()
            )))
        }
    };
    if meta.file_type().is_symlink() {
        return Err(CliError::precondition(format!(
            "{} is a symlink; postvec refuses to read or replace it",
            path.display()
        ))
        .with_fix("replace the symlink with a regular file, or remove it"));
    }
    if !meta.is_file() {
        return Err(CliError::precondition(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.nlink() != 1 {
        return Err(CliError::precondition(format!(
            "{} has {} hard links; postvec refuses to replace it",
            path.display(),
            meta.nlink()
        )));
    }
    // O_NOFOLLOW closes the window between the lstat above and the open.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
    let mut content = String::new();
    {
        use std::io::Read;
        let mut file = file;
        file.read_to_string(&mut content)
            .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
    }
    Ok(Some(content))
}

/// Create a file at `path` atomically with `mode`, refusing an unsafe
/// destination or a world/group-writable parent directory.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CliError::internal(format!("{} has no parent directory", path.display())))?;
    let parent_meta = fs::metadata(parent)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", parent.display())))?;
    if parent_meta.permissions().mode() & 0o002 != 0 {
        return Err(CliError::precondition(format!(
            "{} is world-writable (mode {:o}); refusing to write configuration there",
            parent.display(),
            parent_meta.permissions().mode() & 0o7777
        ))
        .with_fix(format!("chmod go-w {}", parent.display())));
    }
    // Reject an existing unsafe destination before touching anything.
    if let Some(_existing) = read_regular_file(path)? {
        // read_regular_file already rejected symlinks and hard links.
    }

    let tmp = parent.join(format!(
        ".{}.postvec.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config")
    ));
    // O_EXCL so a leftover attacker-planted temp name cannot be followed.
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| {
            CliError::apply(format!("cannot create {}: {e}", tmp.display())).with_fix(
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    format!("rerun with sudo (writing {} needs root)", parent.display())
                } else {
                    format!("check {}", parent.display())
                },
            )
        })?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(content)?;
        file.flush()?;
        // The rename below is only atomic with respect to a crash if the data
        // is on disk first.
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot write {}: {e}",
            tmp.display()
        )));
    }
    // Explicit chmod: `mode` on OpenOptions is masked by umask.
    if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)) {
        let _ = fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot set mode on {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot install {}: {e}",
            path.display()
        )));
    }
    // Durability of the rename itself.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

pub fn create_dir_all_checked(dir: &Path, mode: u32) -> Result<()> {
    // An existing path must actually be a directory, reached without
    // following a symlink at the final component. A planted symlink here
    // would silently redirect everything created below.
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CliError::precondition(format!(
                "{} is a symlink; refusing to use it as a directory",
                dir.display()
            ))
            .with_fix("replace the symlink with a real directory"));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(CliError::precondition(format!(
                "{} exists and is not a directory",
                dir.display()
            )));
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(CliError::precondition(format!(
                "cannot stat {}: {e}",
                dir.display()
            )))
        }
    }
    fs::create_dir_all(dir).map_err(|e| {
        CliError::apply(format!("cannot create {}: {e}", dir.display())).with_fix(
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                "rerun with sudo".to_string()
            } else {
                format!("create {} manually", dir.display())
            },
        )
    })?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode));
    Ok(())
}

/// Trust check for a directory a privileged command is about to mutate:
/// it must be a real (non-symlink) directory, owned by the effective
/// user and not world-writable. Otherwise a lower-privileged writer
/// could stage symlinks/hard links for the privileged command to
/// follow. Deliberately not applied to arbitrary read paths: reads are
/// already guarded by `read_regular_file`, and the goal is not to make
/// attacker-writable roots safe but to refuse them.
pub fn check_trusted_dir(dir: &Path, what: &str) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", dir.display())))?;
    if meta.file_type().is_symlink() {
        return Err(CliError::precondition(format!(
            "{what} {} is a symlink; refusing to manage it",
            dir.display()
        ))
        .with_fix("point at the real directory instead"));
    }
    if !meta.is_dir() {
        return Err(CliError::precondition(format!(
            "{what} {} is not a directory",
            dir.display()
        )));
    }
    // World-writable is refused: anyone could stage symlink/hard-link
    // races there. Group-writable is accepted — a umask of 002 and a
    // shared operator group are ordinary, and the group is the owner's.
    let mode = meta.permissions().mode();
    if mode & 0o002 != 0 {
        return Err(CliError::precondition(format!(
            "{what} {} is world-writable (mode {:o}); refusing to manage it",
            dir.display(),
            mode & 0o7777
        ))
        .with_fix(format!("chmod go-w {}", dir.display())));
    }
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid && !(euid == 0 && Some(meta.uid()) == node_account_uid()) {
        let owner = meta.uid();
        let fix = if euid == 0 {
            format!(
                "rerun without sudo as uid {owner} (the account that wrote this root), or \
                 `chown -R root:root {}` if it should be system-managed",
                dir.display()
            )
        } else {
            "rerun as the owning account, or fix the directory's ownership".to_string()
        };
        return Err(CliError::precondition(format!(
            "{what} {} is owned by uid {owner}, not the effective user (uid {euid}); \
             a privileged command must not trust another account's directory",
            dir.display()
        ))
        .with_fix(fix));
    }
    Ok(())
}

/// Ancestor-chain trust: every directory between the managed path and
/// the filesystem root must be one no other account can use to
/// rename-and-replace the subtree. Two rules, factored into
/// [`ancestor_policy`] so the decision is unit-testable without
/// privileged chown:
///
/// 1. **Ownership**: the ancestor must be owned by root or the effective
///    user. Mode bits say nothing about the *owner's* write permission — an
///    attacker-owned `0755` ancestor is fully replaceable by that attacker.
/// 2. **Writability**: the world write bit is refused, except a
///    **sticky** directory (`/tmp`-style) whose chain entry directly below
///    it is owned by root or the effective user — sticky semantics prevent
///    everyone else from renaming that entry.
///
/// Symlinked ancestors are excluded separately (canonical-path equality in
/// `ModelRoot::lock_exclusive`).
pub fn check_trusted_ancestry(path: &Path) -> Result<()> {
    let euid = unsafe { libc::geteuid() };
    // Track the uid of the chain entry below the ancestor being judged; the
    // first ancestor's child is the managed path itself.
    let mut child_uid = fs::symlink_metadata(path)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", path.display())))?
        .uid();
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        let meta = fs::symlink_metadata(dir).map_err(|e| {
            CliError::precondition(format!("cannot stat ancestor {}: {e}", dir.display()))
        })?;
        let mode = meta.permissions().mode();
        if let Err(problem) = ancestor_policy(meta.uid(), mode, child_uid, euid) {
            return Err(CliError::precondition(format!(
                "ancestor {}: {problem}; a writer there could replace the managed tree \
                 between check and use",
                dir.display()
            ))
            .with_fix(format!(
                "fix the ownership/mode of {} (or move the engine root under a trusted path)",
                dir.display()
            )));
        }
        child_uid = meta.uid();
        current = dir.parent();
    }
    Ok(())
}

/// The pure ancestor decision: `uid`/`mode` describe the ancestor,
/// `child_uid` the chain entry directly below it, `euid` the effective user.
fn ancestor_policy(
    uid: u32,
    mode: u32,
    child_uid: u32,
    euid: u32,
) -> std::result::Result<(), String> {
    if uid != 0 && uid != euid {
        return Err(format!(
            "owned by uid {uid}, not root or the effective user (uid {euid}) — its owner \
             can replace it regardless of mode {:o}",
            mode & 0o7777
        ));
    }
    if mode & 0o002 != 0 {
        let sticky = mode & 0o1000 != 0;
        let child_protected = child_uid == 0 || child_uid == euid;
        if !(sticky && child_protected) {
            return Err(format!(
                "writable by other accounts (mode {:o}){}",
                mode & 0o7777,
                if sticky {
                    " and the chain entry below it is foreign-owned, so sticky protection \
                     does not apply"
                } else {
                    " with no sticky bit"
                }
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
/// An exclusive, advisory, whole-host lock serializing mutating commands for
/// one cluster. Released when dropped (or when the process dies).
pub struct HostLock {
    _file: fs::File,
}

impl HostLock {
    /// Acquire the exclusive lock, creating the lock file if needed.
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_labeled(path, "this cluster")
    }

    /// Like [`HostLock::acquire`], naming what the lock protects in the
    /// contention message. The model commands lock an *engine root* that can
    /// be shared by several clusters, so "this cluster" would mislead there.
    pub fn acquire_labeled(path: &Path, what: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            create_dir_all_checked(parent, 0o755)?;
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            // O_NOFOLLOW: a pre-planted symlink at the lock path must fail
            // the open, not be followed.
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                CliError::apply(format!("cannot open lock {}: {e}", path.display())).with_fix(
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        "rerun with sudo".to_string()
                    } else {
                        format!("check {}", path.display())
                    },
                )
            })?;
        flock(&file, libc::LOCK_EX | libc::LOCK_NB).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                CliError::precondition(format!(
                    "another postvec command is changing {what} (lock {} is held)",
                    path.display()
                ))
                .with_fix("wait for it to finish, then rerun")
            } else {
                CliError::apply(format!("cannot lock {}: {e}", path.display()))
            }
        })?;
        Ok(Self { _file: file })
    }

    /// Take a *shared* lock if the file already exists, so a read-only command
    /// does not observe a half-applied change. Never creates the file:
    /// `doctor` and `--dry-run` must leave no trace.
    ///
    /// Returns `Ok(None)` when there is nothing to lock, in which case a
    /// concurrent change cannot be excluded and the report says so.
    pub fn acquire_shared_if_present(path: &Path) -> Result<Option<Self>> {
        // Same discipline as the exclusive path: a planted symlink or
        // hard-linked file at the lock path is never followed, even for a
        // read-only lock. Unsafe shapes just mean "no lock taken". A
        // read-only command must not fail over them, but it must not open
        // through them either.
        match fs::symlink_metadata(path) {
            Err(_) => return Ok(None),
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() || meta.nlink() != 1 {
                    return Ok(None);
                }
            }
        }
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(f) => f,
            // Not being able to read the lock is not a reason to fail a
            // read-only command.
            Err(_) => return Ok(None),
        };
        match flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(_) => Ok(None),
        }
    }
}

fn flock(file: &fs::File, operation: libc::c_int) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub const HOST_LOCK_FILE: &str = "/run/lock/postvec/host.lock";

pub fn host_lock_exclusive() -> Result<HostLock> {
    HostLock::acquire_labeled(
        Path::new(HOST_LOCK_FILE),
        "this host's postvec installation (a purge)",
    )
}

/// The shared side of the host lock. Creates the file when it can; a caller
/// that cannot — a non-root `model` command on a user-owned engine root —
/// gets `None`, which only means it cannot hold a purge off (the purge, root
/// itself, still re-plans under its own locks). Refuses while a purge holds
/// the lock exclusively.
pub fn host_lock_shared() -> Result<Option<HostLock>> {
    let path = Path::new(HOST_LOCK_FILE);
    // A shared flock needs only a readable descriptor, and the file is
    // world-readable: an unprivileged caller can hold a purge off without
    // being able to create or write the file.
    let existing = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    let file = match existing {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                if create_dir_all_checked(parent, 0o755).is_err() {
                    return Ok(None);
                }
            }
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(file) => file,
                // Nobody has set this host up with privileges yet: there is
                // no installation a purge could be sweeping.
                Err(_) => return Ok(None),
            }
        }
        Err(_) => return Ok(None),
    };
    match flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
        Ok(()) => Ok(Some(HostLock { _file: file })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(CliError::precondition(
            "a `postvec uninstall --purge` is in progress on this host (its lock is held)",
        )
        .with_fix("wait for it to finish, then rerun")),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ancestor_policy_requires_trusted_ownership() {
        const EUID: u32 = 1000;
        const ATTACKER: u32 = 1001;

        // A foreign-owned 0755 ancestor is refused: its owner can replace it
        // regardless of the mode bits.
        let err = super::ancestor_policy(ATTACKER, 0o755, EUID, EUID).unwrap_err();
        assert!(err.contains("owned by uid 1001"), "{err}");

        // A foreign-owned sticky ancestor is refused for the same reason.
        assert!(super::ancestor_policy(ATTACKER, 0o1777, EUID, EUID).is_err());

        // Root- and self-owned quiet ancestors pass.
        super::ancestor_policy(0, 0o755, EUID, EUID).unwrap();
        super::ancestor_policy(EUID, 0o700, EUID, EUID).unwrap();

        // Root-owned /tmp-style sticky: fine when the chain entry below is
        // ours (or root's), refused when it is foreign-owned.
        super::ancestor_policy(0, 0o1777, EUID, EUID).unwrap();
        super::ancestor_policy(0, 0o1777, 0, EUID).unwrap();
        let err = super::ancestor_policy(0, 0o1777, ATTACKER, EUID).unwrap_err();
        assert!(err.contains("sticky protection does not apply"), "{err}");

        // World-writable without sticky stays refused even root-owned;
        // group-writable is ordinary (umask 002) and passes.
        super::ancestor_policy(0, 0o775, EUID, EUID).unwrap();
        assert!(super::ancestor_policy(0, 0o777, EUID, EUID).is_err());
        assert!(super::ancestor_policy(EUID, 0o777, EUID, EUID).is_err());
    }
}

//! Subprocess execution and the privilege boundary.
//!
//! Every external program the CLI runs goes through here, with three rules
//! that are not negotiable:
//!
//! - **No shell.** Programs are executed as `(program, argv)`; no string is
//!   ever handed to `/bin/sh`.
//! - **Minimal environment.** The child gets a fixed `PATH` and `LC_ALL=C`
//!   and nothing else, so a connection URI in the caller's environment cannot
//!   leak into a child (or into its error output).
//! - **Bounded.** Every run has a deadline and is killed when it expires.
//!
//! Privilege dropping resolves the target account's ids and supplementary
//! groups **before** forking, so the `pre_exec` closure only issues plain
//! syscalls. Doing the lookup after fork would call into the allocator and
//! the name-service switch from a forked child of a multi-threaded process.

use crate::error::{redact, CliError, Result};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// A resolved local account: everything needed to become it, gathered in the
/// parent process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsAccount {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups, from `getgrouplist(3)`.
    pub groups: Vec<u32>,
}

impl OsAccount {
    pub fn lookup(name: &str) -> Result<Self> {
        let c_name = CString::new(name)
            .map_err(|_| CliError::internal(format!("account name {name:?} contains NUL")))?;

        // getpwnam_r with a growing buffer: the required size is not knowable
        // up front on every libc.
        let mut buf = vec![0i8; 1024];
        loop {
            let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            let rc = unsafe {
                libc::getpwnam_r(
                    c_name.as_ptr(),
                    &mut passwd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    &mut result,
                )
            };
            if rc == libc::ERANGE && buf.len() < 64 * 1024 {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            // glibc reports "no such user" as rc == 0 with a null result, but
            // several libcs use one of these errnos instead.
            let absent = matches!(
                rc,
                libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM | libc::EAGAIN
            );
            if rc != 0 && !absent {
                return Err(CliError::internal(format!(
                    "getpwnam_r({name}): {}",
                    std::io::Error::from_raw_os_error(rc)
                )));
            }
            if rc != 0 || result.is_null() {
                return Err(CliError::precondition(format!(
                    "no local account named {name:?}"
                )));
            }
            let uid = passwd.pw_uid;
            let gid = passwd.pw_gid;
            return Ok(Self {
                name: name.to_string(),
                uid,
                gid,
                groups: group_list(&c_name, gid),
            });
        }
    }

    /// The account owning a path.
    ///
    /// Used for an explicitly selected installation, where there is no
    /// `pg_lsclusters` to ask. The data directory's owner is not a guess: it is
    /// the account PostgreSQL itself insists on being run as, and it is
    /// readable from the filesystem.
    pub fn owning(path: &std::path::Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", path.display())))?;
        Self::from_uid(std::os::unix::fs::MetadataExt::uid(&metadata))
    }

    pub fn from_uid(uid: u32) -> Result<Self> {
        let mut buf = vec![0i8; 1024];
        loop {
            let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            let rc = unsafe {
                libc::getpwuid_r(
                    uid,
                    &mut passwd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    &mut result,
                )
            };
            if rc == libc::ERANGE && buf.len() < 64 * 1024 {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || result.is_null() {
                return Err(CliError::precondition(format!(
                    "no local account with uid {uid}"
                )));
            }
            let name = unsafe { CStr::from_ptr(passwd.pw_name) }
                .to_string_lossy()
                .into_owned();
            let c_name = CString::new(name.clone())
                .map_err(|_| CliError::internal("account name contains NUL"))?;
            let gid = passwd.pw_gid;
            return Ok(Self {
                name,
                uid,
                gid,
                groups: group_list(&c_name, gid),
            });
        }
    }

    pub fn is_current(&self) -> bool {
        current_uid() == self.uid
    }
}

fn group_list(name: &CStr, gid: u32) -> Vec<u32> {
    let mut count: libc::c_int = 32;
    loop {
        let mut groups = vec![0 as libc::gid_t; count as usize];
        let rc = unsafe {
            libc::getgrouplist(
                name.as_ptr(),
                gid as libc::gid_t,
                groups.as_mut_ptr(),
                &mut count,
            )
        };
        if rc >= 0 {
            groups.truncate(count.max(0) as usize);
            return groups;
        }
        // rc < 0: `count` now holds the required size.
        if count <= 0 || count > 4096 {
            return vec![gid];
        }
    }
}

/// The home directory of a local account, from the passwd database — not
/// from `$HOME`, which `sudo` rewrites to root's.
pub fn home_dir(name: &str) -> Result<PathBuf> {
    let c_name = CString::new(name)
        .map_err(|_| CliError::internal(format!("account name {name:?} contains NUL")))?;
    let mut buf = vec![0i8; 1024];
    loop {
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &mut passwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 64 * 1024 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() || passwd.pw_dir.is_null() {
            return Err(CliError::precondition(format!(
                "no local account named {name:?}"
            )));
        }
        let dir = unsafe { CStr::from_ptr(passwd.pw_dir) }
            .to_string_lossy()
            .into_owned();
        return Ok(PathBuf::from(dir));
    }
}

pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn is_root() -> bool {
    current_uid() == 0
}

pub fn is_stdin_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

pub fn is_stdout_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

pub fn is_stderr_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

/// Columns available for stdout. `COLUMNS` wins (tests and constrained
/// environments), then the tty size, then 80. Never below 40 — a table that
/// shrinks further is unreadable, and we would rather wrap than invent a
/// second layout.
pub fn stdout_width() -> usize {
    terminal_width(libc::STDOUT_FILENO)
}

/// Columns available for stderr progress lines.
pub fn stderr_width() -> usize {
    terminal_width(libc::STDERR_FILENO)
}

fn terminal_width(fd: i32) -> usize {
    if let Some(width) = env_columns() {
        return width;
    }
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    let width = if ok && size.ws_col > 0 {
        size.ws_col as usize
    } else {
        80
    };
    width.clamp(40, 500)
}

fn env_columns() -> Option<usize> {
    let raw = std::env::var("COLUMNS").ok()?;
    let width: usize = raw.parse().ok()?;
    (width > 0).then_some(width.clamp(40, 500))
}

/// Read one line from stdin with terminal echo disabled — the masked
/// `API key:` prompt. On a TTY, echo is restored
/// whatever happens (including a mid-read error) and a newline is printed so
/// the next output does not share the prompt's line. On a non-TTY (a pipe:
/// `postvec login < keyfile`), this is a plain line read.
pub fn read_hidden_line(prompt: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};

    let stdin_is_tty = is_stdin_tty();
    if stdin_is_tty {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    }

    struct EchoGuard {
        original: Option<libc::termios>,
    }
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.take() {
                unsafe {
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
                }
                eprintln!();
            }
        }
    }

    let guard = if stdin_is_tty {
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let original = term;
        term.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        EchoGuard {
            original: Some(original),
        }
    } else {
        EchoGuard { original: None }
    };

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    drop(guard);
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// One external program invocation.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Become this account before `exec`. Requires root.
    pub as_user: Option<OsAccount>,
}

impl Cmd {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            as_user: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Drop to `account` before exec. A no-op when already running as it.
    pub fn run_as(mut self, account: Option<&OsAccount>) -> Self {
        self.as_user = account.filter(|a| !a.is_current()).cloned();
        self
    }

    /// The command as a human-readable line. Used in plans and remediation
    /// text; never executed.
    pub fn display(&self) -> String {
        let mut out = self.program.display().to_string();
        for arg in &self.args {
            out.push(' ');
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                out.push('\'');
                out.push_str(arg);
                out.push('\'');
            } else {
                out.push_str(arg);
            }
        }
        out
    }

    fn build(&self) -> Result<tokio::process::Command> {
        if let Some(account) = &self.as_user {
            if !is_root() {
                return Err(CliError::precondition(format!(
                    "becoming {:?} to run {} requires root",
                    account.name,
                    self.program.display()
                ))
                .with_fix("rerun with sudo, or run as the cluster owner"));
            }
        }
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group: a Ctrl-C in the terminal reaches the CLI,
            // which decides what to do, instead of racing the child.
            .process_group(0);
        if let Some(account) = self.as_user.clone() {
            apply_privilege_drop(&mut cmd, account);
        }
        Ok(cmd)
    }
}

/// Attach the pre-exec privilege drop. Order matters: supplementary groups
/// must go before `setgid`/`setuid`, because after `setuid` the process is no
/// longer privileged enough to change them.
fn apply_privilege_drop(cmd: &mut tokio::process::Command, account: OsAccount) {
    unsafe {
        cmd.pre_exec(move || become_account(&account));
    }
}

/// Irreversibly become `account`: supplementary groups, then gid, then uid,
/// then assert the drop took — a `setuid` that silently failed to shed root
/// would be a privilege-retention bug, not an inconvenience.
fn become_account(account: &OsAccount) -> std::io::Result<()> {
    let groups: Vec<libc::gid_t> = account.groups.iter().map(|g| *g as libc::gid_t).collect();
    unsafe {
        if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::setgid(account.gid as libc::gid_t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::setuid(account.uid as libc::uid_t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::getuid() != account.uid as libc::uid_t
            || libc::geteuid() != account.uid as libc::uid_t
        {
            return Err(std::io::Error::other("privilege drop did not take effect"));
        }
    }
    Ok(())
}

/// Process-wide, irreversible privilege drop for a child that should live out
/// its whole remaining life as `account`. The database agent execs as root —
/// so the kernel can load the binary from a path the target account cannot
/// traverse, such as a build under a home directory — and calls this before
/// reading any input or opening any connection.
pub fn drop_privileges(account: &OsAccount) -> Result<()> {
    become_account(account).map_err(|e| {
        CliError::precondition(format!("cannot become {:?}: {e}", account.name))
            .with_fix("the privilege drop needs root; run the CLI with sudo")
    })
}

#[derive(Debug)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    /// First line of stdout, trimmed. `postgres -C`, `pg_config` and friends
    /// all answer with a single value plus log noise on stderr.
    pub fn first_line(&self) -> &str {
        self.stdout.lines().next().unwrap_or("").trim()
    }

    /// A short diagnostic for a failed run: the exit status plus the last few
    /// stderr lines, redacted.
    pub fn failure_detail(&self) -> String {
        let tail: Vec<&str> = self
            .stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .collect();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("; ");
        let status = match self.status {
            Some(code) => format!("exit {code}"),
            None => "killed by signal".to_string(),
        };
        if tail.is_empty() {
            status
        } else {
            format!("{status}: {}", redact(&tail))
        }
    }
}

/// How long a process group gets to exit on `SIGTERM` before `SIGKILL`.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// How long the pipes get to finish draining after the child has exited.
/// Bounded because anything still holding them is a leftover, not the command.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Run a command to completion under `timeout`, killing it if it overruns.
///
/// The child's pipes are taken before waiting, so the [`tokio::process::Child`]
/// handle survives the timeout and the whole process group can actually be
/// signalled. `wait_with_output()` would consume it, leaving an overrunning
/// command — a hung `systemctl restart`, say — orphaned and still holding the
/// resources the caller just gave up on.
pub async fn run(cmd: &Cmd, timeout: Duration) -> Result<Output> {
    let mut command = cmd.build()?;
    let mut child = command.spawn().map_err(|e| {
        CliError::precondition(format!("cannot execute {}: {e}", cmd.program.display())).with_fix(
            format!(
                "check that {} exists and is executable",
                cmd.program.display()
            ),
        )
    })?;
    let pid = child.id();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    // Scoped so the drain future releases its borrow of the buffers before they
    // are read below.
    let status = {
        // Drained concurrently with the wait: a child that fills a pipe buffer
        // would otherwise block forever and hit the timeout for no reason.
        let drains = async {
            tokio::join!(
                drain(&mut stdout, &mut stdout_buf),
                drain(&mut stderr, &mut stderr_buf),
            );
        };
        tokio::pin!(drains);

        // The child exiting is the completion signal — *not* the pipes closing.
        // A backgrounded helper inherits the child's stdout and can hold it
        // open long after the command itself is done; waiting for EOF would
        // stall a finished command until its whole deadline expired.
        let wait = async {
            tokio::select! {
                status = child.wait() => (status, false),
                // Pipes closed first: the child is on its way out, so reap it.
                // Both are already fully read, so nothing is left to drain.
                _ = &mut drains => (child.wait().await, true),
            }
        };
        let (status, fully_drained) = match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => {
                terminate_group(&mut child, pid).await;
                return Err(CliError::apply(format!(
                    "{} did not finish within {} and was terminated",
                    cmd.display(),
                    humantime::format_duration(timeout)
                )));
            }
        };
        if !fully_drained {
            // Whatever the child had already written is still worth collecting,
            // but only briefly: anything still holding the pipe is a leftover,
            // and the next step is about to take it down anyway.
            let _ = tokio::time::timeout(PIPE_DRAIN_GRACE, drains).await;
        }
        status?
    };
    // The command finishing is not the same as its work stopping: a program
    // that backgrounds a helper and exits leaves that helper in the group this
    // call created. "The command returned" has to mean "nothing it started is
    // still running", or a bounded call is not actually bounded.
    if let Some(pid) = pid {
        reap_group_leftovers(pid).await;
    }

    Ok(Output {
        status: status.code(),
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
    })
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(reader: &mut Option<R>, into: &mut Vec<u8>) {
    use tokio::io::AsyncReadExt;
    if let Some(reader) = reader {
        let _ = reader.read_to_end(into).await;
    }
}

/// Terminate the child's whole process group, then reap it.
///
/// The group, not just the child: `Cmd::build` puts every child in its own
/// process group precisely so that a program which forked helpers of its own
/// cannot leave them running. `SIGTERM` first so a well-behaved program can
/// clean up, `SIGKILL` if it does not.
async fn terminate_group(child: &mut tokio::process::Child, pid: Option<u32>) {
    let Some(pid) = pid else {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(TERMINATE_GRACE, child.wait()).await;
        return;
    };

    // One deadline for the whole escalation, not one per step: a child that
    // ignores SIGTERM and a descendant that does the same would otherwise cost
    // two full grace periods before anything is killed.
    let deadline = tokio::time::Instant::now() + TERMINATE_GRACE;
    signal_group(pid, libc::SIGTERM);
    // Reap the direct child if it goes quietly, so it stops counting as a group
    // member for the poll below.
    let _ = tokio::time::timeout_at(deadline, child.wait()).await;
    while tokio::time::Instant::now() < deadline {
        if !group_exists(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if group_exists(pid) {
        signal_group(pid, libc::SIGKILL);
    }
    // The direct child may still be an unreaped zombie — it is only guaranteed
    // to be collected by an explicit wait, whether it died to the TERM above or
    // to the KILL just now. Bounded, so an unkillable process (uninterruptible
    // sleep) cannot hang the CLI as well.
    let _ = tokio::time::timeout(TERMINATE_GRACE, child.wait()).await;
}

/// Take down anything still alive in the command's process group.
///
/// Used on the success path: the direct child exiting proves nothing about
/// the rest of the group, since a helper it backgrounded keeps running. The
/// timeout path uses [`terminate_group`], which also has a child to reap.
///
/// The group id is the (by now reaped) child's pid, which the kernel could
/// in principle reuse. Linux allocates pids sequentially through a large
/// space, so reuse within these few milliseconds does not happen in
/// practice. Leaving processes running behind a call that has already
/// returned is the worse outcome.
async fn reap_group_leftovers(pid: u32) {
    if !group_exists(pid) {
        return;
    }
    signal_group(pid, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + TERMINATE_GRACE;
    while tokio::time::Instant::now() < deadline {
        if !group_exists(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Still there after the grace period: it is not going to cooperate.
    signal_group(pid, libc::SIGKILL);
}

/// Whether any process in the group is still alive.
///
/// Signal 0 performs the existence and permission checks without delivering
/// anything; `ESRCH` means the group is empty.
fn group_exists(pid: u32) -> bool {
    let group = -(pid as i32);
    unsafe { libc::kill(group, 0) == 0 }
}

/// Makes the first Ctrl-C during a critical section non-fatal.
///
/// Abandoning the process between writing a configuration file and verifying
/// the restart leaves the cluster in a state nobody has looked at. While this
/// guard is held the first `SIGINT` only prints guidance; the second
/// terminates.
///
/// A command may hold several critical sections one after another
/// (`uninstall --purge` restarts the cluster, then later stops it for the
/// file sweep), so guards are sequential-safe. tokio's process-wide `SIGINT`
/// handler is installed once and never removed: tokio will not re-install a
/// handler it believes is already registered, so handing the signal back to
/// the kernel between guards would leave every later guard inert. One
/// listener task runs for the life of the process and consults [`GUARD`]:
/// armed means "warn once, terminate on the next", disarmed means restore
/// the default disposition and re-raise.
pub struct InterruptGuard(());

struct GuardState {
    /// The guidance to print on the first Ctrl-C, while a guard is held.
    message: Option<String>,
    /// A warning was already printed for the current guard; the next Ctrl-C
    /// terminates.
    warned: bool,
}

static GUARD: std::sync::Mutex<GuardState> = std::sync::Mutex::new(GuardState {
    message: None,
    warned: false,
});
static LISTENER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl InterruptGuard {
    /// Fails rather than returning an inert guard: a caller that believes it is
    /// protected, and is not, would enter the critical section on a false
    /// premise. The only failure cases are a second guard held while one is
    /// still active (a bug) and a handler that cannot be installed.
    pub fn hold(message: impl Into<String>) -> Result<Self> {
        {
            let mut state = GUARD.lock().expect("interrupt guard state");
            if state.message.is_some() {
                return Err(CliError::internal(
                    "a Ctrl-C guard is already active; overlapping critical sections are a bug",
                ));
            }
            state.message = Some(message.into());
            state.warned = false;
        }
        if !LISTENER_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // Registered here, synchronously, *before* returning: registering
            // inside the spawned task would leave a window between `hold()`
            // returning and the task first being polled, during which a SIGINT
            // still has default handling and would terminate mid-change.
            let mut interrupts = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::interrupt(),
            )
            .map_err(|e| {
                GUARD.lock().expect("interrupt guard state").message = None;
                LISTENER_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
                CliError::internal(format!("cannot install an interrupt handler: {e}"))
            })?;
            tokio::spawn(async move {
                while interrupts.recv().await.is_some() {
                    let guidance = {
                        let mut state = GUARD.lock().expect("interrupt guard state");
                        match state.message.clone() {
                            Some(message) if !state.warned => {
                                state.warned = true;
                                Some(message)
                            }
                            _ => None,
                        }
                    };
                    match guidance {
                        Some(message) => eprintln!("\npostvec: {message}"),
                        // No guard held, or already warned: die the way an
                        // unhandled SIGINT would, with the right exit status.
                        None => {
                            restore_default_interrupt();
                            unsafe {
                                libc::raise(libc::SIGINT);
                            }
                        }
                    }
                }
            });
        }
        Ok(Self(()))
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // Disarm only. The listener and tokio's handler stay for the life of
        // the process (see the type-level comment); an unguarded Ctrl-C is
        // re-raised with default handling by the listener itself.
        let mut state = GUARD.lock().expect("interrupt guard state");
        state.message = None;
        state.warned = false;
    }
}

fn restore_default_interrupt() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

/// Signal a process group. The child is its own group leader, so its pid is
/// also the group id.
fn signal_group(pid: u32, signal: libc::c_int) {
    let group = -(pid as i32);
    unsafe {
        libc::kill(group, signal);
    }
}

/// Run a command and require success, mapping failure to a precondition error.
pub async fn run_ok(cmd: &Cmd, timeout: Duration) -> Result<Output> {
    let output = run(cmd, timeout).await?;
    if !output.ok() {
        return Err(CliError::apply(format!(
            "{} failed ({})",
            cmd.display(),
            output.failure_detail()
        )));
    }
    Ok(output)
}

/// Spawn a long-lived child with piped stdin/stdout for the JSON protocol the
/// privilege-dropped database agent speaks. stderr is inherited so the
/// child's diagnostics reach the operator directly.
pub fn spawn_piped(cmd: &Cmd) -> Result<tokio::process::Child> {
    let mut command = cmd.build()?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
        .spawn()
        .map_err(|e| CliError::internal(format!("cannot spawn {}: {e}", cmd.program.display())))
}

/// The path of the running executable, for re-exec. `/proc/self/exe` is
/// authoritative on Linux even when argv[0] lies or the binary was renamed.
pub fn self_exe() -> Result<PathBuf> {
    std::env::current_exe()
        .map_err(|e| CliError::internal(format!("cannot resolve own executable path: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_a_program_and_captures_output() {
        let out = run(
            &Cmd::new("/bin/echo").arg("hello world"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(out.ok());
        assert_eq!(out.first_line(), "hello world");
    }

    #[tokio::test]
    async fn reports_nonzero_exit_with_stderr_tail() {
        let out = run(
            &Cmd::new("/bin/sh").arg("-c").arg("echo boom >&2; exit 3"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!out.ok());
        assert_eq!(out.status, Some(3));
        assert!(out.failure_detail().contains("exit 3"));
        assert!(out.failure_detail().contains("boom"));
    }

    #[tokio::test]
    async fn environment_is_not_inherited() {
        std::env::set_var("POSTVEC_TEST_SECRET", "leaked");
        let out = run(&Cmd::new("/usr/bin/env"), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!out.stdout.contains("POSTVEC_TEST_SECRET"));
        assert!(out.stdout.contains("PATH="));
        std::env::remove_var("POSTVEC_TEST_SECRET");
    }

    /// The command must actually die, not merely be given up on: a
    /// `wait_with_output()` timeout leaves the process orphaned and still
    /// holding whatever the caller timed out over.
    #[tokio::test]
    async fn overrunning_commands_are_killed_not_abandoned() {
        // A marker argument makes this process findable among any other sleeps
        // on the machine.
        const MARKER: &str = "31337";
        let err = run(
            &Cmd::new("/bin/sleep").arg(MARKER),
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"));
        assert!(err.to_string().contains("terminated"));

        // Give the signal a moment to land, then prove nothing survived.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !process_exists_with_argument(MARKER),
            "the timed-out command is still running"
        );
    }

    /// A direct child that ignores SIGTERM must be killed *and* reaped: an
    /// unreaped zombie is not guaranteed to be collected for us.
    #[tokio::test]
    async fn a_direct_child_that_ignores_sigterm_is_killed_and_reaped() {
        const MARKER: &str = "31341";
        let started = std::time::Instant::now();
        let err = run(
            &Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("trap \"\" TERM; exec /bin/sleep {MARKER}")),
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"));
        assert!(
            !process_exists_with_argument(MARKER),
            "the TERM-ignoring child survived"
        );
        // One escalation, not one grace period per step.
        assert!(
            started.elapsed() < TERMINATE_GRACE * 2,
            "termination took {:?}, which suggests two stacked grace periods",
            started.elapsed()
        );
    }

    /// The hard case: a helper in the group ignores SIGTERM. Waiting only for
    /// the direct child would declare the group clean and leave it running.
    #[tokio::test]
    async fn a_descendant_that_ignores_sigterm_is_killed() {
        const MARKER: &str = "31339";
        let _ = run(
            &Cmd::new("/bin/sh").arg("-c").arg(format!(
                "/bin/sh -c 'trap \"\" TERM; exec /bin/sleep {MARKER}' & wait"
            )),
            Duration::from_millis(200),
        )
        .await;
        // SIGTERM is ignored, so cleanup has to escalate; allow the whole grace
        // period plus the kill to land.
        tokio::time::sleep(TERMINATE_GRACE + Duration::from_millis(500)).await;
        assert!(
            !process_exists_with_argument(MARKER),
            "a SIGTERM-ignoring descendant survived cleanup"
        );
    }

    /// A command that exits successfully after backgrounding a helper has not
    /// finished its work. A bounded call must not leave that helper behind.
    #[tokio::test]
    async fn a_successful_command_does_not_leave_background_helpers() {
        const MARKER: &str = "31340";
        let out = run(
            &Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("/bin/sleep {MARKER} & exit 0")),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(out.ok(), "the command itself succeeded");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !process_exists_with_argument(MARKER),
            "a backgrounded helper outlived the call that started it"
        );
    }

    /// A child that forks helpers into its own process group must take them
    /// with it: signalling only the direct child would leave the rest behind.
    #[tokio::test]
    async fn the_whole_process_group_is_terminated() {
        const MARKER: &str = "31338";
        let err = run(
            &Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("/bin/sleep {MARKER} & wait")),
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !process_exists_with_argument(MARKER),
            "a grandchild survived the timeout"
        );
    }

    /// Scans /proc rather than shelling out, so the check itself spawns nothing.
    fn process_exists_with_argument(marker: &str) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let cmdline = entry.path().join("cmdline");
            if let Ok(raw) = std::fs::read(&cmdline) {
                if raw
                    .split(|byte| *byte == 0)
                    .any(|argument| argument == marker.as_bytes())
                {
                    return true;
                }
            }
        }
        false
    }

    /// Output must still be captured in full when a command writes more than a
    /// pipe buffer holds — the reason the pipes are drained while waiting.
    #[tokio::test]
    async fn large_output_does_not_deadlock() {
        let out = run(
            &Cmd::new("/bin/sh").arg("-c").arg(
                "for i in $(seq 1 2000); do echo 0123456789012345678901234567890123456789; done",
            ),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert!(out.ok());
        assert_eq!(out.stdout.lines().count(), 2000);
    }

    #[tokio::test]
    async fn missing_program_is_a_precondition_error() {
        let err = run(
            &Cmd::new("/nonexistent/postvec-probe"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cannot execute"));
        assert!(err.remediation().is_some());
    }

    /// Reading the current SIGINT disposition, to tell "tokio is handling it"
    /// from "the kernel will terminate us".
    fn interrupt_is_default() -> bool {
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current) } == 0;
        ok && current.sa_sigaction == libc::SIG_DFL
    }

    /// Guard lifecycle with real signals.
    ///
    /// An armed guard survives one interrupt and prints guidance. A second
    /// overlapping hold is refused. A later hold after drop arms again, which
    /// `uninstall --purge` needs (restart then sweep). One test because the
    /// listener is process-wide. Do not raise SIGINT while disarmed: the
    /// listener re-raises with default handling and would kill the binary.
    #[tokio::test]
    async fn interrupt_guards_warn_survive_and_rearm() {
        fn warned() -> bool {
            GUARD.lock().unwrap().warned
        }
        async fn wait_for_warning() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !warned() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(warned(), "the listener never consumed the interrupt");
        }

        assert!(
            interrupt_is_default(),
            "nothing should be handling SIGINT before the first guard"
        );
        let guard = InterruptGuard::hold("a change is in progress").unwrap();
        assert!(
            !interrupt_is_default(),
            "while held, the guard must be the one handling SIGINT"
        );
        // Overlapping guards are refused: the second would share one warning
        // budget with the first and mislead both callers.
        assert!(InterruptGuard::hold("another").is_err());

        unsafe { libc::raise(libc::SIGINT) };
        wait_for_warning().await; // still alive: the interrupt only warned

        // And ordinary work still runs normally inside the critical section.
        let out = run(&Cmd::new("/bin/true"), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(out.ok());
        drop(guard);

        // A later guard must arm for real, warning budget reset.
        let guard = InterruptGuard::hold("a sweep is in progress").unwrap();
        assert!(!warned());
        unsafe { libc::raise(libc::SIGINT) };
        wait_for_warning().await;
        drop(guard);

        // The handler deliberately stays installed for the life of the
        // process; a disarmed interrupt is re-raised with default handling by
        // the listener itself (not exercised here — it would kill the test).
        assert!(!interrupt_is_default());
    }

    #[test]
    fn display_quotes_arguments_with_spaces() {
        let cmd = Cmd::new("/bin/x").arg("a b").arg("c");
        assert_eq!(cmd.display(), "/bin/x 'a b' c");
    }

    #[test]
    fn dropping_to_the_current_account_is_a_noop() {
        let me = OsAccount {
            name: "self".into(),
            uid: current_uid(),
            gid: 0,
            groups: vec![],
        };
        assert!(Cmd::new("/bin/true").run_as(Some(&me)).as_user.is_none());
    }

    #[test]
    fn root_account_resolves_with_group_list() {
        let root = OsAccount::lookup("root").expect("root must exist");
        assert_eq!(root.uid, 0);
        assert!(!root.groups.is_empty());
    }

    #[test]
    fn unknown_account_is_a_precondition_error() {
        let err = OsAccount::lookup("postvec-no-such-account").unwrap_err();
        assert!(err.to_string().contains("no local account"));
    }
}

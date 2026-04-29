//! The privilege-dropped database agent.
//!
//! Parent side ([`AgentClient`]) re-executes this binary as the cluster owner
//! and exchanges newline-delimited JSON over pipes. Child side ([`serve`])
//! decodes each request and hands it to [`super::local`].
//!
//! Why re-exec rather than fork-and-connect: `setuid` is process-wide and
//! irreversible, so the parent must keep root to write `/etc/postgresql` and
//! restart the service. Why not `psql`: passing SQL through another program's
//! argv and parsing its text output is both a quoting hazard and a redaction
//! hazard.
//!
//! The hidden `__db-agent` subcommand grants no privilege of its own — a user
//! running it directly gets exactly the database access they already had — so
//! it needs no capability token. It does refuse an interactive stdin, which is
//! the only way it could be invoked by accident.

use super::local::Direct;
use super::{DbReply, DbRequest, DbTarget};
use crate::error::{redact, CliError, Exit, Result};
use crate::proc::{self, Cmd, OsAccount};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Upper bound on one protocol line. Requests are tiny; replies are dominated
/// by registry and model lists. A cap turns a runaway reply into an error
/// instead of unbounded memory growth.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// The first message: how to connect, and how long a statement may run. Sent
/// over the pipe rather than argv so a connection URI never appears in
/// `/proc/*/cmdline`.
#[derive(Debug, Serialize, Deserialize)]
struct AgentInit {
    target: DbTarget,
    statement_timeout_ms: u64,
}

/// Parent-side handle to a running agent.
pub struct AgentClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_timeout: Duration,
}

impl AgentClient {
    /// Spawn the agent as `account`. Requires root.
    pub async fn spawn(
        account: &OsAccount,
        target: DbTarget,
        statement_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let exe = proc::self_exe()?;
        let cmd = Cmd::new(exe).arg("__db-agent").run_as(Some(account));
        let mut child = proc::spawn_piped(&cmd)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CliError::internal("database agent has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CliError::internal("database agent has no stdout"))?;
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_timeout,
        };
        let init = AgentInit {
            target,
            statement_timeout_ms: statement_timeout.as_millis().min(u64::MAX as u128) as u64,
        };
        client.send(&serde_json::to_string(&init)?).await?;
        // The agent acknowledges before the first request so a privilege-drop
        // or startup failure is reported here rather than as a confusing
        // failure of whatever query happened to come first.
        match client.read_reply().await? {
            DbReply::Ok(_) => Ok(client),
            DbReply::Err { message, .. } => Err(CliError::precondition(format!(
                "database agent (as {}) could not start: {message}",
                account.name
            ))),
        }
    }

    pub async fn execute(&mut self, request: DbRequest) -> Result<Value> {
        self.send(&serde_json::to_string(&request)?).await?;
        match self.read_reply().await? {
            DbReply::Ok(value) => Ok(value),
            DbReply::Err { message, kind } => Err(match kind.as_str() {
                "usage" => CliError::usage(message),
                "precondition" => CliError::precondition(message),
                "apply" => CliError::apply(message),
                _ => CliError::internal(message),
            }),
        }
    }

    async fn send(&mut self, line: &str) -> Result<()> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_reply(&mut self) -> Result<DbReply> {
        let mut line = String::new();
        let read = tokio::time::timeout(
            self.request_timeout,
            read_bounded_line(&mut self.stdout, &mut line),
        )
        .await
        .map_err(|_| {
            CliError::apply(format!(
                "database agent did not answer within {}",
                humantime::format_duration(self.request_timeout)
            ))
        })??;
        if read == 0 {
            return Err(CliError::internal(
                "database agent exited unexpectedly (see its output above)",
            ));
        }
        serde_json::from_str(&line).map_err(|e| {
            CliError::internal(format!(
                "database agent sent an unreadable reply: {}",
                redact(&e.to_string())
            ))
        })
    }

    pub async fn close(mut self) {
        // Closing stdin is the agent's shutdown signal; then reap it so no
        // zombie outlives the command.
        let _ = self.stdin.shutdown().await;
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

/// Child entry point: read `AgentInit`, then serve requests until stdin closes.
///
/// Synchronous signature (it builds its own runtime) because it is dispatched
/// before the parent's runtime exists.
pub fn serve() -> ExitCode {
    if proc::is_stdin_tty() {
        eprintln!(
            "postvec __db-agent is an internal subcommand driven over a pipe; \
             use `postvec setup`, `postvec uninstall` or `postvec doctor`."
        );
        return ExitCode::from(Exit::Usage.code() as u8);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("postvec agent: cannot start runtime: {e}");
            return ExitCode::from(Exit::Failure.code() as u8);
        }
    };
    match runtime.block_on(serve_loop()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("postvec agent: {e}");
            ExitCode::from(Exit::Failure.code() as u8)
        }
    }
}

async fn serve_loop() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();

    let Some(line) = read_line(&mut reader).await? else {
        // Parent went away before sending anything: nothing to do.
        return Ok(());
    };
    let init: AgentInit = serde_json::from_str(&line)
        .map_err(|e| CliError::internal(format!("agent init is unreadable: {e}")))?;
    let mut direct = Direct::new(
        init.target,
        Some(Duration::from_millis(init.statement_timeout_ms.max(100))),
    );
    write_reply(&mut stdout, &DbReply::Ok(Value::Null)).await?;

    while let Some(line) = read_line(&mut reader).await? {
        let reply = match serde_json::from_str::<DbRequest>(&line) {
            Ok(request) => {
                // Everything this privileged child changes is announced on
                // stderr (inherited from the parent), so the operator sees the
                // mutations, not just their effects.
                if request.is_mutating() {
                    eprintln!("postvec agent: {}", describe(&request));
                }
                match direct.execute(request).await {
                    Ok(value) => DbReply::Ok(value),
                    Err(e) => DbReply::Err {
                        message: redact(&e.to_string()),
                        kind: e.kind().to_string(),
                    },
                }
            }
            Err(e) => DbReply::Err {
                message: format!("unreadable request: {e}"),
                kind: "internal".to_string(),
            },
        };
        write_reply(&mut stdout, &reply).await?;
    }
    direct.close().await;
    Ok(())
}

/// A one-line description of a request, for the audit line above. Deliberately
/// names the operation and target only — never a value that could be a secret.
fn describe(request: &DbRequest) -> String {
    match request {
        DbRequest::CreateDatabase { database } => format!("CREATE DATABASE {database:?}"),
        DbRequest::InstallExtension { database, expect } => format!(
            "CREATE EXTENSION postvec in {database:?} (embedded build required: {})",
            expect.require_embedded_build
        ),
        DbRequest::UninstallExtension {
            database,
            drop_columns,
            drop_destinations,
            ..
        } => format!(
            "postvec.uninstall(drop_columns => {drop_columns}, drop_destinations => \
             {drop_destinations}) and DROP EXTENSION in {database:?}"
        ),
        DbRequest::ListDatabases => "list the cluster's databases".to_string(),
        DbRequest::RefreshModels { database } => {
            format!("postvec.refresh_models() in {database:?}")
        }
        DbRequest::ReloadConfig { database } => format!("pg_reload_conf() via {database:?}"),
        other => format!("{other:?}"),
    }
}

async fn read_line(reader: &mut BufReader<tokio::io::Stdin>) -> Result<Option<String>> {
    let mut line = String::new();
    let read = read_bounded_line(reader, &mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

/// Read one newline-terminated line, refusing to grow past
/// [`MAX_LINE_BYTES`]. A malformed peer must not be able to exhaust memory
/// while we wait for a newline that never comes.
async fn read_bounded_line<R>(reader: &mut R, out: &mut String) -> Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buffer = Vec::new();
    let read = reader.read_until(b'\n', &mut buffer).await?;
    if buffer.len() > MAX_LINE_BYTES {
        return Err(CliError::internal(
            "database agent protocol line exceeded the size cap",
        ));
    }
    out.push_str(&String::from_utf8_lossy(&buffer));
    Ok(read)
}

async fn write_reply(stdout: &mut tokio::io::Stdout, reply: &DbReply) -> Result<()> {
    // One reply per line, and never anything else on stdout: the parent's
    // parser depends on it. Diagnostics go to inherited stderr.
    let encoded = serde_json::to_string(reply)?;
    stdout.write_all(encoded.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_carries_the_target_off_the_command_line() {
        let init = AgentInit {
            target: DbTarget::Url("postgres://u:p@h/db".into()),
            statement_timeout_ms: 15_000,
        };
        let encoded = serde_json::to_string(&init).unwrap();
        let decoded: AgentInit = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.target, init.target);
        assert_eq!(decoded.statement_timeout_ms, 15_000);
    }

    #[test]
    fn replies_round_trip_with_their_error_kind() {
        let reply = DbReply::Err {
            message: "boom".into(),
            kind: "precondition".into(),
        };
        let encoded = serde_json::to_string(&reply).unwrap();
        let decoded: DbReply = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            DbReply::Err { ref kind, .. } if kind == "precondition"
        ));
    }
}

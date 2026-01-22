//! End-to-end tests of the binary's contract: argument validation, exit codes,
//! output formats, and the guarantees that must hold before anything touches a
//! cluster.
//!
//! These run the real executable, so they cover the parts unit tests cannot:
//! what a caller actually observes on stdout, stderr and the exit status.
//! Anything needing a live PostgreSQL cluster is a separate integration
//! matrix.

use std::path::PathBuf;
use std::process::{Command, Output};

fn binary() -> PathBuf {
    // The test binary lives next to the executable under test.
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("postvec")
}

fn run(args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        // A cluster on the host must not influence these tests.
        .env_remove("POSTVEC_DATABASE_URL")
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

#[test]
fn help_lists_the_public_commands() {
    let output = run(&["--help"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    for command in [
        "setup",
        "uninstall",
        "doctor",
        "model",
        "login",
        "logout",
        "whoami",
    ] {
        assert!(text.contains(command), "--help omits {command}:\n{text}");
    }
    for internal in ["__db-agent", "__preload-merge"] {
        assert!(
            !text.contains(internal),
            "the internal subcommand {internal} must stay hidden:\n{text}"
        );
    }
}

#[test]
fn model_help_lists_every_verb() {
    let output = run(&["model", "--help"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    for verb in [
        "pull",
        "upgrade",
        "ls",
        "show",
        "rm",
        "activate",
        "deactivate",
    ] {
        assert!(text.contains(verb), "model --help omits {verb}:\n{text}");
    }
}

/// The activate/deactivate argument contract, which is asymmetric on
/// purpose: a bare `activate` still means "everything eligible", while
/// `deactivate` has no `--all` at all — turning every model off in one flag
/// is how search goes down by accident.
#[test]
fn activate_takes_names_or_all_and_deactivate_requires_a_name() {
    assert_eq!(code(&run(&["model", "activate", "--help"])), 0);
    // A bare activate is valid: it keeps its original catch-up meaning. The
    // absent root below is a precondition failure (exit 1), never usage —
    // and `--path` keeps this away from whatever cluster the host has.
    assert_ne!(
        code(&run(&[
            "model",
            "activate",
            "--path",
            "/nonexistent-engine-root",
            "--dry-run"
        ])),
        2,
        "a bare `model activate` must not be a usage error"
    );
    // …but naming a model and --all at once is contradictory.
    assert_eq!(code(&run(&["model", "activate", "m", "--all"])), 2);

    // Deactivate needs at least one name and offers no --all.
    assert_eq!(code(&run(&["model", "deactivate"])), 2);
    let help = stdout(&run(&["model", "deactivate", "--help"]));
    // The prose explains why there is no --all, so look for it as a flag.
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("--all")),
        "deactivate must offer no --all flag:\n{help}"
    );
    assert_eq!(
        code(&run(&["model", "deactivate", "--all"])),
        2,
        "--all must not be accepted by deactivate"
    );
    assert!(
        help.contains("--acknowledge-in-use"),
        "deactivate must offer the in-use acknowledgement:\n{help}"
    );
}

/// `--acknowledge-in-use` exists exactly where a model can be taken away
/// from a column that still declares it, and nowhere else.
#[test]
fn the_in_use_acknowledgement_is_offered_only_where_it_applies() {
    for verb in ["deactivate", "rm"] {
        let help = stdout(&run(&["model", verb, "--help"]));
        assert!(
            help.contains("--acknowledge-in-use"),
            "model {verb} --help omits --acknowledge-in-use:\n{help}"
        );
    }
    for verb in ["pull", "upgrade", "activate", "ls", "show"] {
        let help = stdout(&run(&["model", verb, "--help"]));
        assert!(
            !help.contains("--acknowledge-in-use"),
            "model {verb} must not offer --acknowledge-in-use:\n{help}"
        );
    }
}

/// `pull` must not promise activation any more: the whole point of the split
/// is that installing files and serving them are separate decisions.
#[test]
fn pull_help_does_not_claim_to_activate() {
    let text = stdout(&run(&["model", "--help"]));
    let pull_line = text
        .lines()
        .find(|line| line.trim_start().starts_with("pull"))
        .unwrap_or_default();
    assert!(
        !pull_line.to_lowercase().contains("activate them"),
        "pull's summary still claims to activate:\n{pull_line}"
    );
    let help = stdout(&run(&["model", "pull", "--help"]));
    assert!(
        help.contains("deactivated") || help.contains("not activating"),
        "pull --help must say the models land deactivated:\n{help}"
    );
}

/// The key is never accepted as a command-line value, because argv is
/// observable by every process on the host.
#[test]
fn login_has_no_api_key_value_flag() {
    let output = run(&["login", "--api-key", "uv_secretsecret"]);
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("secretsecret"),
        "a pasted key must not be echoed:\n{}",
        stderr(&output)
    );
}

/// A hostile or typo'd model name must be rejected before any network or
/// host access (exit 2, usage).
#[test]
fn model_pull_rejects_bad_names_before_any_io() {
    for name in ["../escape", "UPPER", "a/b", "-flag2"] {
        let output = run(&["model", "pull", name, "--yes"]);
        assert_eq!(code(&output), 2, "{name}: {}", stderr(&output));
    }
}

#[test]
fn model_pull_requires_an_absolute_path() {
    let output = run(&[
        "model",
        "pull",
        "some-model",
        "--path",
        "relative/dir",
        "--yes",
    ]);
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(stderr(&output).contains("--path"), "{}", stderr(&output));
}

/// `whoami` must succeed (exit 0) with no credential anywhere, even on a host
/// with no cluster — it reports the anonymous state rather than failing.
#[test]
fn whoami_succeeds_anonymously() {
    let output = Command::new(binary())
        .args(["whoami", "--timeout", "1s"])
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_API_KEY")
        // An unreachable loopback override keeps the test off the network.
        .env(
            "POSTVEC_REGISTRY_PUBLIC_INDEX_URL",
            "http://127.0.0.1:9/index.json",
        )
        .env("XDG_CONFIG_HOME", "/nonexistent-config-home")
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec");
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Not signed in"), "{text}");
    assert!(text.contains("Searched:"), "{text}");
}

/// The container entrypoint delegates `shared_preload_libraries` merging here
/// rather than reimplementing PostgreSQL's list grammar in shell. It has to
/// work with no cluster, no database and no privileges — it is pure string
/// work — and it must preserve the operator's own list.
#[test]
fn preload_merge_applies_the_servers_list_grammar() {
    for (input, expected) in [
        ("", "postvec"),
        ("postvec", "postvec"),
        ("pg_stat_statements", "pg_stat_statements,postvec"),
        ("pg_stat_statements,postvec", "pg_stat_statements,postvec"),
        // Surrounding whitespace is ignored, exactly as the server does…
        ("  pg_stat_statements  ", "pg_stat_statements,postvec"),
        // …but case is not: these are file names, not identifiers.
        ("  PG_Stat_Statements ", "PG_Stat_Statements,postvec"),
        // Quoting is only re-emitted where it is load-bearing: `MyLib` and
        // `"MyLib"` name the same file, so the rendered form drops the quotes.
        ("\"MyLib\",pg_cron", "MyLib,pg_cron,postvec"),
        // A name that genuinely needs quoting keeps it.
        ("\"my,lib\"", "\"my,lib\",postvec"),
        // The $libdir spelling is the same library, so nothing is appended.
        ("$libdir/postvec", "$libdir/postvec"),
    ] {
        let output = run(&["__preload-merge", input]);
        assert_eq!(code(&output), 0, "merging {input:?} failed");
        assert_eq!(stdout(&output).trim(), expected, "merging {input:?}");
    }
}

/// A value PostgreSQL would refuse must be refused here too. Repairing it
/// silently is how a configuration the operator got wrong becomes one that
/// starts, with the mistake still in their file.
#[test]
fn preload_merge_refuses_what_postgresql_refuses() {
    for malformed in [
        "\"postvec",
        "\"pg_stat\"statements",
        "a,,b",
        ",postvec",
        "postvec,",
    ] {
        let output = run(&["__preload-merge", malformed]);
        assert_eq!(
            code(&output),
            2,
            "{malformed:?} should have been refused, output was {:?}",
            stdout(&output)
        );
        assert!(
            stdout(&output).trim().is_empty(),
            "{malformed:?} must not produce a repaired list"
        );
    }
}

#[test]
fn version_is_reported() {
    let output = run(&["--version"]);
    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn an_unknown_command_exits_with_the_usage_code() {
    let output = run(&["nonsense"]);
    assert_eq!(code(&output), 2);
}

#[test]
fn missing_required_arguments_exit_with_the_usage_code() {
    // Remote mode needs both endpoint kinds.
    assert_eq!(code(&run(&["setup", "--database", "d"])), 2);
    assert_eq!(
        code(&run(&["setup", "--database", "d", "--grpc", "h:1"])),
        2
    );
    // A destructive flag without its acknowledgement.
    assert_eq!(
        code(&run(&[
            "uninstall",
            "--database",
            "d",
            "--acknowledge-data-loss"
        ])),
        2
    );
}

#[test]
fn a_malformed_endpoint_is_rejected_before_any_host_access() {
    // The endpoint is invalid, so this must fail on validation rather than on
    // cluster discovery — which also means it is safe to run anywhere.
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--grpc",
        "http://192.0.2.2:33333",
        "--http",
        "https://192.0.2.2:22222",
        "--dry-run",
    ]);
    assert_eq!(code(&output), 2);
    let text = stderr(&output);
    assert!(text.contains("--grpc"), "{text}");
    assert!(text.contains("scheme"), "{text}");
}

#[test]
fn a_database_name_with_a_comma_is_rejected_with_the_reason() {
    let output = run(&[
        "setup",
        "--database",
        "a b,",
        "--grpc",
        "h:1",
        "--http",
        "http://h",
        "--dry-run",
    ]);
    assert_eq!(code(&output), 2);
    // The trailing comma splits into an empty name.
    assert!(stderr(&output).contains("empty"), "{}", stderr(&output));
}

#[test]
fn credentials_in_an_http_endpoint_are_refused_rather_than_stored() {
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--grpc",
        "192.0.2.2:33333",
        "--http",
        "https://user:s3cret@192.0.2.2:22222",
        "--dry-run",
    ]);
    assert_eq!(code(&output), 2);
    let text = stderr(&output);
    assert!(text.contains("credentials"), "{text}");
    assert!(
        !text.contains("s3cret"),
        "the secret must not be echoed back:\n{text}"
    );
}

#[test]
fn a_non_loopback_embedded_listener_is_refused() {
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--embedded",
        "--path",
        "/opt/ninference",
        "--embedded-grpc-listen",
        "192.0.2.2:33433",
        "--dry-run",
    ]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("loopback"));
}

#[test]
fn a_relative_engine_root_is_refused() {
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--embedded",
        "--path",
        "relative/ninference",
        "--dry-run",
    ]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("absolute"));
}

#[test]
fn validation_errors_are_reported_as_a_versioned_json_envelope() {
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--grpc",
        "not-an-endpoint",
        "--http",
        "https://192.0.2.2:22222",
        "--format",
        "json",
        "--dry-run",
    ]);
    assert_eq!(code(&output), 2);
    let envelope: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("a single JSON object on stdout");
    assert_eq!(envelope["schema_version"], 1);
    assert_eq!(envelope["command"], "setup");
    assert_eq!(envelope["error"]["kind"], "usage");
    assert_eq!(envelope["exit_code"], 2);
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--grpc"));
}

#[test]
fn an_unknown_cluster_is_a_usage_error_that_lists_what_exists() {
    // Safe anywhere: either the host has no pg_lsclusters (a precondition
    // error) or it has one and the requested name does not match.
    let output = run(&["doctor", "--cluster", "99/nonexistent"]);
    assert!(
        matches!(code(&output), 1 | 2),
        "expected a usage or precondition failure, got {}",
        code(&output)
    );
    let text = stderr(&output);
    assert!(
        text.contains("99/nonexistent") || text.contains("pg_lsclusters"),
        "{text}"
    );
}

#[test]
fn an_explicit_pg_config_that_does_not_exist_is_a_usage_error() {
    let output = run(&["doctor", "--pg-config", "/nonexistent/pg_config"]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("--pg-config"));
}

#[test]
fn config_dir_without_pg_config_is_refused() {
    let output = run(&["doctor", "--config-dir", "/etc/postgresql/18/main/conf.d"]);
    assert_eq!(code(&output), 2);
}

#[test]
fn the_internal_agent_refuses_an_interactive_invocation() {
    // Its stdin here is not a TTY, so it exits cleanly on an empty protocol
    // stream rather than doing anything.
    let output = Command::new(binary())
        .arg("__db-agent")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run agent");
    assert_eq!(
        code(&output),
        0,
        "with no request on stdin the agent must exit without acting: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn a_bad_timeout_value_is_a_usage_error() {
    let output = run(&["doctor", "--timeout", "not-a-duration"]);
    assert_eq!(code(&output), 2);
}

#[test]
fn dry_run_never_needs_yes() {
    // What is under test is the confirmation guard, so the assertion reads
    // the guard's own message rather than an exit code.
    //
    // `CliError::usage` — exit 2 — is raised BOTH by the guard and by cluster
    // selection ("no supported cluster is online"), so "exit code is not 2"
    // silently means "and a cluster happened to be running". On a host with
    // PostgreSQL stopped, and on any CI runner, that made this test fail for
    // a reason it was never about. The exit code cannot distinguish the two;
    // the message can.
    //
    // These two strings are the guard's contract (`plan.rs::confirm`): one
    // for an ordinary change, one for the destructive path. If either is
    // reworded, reword it here too.
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--grpc",
        "192.0.2.2:33333",
        "--http",
        "https://192.0.2.2:22222",
        "--dry-run",
    ]);
    let err = stderr(&output);
    for refusal in ["without confirmation", "needs confirmation"] {
        assert!(
            !err.contains(refusal),
            "--dry-run must never be refused for lack of --yes, but stderr contains {refusal:?}: {err}"
        );
    }
    // Whatever else happens, a dry run must not report a deferred restart or
    // a partial apply: it changed nothing, so those codes cannot apply.
    assert!(
        !matches!(code(&output), 3 | 4),
        "a dry run cannot report Partial or RestartRequired: {err}"
    );
}

#[test]
fn setup_without_yes_is_refused_before_touching_anything_when_not_interactive() {
    // Whatever the host looks like, a mutating command with no TTY and no
    // --yes must never proceed. Either it is refused for that reason (exit 2)
    // or it never got past discovery (exit 1) — it must not report success.
    let output = run(&[
        "setup",
        "--database",
        "univec",
        "--grpc",
        "192.0.2.2:33333",
        "--http",
        "https://192.0.2.2:22222",
    ]);
    assert_ne!(code(&output), 0, "{}", stderr(&output));
}

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

// ---- external providers (`postvec provider …`) --------------------------

/// A `--path` root for the provider tests.
///
/// `tempfile` honours the umask, so on a umask-002 host the root is
/// group-writable — and the loader refuses a providers.d whose *ancestor* can
/// be replaced by another account. A real root (`/var/lib/postvec-server`,
/// `/etc/postvec`) is not group-writable, so this restores the deployed
/// shape rather than relaxing the rule.
fn provider_root() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dir
}

#[test]
fn provider_help_lists_every_verb_and_the_key_rules() {
    let output = run(&["provider", "--help"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    for verb in ["add", "ls", "rm", "test"] {
        assert!(text.contains(verb), "provider --help omits {verb}:\n{text}");
    }

    let add = stdout(&run(&["provider", "add", "--help"]));
    // The three key sources, and no bare value flag: argv is world-observable.
    for flag in ["--api-key-file", "--api-key-env", "--key-stdin"] {
        assert!(
            add.contains(flag),
            "provider add --help omits {flag}:\n{add}"
        );
    }
    assert!(
        !add.lines()
            .any(|line| line.trim_start().starts_with("--api-key ")),
        "a key must never be accepted as a command-line value:\n{add}"
    );
    // The privacy gate is offered where the bridge-upgrade event applies.
    assert!(add.contains("--acknowledge-in-use"), "{add}");
    assert!(add.contains("--path"), "{add}");
}

/// A key is never accepted as a bare flag value, on any provider verb.
#[test]
fn no_provider_verb_accepts_a_key_on_the_command_line() {
    for args in [
        vec![
            "provider",
            "add",
            "openai",
            "--model",
            "m",
            "--api-key",
            "sk-x",
        ],
        vec!["provider", "test", "openai", "--api-key", "sk-x"],
    ] {
        assert_eq!(
            code(&run(&args)),
            2,
            "{args:?} must be a usage error, not an accepted key"
        );
    }
}

/// `provider add` writes a 0600 file into `<path>/providers.d`, `ls` reads
/// it back showing the key *source* and never the key, and `rm` removes it
/// — the walkthrough, entirely on the filesystem (`--path`, so no cluster
/// and no network).
#[test]
fn provider_add_ls_rm_round_trip_on_a_path_root() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let key = root.path().join("openai.key");
    std::fs::write(&key, "sk-test-key-value\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

    let root_arg = root.path().to_str().unwrap();
    let key_arg = key.to_str().unwrap();
    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-file",
        key_arg,
        "--path",
        root_arg,
        // No paid API call in a test, and no cluster is in scope.
        "--no-verify",
        // --path cannot inspect a cluster, so every name this makes live
        // gets the UNKNOWN privacy step; `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let file = root.path().join("providers.d").join("openai.toml");
    assert!(
        file.is_file(),
        "provider add wrote no file: {}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600,
        "provider files are 0600"
    );
    assert_eq!(
        std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "the providers.d directory is 0700"
    );
    let body = std::fs::read_to_string(&file).unwrap();
    assert!(
        body.contains("provider = \"openai\""),
        "the canonical connector type is written:\n{body}"
    );
    assert!(
        body.contains("api_key_file"),
        "the key file is referenced, not copied:\n{body}"
    );
    assert!(
        !body.contains("sk-test-key-value"),
        "the key value must never be copied into the provider file:\n{body}"
    );
    // The catalog filled in the descriptor.
    assert!(body.contains("openai-text-embedding-3-small"), "{body}");
    assert!(body.contains("dim = 1536"), "{body}");

    // ls reports the source, never the key.
    let listed = run(&["provider", "ls", "--path", root_arg]);
    assert_eq!(code(&listed), 0, "{}", stderr(&listed));
    let text = format!("{}{}", stdout(&listed), stderr(&listed));
    assert!(text.contains("openai-text-embedding-3-small"), "{text}");
    assert!(text.contains("file:"), "the key source is shown: {text}");
    assert!(
        !text.contains("sk-test-key-value"),
        "provider ls must never print a key: {text}"
    );

    // rm takes the file away again.
    let removed = run(&["provider", "rm", "openai", "--path", root_arg, "--yes"]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    assert!(!file.exists(), "provider rm left the file behind");
}

/// `--dry-run` sends nothing. The verification embed is a live, billed
/// request that also puts the key on the network, so the dry run must skip
/// it: this test passes on a host with no route to any provider, which it
/// could not do if the probe still ran.
#[test]
fn provider_add_dry_run_makes_no_provider_call_and_writes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let key = root.path().join("openai.key");
    std::fs::write(&key, "sk-test-key-value\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-file",
        key.to_str().unwrap(),
        "--path",
        root.path().to_str().unwrap(),
        // Deliberately NOT --no-verify: the dry run itself is what must
        // suppress the probe.
        "--dry-run",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        !root.path().join("providers.d").join("openai.toml").exists(),
        "--dry-run wrote a connector file"
    );
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("verification embed was not sent"),
        "the dry run must say the probe was skipped:\n{text}"
    );
}

/// A `--path` root that does not exist is refused. It is the only thing that
/// says who the files should belong to, and a typo would otherwise build a
/// credential directory nothing reads.
#[test]
fn provider_add_refuses_a_path_root_that_does_not_exist() {
    let root = provider_root();
    let missing = root.path().join("no-such-root");

    let output = run(&["provider", "ls", "--path", missing.to_str().unwrap()]);
    // A precondition, not a usage error: the value is well formed, the host
    // is not in the state it describes.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("--path") && text.contains("existing directory"),
        "the refusal must name --path and why:\n{text}"
    );
}

/// Re-running `add` purely to move `base_url` is a real change, not a
/// no-op. The file records the new value and the command says it wrote.
#[test]
fn provider_add_applies_a_base_url_change_to_an_existing_file() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let key = root.path().join("openai.key");
    std::fs::write(&key, "sk-test-key-value\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let root_arg = root.path().to_str().unwrap();

    let first = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-file",
        key.to_str().unwrap(),
        "--base-url",
        "https://api.openai.com",
        "--path",
        root_arg,
        "--no-verify",
        // --path cannot inspect a cluster, so every name it makes live gets
        // the UNKNOWN privacy step. `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&first), 0, "{}", stderr(&first));

    // Same model, same key, new front end. Moving the endpoint of an
    // *existing* file is a recipient change, so it needs the privacy
    // acknowledgement as well as --yes.
    let second = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--base-url",
        "https://eu.example.invalid",
        "--path",
        root_arg,
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&second), 0, "{}", stderr(&second));

    let body = std::fs::read_to_string(root.path().join("providers.d").join("openai.toml"))
        .expect("connector file");
    assert!(
        body.contains("https://eu.example.invalid"),
        "the base_url change was dropped:\n{body}"
    );
    assert!(
        body.contains("text-embedding-3-small"),
        "the existing model entry must survive:\n{body}"
    );
}

/// A world-readable key file is refused with the same rule the serving host
/// applies, before anything is written.
#[test]
fn provider_add_refuses_a_world_readable_key_file() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let key = root.path().join("leaky.key");
    std::fs::write(&key, "sk-test-key-value\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();

    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-file",
        key.to_str().unwrap(),
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0);
    let err = stderr(&output);
    assert!(err.contains("readable by other users"), "{err}");
    assert!(
        !root.path().join("providers.d").join("openai.toml").exists(),
        "nothing may be written when the key file is refused"
    );
}

/// An unknown model id with --no-verify has no dimension to write, and
/// says so instead of guessing one.
#[test]
fn an_unknown_model_needs_a_dim_when_verification_is_skipped() {
    let root = provider_root();
    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "some-future-model",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0);
    let err = stderr(&output);
    assert!(err.contains("--dim"), "{err}");
}

/// `gemini` is accepted and writes the canonical `google` connector type,
/// so the file matches the factory arm.
#[test]
fn the_gemini_alias_writes_the_google_connector_type() {
    let root = provider_root();
    let output = run(&[
        "provider",
        "add",
        "gemini",
        "--model",
        "gemini-embedding-001",
        "--api-key-env",
        "GEMINI_API_KEY",
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let body = std::fs::read_to_string(root.path().join("providers.d").join("google.toml"))
        .expect("written under the canonical name");
    assert!(body.contains("provider = \"google\""), "{body}");
    assert!(body.contains("api_key_env = \"GEMINI_API_KEY\""), "{body}");
    // The public name keeps the documented spelling.
    assert!(body.contains("name = \"gemini-embedding-001\""), "{body}");
}

/// The provider name becomes `<providers.d>/<name>.toml` in every verb, and
/// `rm` deletes that path. A traversing name — under `sudo`, against a
/// system directory — must be refused before it is joined onto anything.
#[test]
fn a_traversing_provider_name_is_refused_by_every_verb() {
    let root = provider_root();
    let outside = root.path().join("outside.toml");
    std::fs::write(&outside, "provider = 'openai'\n").expect("write");
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir(&providers_d).expect("mkdir");
    let escape = "../outside";

    for args in [
        vec![
            "provider",
            "rm",
            escape,
            "--path",
            providers_d.to_str().unwrap(),
            "--yes",
        ],
        vec![
            "provider",
            "test",
            escape,
            "--path",
            providers_d.to_str().unwrap(),
        ],
    ] {
        let output = run(&args);
        assert_ne!(code(&output), 0, "{args:?} was accepted");
        let err = stderr(&output);
        assert!(
            err.contains("outside [a-z0-9._-]") || err.contains("must start with"),
            "{args:?}: {err}"
        );
    }
    assert!(outside.exists(), "provider rm escaped the providers.d");
}

/// The Titan connector derives its endpoint from the region and never reads
/// `base_url`, so accepting the flag would write a setting the host silently
/// ignores. The region itself becomes a hostname, so it is validated too.
#[test]
fn aws_refuses_a_base_url_and_a_region_that_could_move_the_endpoint() {
    let root = provider_root();
    let base = |extra: &[&'static str]| -> Vec<&'static str> {
        let root_arg: &'static str =
            Box::leak(root.path().to_str().unwrap().to_string().into_boxed_str());
        let mut args = vec![
            "provider",
            "add",
            "aws",
            "--model",
            "amazon.titan-embed-text-v2:0",
            "--api-key-env",
            "AWS_BEARER_TOKEN_BEDROCK",
            "--path",
            root_arg,
            "--no-verify",
            "--acknowledge-in-use",
            "--yes",
        ];
        args.extend_from_slice(extra);
        args
    };

    let output = run(&base(&["--region", "us-east-1", "--base-url", "http://x"]));
    assert_ne!(code(&output), 0);
    assert!(
        stderr(&output).contains("--base-url"),
        "{}",
        stderr(&output)
    );

    let output = run(&base(&["--region", "us-east-1.evil.example"]));
    assert_ne!(code(&output), 0);
    assert!(stderr(&output).contains("region"), "{}", stderr(&output));

    // The good pair still works.
    let output = run(&base(&["--region", "us-east-1"]));
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let body =
        std::fs::read_to_string(root.path().join("providers.d").join("aws.toml")).expect("written");
    assert!(body.contains("region = \"us-east-1\""), "{body}");
    assert!(!body.contains("base_url"), "{body}");
}

/// `enabled = false` is a state an operator chose. `ls` says so instead of
/// telling them to reload a host that is behaving correctly.
#[test]
fn provider_ls_reports_a_disabled_file_as_disabled() {
    use std::os::unix::fs::PermissionsExt;
    let root = provider_root();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir(&providers_d).expect("mkdir");
    let path = providers_d.join("openai.toml");
    std::fs::write(
        &path,
        "provider = \"openai\"\nenabled = false\napi_key_env = \"OPENAI_API_KEY\"\n\n\
         [[models]]\nname = \"openai-text-embedding-3-small\"\n\
         provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
    )
    .expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

    let output = run(&["provider", "ls", "--path", root.path().to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("enabled = false"), "{text}");
    assert!(
        !text.contains("NOT served"),
        "a parked file must not read as a stale reload: {text}"
    );
}

/// A model id the host's name charset does not accept is *reduced*, not
/// written through. A public name the loader refuses fails the whole
/// connector file at the next reload, taking that provider's working models
/// down with it — so the derivation has to be total.
#[test]
fn an_awkward_model_id_still_derives_a_name_the_host_accepts() {
    let root = provider_root();
    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "Some/Future Model@v3",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--dim",
        "8",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let body = std::fs::read_to_string(root.path().join("providers.d").join("openai.toml"))
        .expect("written");
    assert!(
        body.contains("name = \"openai-some-future-model-v3\""),
        "{body}"
    );
    // The provider id itself is preserved verbatim: it is what the API expects.
    assert!(
        body.contains("provider_model_id = \"Some/Future Model@v3\""),
        "{body}"
    );
}

/// `provider add` composes a file the serving host then has to accept as a
/// **whole** — one bad entry takes that provider's already-working models
/// down at the next reload. So the rendered document is run through the
/// loader's own rules before it lands, and a refusal leaves the host exactly
/// as it was.
#[test]
fn provider_add_will_not_write_a_file_the_host_would_refuse() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let file = root.path().join("providers.d").join("openai.toml");

    // An implausible dimension. Nothing else catches it: --no-verify skips
    // the probe, and the loader refuses `dim = 0`.
    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "some-future-model",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--dim",
        "0",
        "--path",
        root_arg,
        "--no-verify",
        // --path cannot inspect a cluster, so every name it makes live gets
        // the UNKNOWN privacy step. `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    assert!(!file.exists(), "a refused file must not be written");

    // A base_url reqwest cannot parse would fail at request time as a
    // *transport* error, which the taxonomy classifies transient — so the
    // rows would be retried to death over a permanent typo.
    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--base-url",
        "api.openai.com",
        "--path",
        root_arg,
        "--no-verify",
        // --path cannot inspect a cluster, so every name it makes live gets
        // the UNKNOWN privacy step. `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0);
    assert!(stderr(&output).contains("base_url"), "{}", stderr(&output));
    assert!(!file.exists());

    // And an unsupported connector type, which the factory would only
    // refuse at gateway build, in the host's log.
    let output = run(&[
        "provider",
        "add",
        "opanai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--path",
        root_arg,
        "--no-verify",
        // --path cannot inspect a cluster, so every name it makes live gets
        // the UNKNOWN privacy step. `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0);
    assert!(
        stderr(&output).contains("unknown provider type"),
        "{}",
        stderr(&output)
    );
}

/// A hand-edited file the host refuses reads as a *refusal* in `ls`, not as
/// "NOT served (reload or restart the host)" — the one remedy that cannot
/// work. `doctor` stopped making that misdiagnosis when it started running
/// the loader's rules; `ls` answers the same question.
#[test]
fn provider_ls_names_a_refused_file_instead_of_blaming_a_reload() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let dir = root.path().join("providers.d");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file = dir.join("openai.toml");
    // A transposed credential field: valid TOML, refused by the host's
    // `deny_unknown_fields`, and invisible to a permissive read.
    std::fs::write(
        &file,
        "provider = \"openai\"\napi_kee = \"sk-x\"\n\n[[models]]\n\
         name = \"openai-text-embedding-3-small\"\n\
         provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
    )
    .expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");

    let listed = run(&["provider", "ls", "--path", root.path().to_str().unwrap()]);
    assert_eq!(code(&listed), 0, "{}", stderr(&listed));
    let text = format!("{}{}", stdout(&listed), stderr(&listed));
    assert!(text.contains("REFUSES"), "{text}");
    assert!(text.contains("api_kee"), "{text}");
    assert!(
        !text.contains("reload or restart the host"),
        "a file the host will never load must not be reported as a stale reload:\n{text}"
    );
}

/// This command runs under `sudo`. A providers.d another account can write
/// to lets that account choose what a root-run `provider add` creates — and,
/// with a planted `.NAME.postvec.tmp` symlink, what it truncates. The
/// serving host refuses such a directory outright; the CLI refuses to write
/// into one, and names the fix rather than repairing it silently.
#[test]
fn provider_add_refuses_a_world_writable_providers_directory() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let dir = root.path().join("providers.d");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");

    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let err = stderr(&output);
    assert!(err.contains("writable by other users"), "{err}");
    assert!(
        !dir.join("openai.toml").exists(),
        "nothing may be written into an unsafe directory"
    );
}

/// The temporary file a privileged writer creates must be *created*, not
/// opened: a pre-planted symlink at the temp path would otherwise be followed
/// and truncated. `create_new` + `O_NOFOLLOW` makes that a refusal even if
/// the directory check above were somehow passed.
#[test]
fn provider_add_will_not_follow_a_planted_temporary_symlink() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let dir = root.path().join("providers.d");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    // The file an attacker wants truncated.
    let victim = root.path().join("precious");
    std::fs::write(&victim, "must survive").expect("write");
    std::os::unix::fs::symlink(&victim, dir.join(".openai.toml.postvec.tmp")).expect("symlink");

    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    // Either the symlink is refused, or it was removed as a stale temp file
    // and a fresh regular file was created — never followed.
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap_or_default(),
        "must survive",
        "a planted temp symlink was followed and truncated: {}",
        stderr(&output)
    );
}

/// A hand-edited `models = 3` is an operator mistake. It must be a refusal
/// naming the file, not a panic in a command run under `sudo`.
#[test]
fn a_malformed_models_field_is_an_error_not_a_panic() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let dir = root.path().join("providers.d");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let file = dir.join("openai.toml");
    std::fs::write(
        &file,
        "provider = \"openai\"\napi_key = \"sk\"\nmodels = 3\n",
    )
    .expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");

    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--path",
        root.path().to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0);
    let err = stderr(&output);
    assert!(!err.contains("panicked"), "{err}");
    assert!(err.contains("models"), "{err}");
}

/// Moving `base_url` or `region` on an existing file is a **recipient**
/// change: the same public names, the same bound columns, a different
/// organisation receiving their source text. The privacy gate was built from
/// newly added names only, which for an endpoint-only edit is the empty set —
/// so the one gate this feature is designed around never fired for the one
/// edit that most needs it.
///
/// With `--path` there is no cluster to scan, so the command must say so
/// rather than silently treating "no columns found" as "no columns".
#[test]
fn moving_an_endpoint_announces_that_the_recipient_changes() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();

    let first = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--base-url",
        "https://api.openai.com",
        "--path",
        root_arg,
        "--no-verify",
        // --path cannot inspect a cluster, so every name it makes live gets
        // the UNKNOWN privacy step. `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&first), 0, "{}", stderr(&first));

    // Same models, same key, different endpoint. `--yes` alone must NOT get
    // through: it answers "change the configuration", not "may this text go
    // to someone else". With --path there is no cluster to list the affected
    // columns, and an unanswerable question is not a clean answer.
    let moved_args = [
        "provider",
        "add",
        "openai",
        // Already declared, so this adds nothing — the endpoint is the change.
        "--model",
        "text-embedding-3-small",
        "--base-url",
        "https://someone-else.example",
        "--path",
        root_arg,
        "--no-verify",
        // Deliberately WITHOUT --acknowledge-in-use: that is the point.
        "--yes",
    ];
    let refused = run(&moved_args);
    assert_ne!(
        code(&refused),
        0,
        "--yes must not answer a recipient change:\n{}",
        stdout(&refused)
    );
    let text = format!("{}{}", stdout(&refused), stderr(&refused));
    assert!(
        text.contains("where source text is SENT"),
        "an endpoint move must announce the recipient change:\n{text}"
    );
    assert!(text.contains("--acknowledge-in-use"), "{text}");

    // With the acknowledgement it proceeds.
    let mut acked = moved_args.to_vec();
    acked.push("--acknowledge-in-use");
    let moved = run(&acked);
    assert_eq!(code(&moved), 0, "{}", stderr(&moved));

    // A key-source rotation to the same endpoint is NOT a recipient change,
    // and must not raise the same alarm — over-prompting is how a gate stops
    // being read.
    let rotated = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY_NEW",
        "--path",
        root_arg,
        "--no-verify",
        // --path cannot inspect a cluster, so every name it makes live gets
        // the UNKNOWN privacy step. `--yes` does not answer it.
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&rotated), 0, "{}", stderr(&rotated));
    let text = format!("{}{}", stdout(&rotated), stderr(&rotated));
    assert!(
        !text.contains("where source text is SENT"),
        "a key rotation is not a recipient change:\n{text}"
    );
}

/// `ensure_private_dir` validated only a providers.d that *already existed*,
/// so the very first `provider add` on a host created the credential
/// directory — and then a `.lock` file and a `chown` — under a parent chain
/// nobody had looked at. The deepest existing ancestor is what the new
/// directory hangs from, so it is what has to be safe.
#[test]
fn creating_a_providers_directory_checks_the_chain_it_hangs_from() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    // A world-writable parent for the not-yet-existing providers.d: anyone
    // could swap the directory out from under the command that is about to
    // create and chown it.
    let hostile = root.path().join("hostile");
    std::fs::create_dir(&hostile).expect("mkdir");
    std::fs::set_permissions(&hostile, std::fs::Permissions::from_mode(0o777)).expect("chmod");

    let output = run(&[
        "provider",
        "add",
        "openai",
        "--model",
        "text-embedding-3-small",
        "--api-key-env",
        "OPENAI_API_KEY",
        "--path",
        hostile.to_str().unwrap(),
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    assert!(
        stderr(&output).contains("writable by other users"),
        "{}",
        stderr(&output)
    );
    assert!(
        !hostile.join("providers.d").exists(),
        "nothing may be created under an unchecked chain"
    );
}

/// The probe is a **paid** call, so it must not be spent on a file or a model
/// the serving host would refuse.
///
/// Hermetic by construction: every case points `--base-url` at an in-process
/// mock and asserts it received **zero** requests. An earlier version used
/// the real endpoints and would have contacted Google and OpenAI if the gate
/// regressed — a test that can make the call it forbids is not a test of the
/// gate. The runtime is **multi-threaded and held for the whole test** so the
/// mock's accept loop actually runs; a current-thread runtime that has
/// returned from `block_on` never accepts, which would make the zero-request
/// assertion true for the wrong reason.
#[test]
fn nothing_the_host_would_refuse_is_ever_probed() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    // The OpenAI response shape, for the positive control at the end. The
    // refusal cases never reach the mock, so its body is irrelevant to them.
    let mock = runtime.block_on(providers::testing::always(
        200,
        r#"{"data":[{"embedding":[0.1,0.2],"index":0}]}"#,
    ));

    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);

    let cli = |args: &[&str]| {
        Command::new(binary())
            .args(args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env("NO_COLOR", "1")
            // Resolvable, so every one of these runs would otherwise reach
            // the mock.
            .env("POSTVEC_PROBE_GATE_KEY", "not-a-real-key")
            .output()
            .expect("run postvec")
    };
    let add = |extra: &[&str]| {
        let mut args = vec!["provider", "add"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&[
            "--api-key-env",
            "POSTVEC_PROBE_GATE_KEY",
            "--base-url",
            mock.url.as_str(),
            "--path",
            root_arg,
            // Deliberately NOT --no-verify: the probe is what must be
            // skipped, and only the gate can skip it.
            "--acknowledge-in-use",
            "--yes",
        ]);
        cli(&args)
    };

    // A per-model contract the connector cannot honour: a Cohere v3 width
    // that model never produces, and a Gemini id postvec has no contract for.
    let output = add(&["cohere", "--model", "embed-english-v3.0", "--dim", "512"]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let output = add(&["gemini", "--model", "gemini-embedding-2", "--dim", "1536"]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));

    // A structurally invalid *file*: an unknown field the serving loader
    // refuses, which no per-model rule can see.
    let file = providers_d.join("openai.toml");
    std::fs::write(
        &file,
        format!(
            "provider = \"openai\"\napi_kee = \"sk-typo\"\n\
             api_key_env = \"POSTVEC_PROBE_GATE_KEY\"\nbase_url = \"{}\"\n\n\
             [[models]]\nname = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
            mock.url
        ),
    )
    .expect("write");
    set_mode(&file, 0o600);

    let output = add(&["openai", "--model", "text-embedding-3-large"]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("api_kee"), "{text}");

    // And `provider test` on the same broken file, which had no document
    // validation at all.
    let output = cli(&["provider", "test", "openai", "--path", root_arg]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));

    // The assertion the whole test exists for.
    assert_eq!(
        mock.request_count(),
        0,
        "a refused file or model must never reach the provider"
    );

    // Positive control, on the *same* mock: a legitimate run does reach it.
    // Without this, "zero requests" would also hold for a mock that cannot
    // count — which is exactly how the first version of this test passed.
    std::fs::remove_file(&file).expect("clear the broken file");
    let output = add(&["openai", "--model", "text-embedding-3-small", "--dim", "2"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(
        mock.request_count(),
        1,
        "the mock must be able to observe a probe, or zero proves nothing"
    );
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The two halves of pre-probe validation that have to hold at once: every
/// new model must be *in* the document being checked (or the structural rules
/// skip it), and a model whose dimension is not yet known must still reach
/// the probe that measures it.
///
/// A previous pass validated the document with the unknown-dimension models
/// left out, which satisfied the first half by breaking the second: a
/// brand-new file with one uncatalogued model failed with "no [[models]]
/// entries" and could never discover its dimension at all.
#[test]
fn an_uncatalogued_model_reaches_dimension_discovery_and_duplicates_do_not() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let mock = runtime.block_on(providers::testing::always(
        200,
        r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#,
    ));

    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let add = |extra: &[&str]| {
        let mut args = vec!["provider", "add", "openai"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&[
            "--api-key-env",
            "POSTVEC_DIM_DISCOVERY_KEY",
            "--base-url",
            mock.url.as_str(),
            "--path",
            root_arg,
            "--acknowledge-in-use",
            "--yes",
        ]);
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env("NO_COLOR", "1")
            .env("POSTVEC_DIM_DISCOVERY_KEY", "not-a-real-key")
            .output()
            .expect("run postvec")
    };

    // A model the built-in catalog does not know, into a file that does not
    // exist yet: the probe is the only thing that can supply its dimension.
    let output = add(&["--model", "some-uncatalogued-model"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(mock.request_count(), 1, "the probe must have run");
    let body = std::fs::read_to_string(root.path().join("providers.d/openai.toml"))
        .expect("connector file");
    assert!(body.contains("dim = 3"), "the measured dimension: {body}");
    assert!(body.contains("openai-some-uncatalogued-model"), "{body}");

    // Two ids that derive one public name. Both would be probed and only one
    // written, while both were journaled as added — so the check has to
    // happen before either call.
    let before = mock.request_count();
    let output = add(&["--model", "foo/bar", "--model", "foo--bar"]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("more than once"), "{text}");
    assert_eq!(
        mock.request_count(),
        before,
        "a name collision must be caught before anything is probed"
    );
}

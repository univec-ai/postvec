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
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
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
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
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
        "/opt/engine",
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
        "relative/engine",
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

    // `--path` cannot scan columns, so `--yes` alone must not remove a
    // route. The lost-route acknowledgement is demanded with databases
    // marked UNKNOWN, same as `add`.
    let refused = run(&["provider", "rm", "openai", "--path", root_arg, "--yes"]);
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    let text = format!("{}{}", stdout(&refused), stderr(&refused));
    assert!(text.contains("--acknowledge-in-use"), "{text}");
    assert!(text.contains("UNKNOWN"), "{text}");
    assert!(file.exists(), "nothing removed without the acknowledgement");

    let removed = run(&[
        "provider",
        "rm",
        "openai",
        "--path",
        root_arg,
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    assert!(!file.exists(), "provider rm left the file behind");
}

/// The UniVec converter round trip on a `--path` root: `--convert-source`
/// writes a `kind = "convert"` entry with both vocabularies, `ls` renders
/// the route, and `rm` takes it away with the lost-route acknowledgement.
/// Plain `--yes` suffices for the ADD: a converter's own name never binds a
/// column and no source text flows through it, so there is no privacy step
/// to acknowledge — its consent moment is postvec.migrate()'s NOTICE.
#[test]
fn provider_add_univec_converter_round_trip_on_a_path_root() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let key = root.path().join("univec.key");
    std::fs::write(&key, "uv-test-key-value\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

    let root_arg = root.path().to_str().unwrap();
    let key_arg = key.to_str().unwrap();
    let output = run(&[
        "provider",
        "add",
        "univec",
        "--convert-source",
        "openai-ada-002",
        "--convert-target",
        "gemini-text-embedding-004",
        "--source-model",
        "openai-text-embedding-ada-002",
        "--target-model",
        "gemini-embedding-001",
        "--source-dim",
        "1536",
        "--dim",
        "768",
        "--api-key-file",
        key_arg,
        "--path",
        root_arg,
        "--no-verify",
        "--yes",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let file = root.path().join("providers.d").join("univec.toml");
    let body = std::fs::read_to_string(&file).unwrap();
    assert!(body.contains("provider = \"univec\""), "{body}");
    assert!(body.contains("kind = \"convert\""), "{body}");
    assert!(
        body.contains("provider_source_id = \"openai-ada-002\""),
        "{body}"
    );
    assert!(
        body.contains("source_model = \"openai-text-embedding-ada-002\""),
        "{body}"
    );
    assert!(
        body.contains("target_model = \"gemini-embedding-001\""),
        "{body}"
    );
    assert!(body.contains("source_dim = 1536"), "{body}");
    assert!(body.contains("dim = 768"), "{body}");
    // The derived public name follows the documented spelling.
    assert!(
        body.contains(
            "name = \"univec-convert-openai-text-embedding-ada-002-to-gemini-embedding-001\""
        ),
        "{body}"
    );

    // ls renders the route in the resolver's vocabulary, not just a dim.
    let listed = run(&["provider", "ls", "--path", root_arg]);
    assert_eq!(code(&listed), 0, "{}", stderr(&listed));
    let text = format!("{}{}", stdout(&listed), stderr(&listed));
    assert!(
        text.contains("converts openai-text-embedding-ada-002[1536] -> gemini-embedding-001[768]"),
        "{text}"
    );

    // rm still demands the lost-route acknowledgement: an in-flight
    // migration may be resolved through this converter, and `--path` cannot
    // check.
    let removed = run(&[
        "provider",
        "rm",
        "univec",
        "--path",
        root_arg,
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    assert!(!file.exists(), "provider rm left the file behind");
}

/// A partial `rm` beside a surviving converter, with no host to ask: the
/// converter must compare equal to ITSELF across the removal — the document
/// reader and the loader derive one route id — or the plan would report a
/// handoff/drift for an entry the command does not touch.
#[test]
fn a_surviving_converter_is_not_its_own_handoff() {
    use std::os::unix::fs::PermissionsExt;

    let root = provider_root();
    let key = root.path().join("univec.key");
    std::fs::write(&key, "uv-test-key-value\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let root_arg = root.path().to_str().unwrap();
    let key_arg = key.to_str().unwrap();

    // One file holding an embed model and a converter.
    let added = run(&[
        "provider",
        "add",
        "univec",
        "--model",
        "snowflake-arctic-embed-l-v2.0",
        "--dim",
        "1024",
        "--api-key-file",
        key_arg,
        "--path",
        root_arg,
        "--no-verify",
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&added), 0, "{}", stderr(&added));
    let added = run(&[
        "provider",
        "add",
        "univec",
        "--convert-source",
        "openai-ada-002",
        "--convert-target",
        "gemini-text-embedding-004",
        "--source-model",
        "openai-text-embedding-ada-002",
        "--target-model",
        "gemini-embedding-001",
        "--source-dim",
        "1536",
        "--dim",
        "768",
        "--path",
        root_arg,
        "--no-verify",
        "--yes",
    ]);
    assert_eq!(code(&added), 0, "{}", stderr(&added));

    // Remove only the embed model. The surviving converter must not be
    // reported as drifted or handed off, and must survive the write.
    let removed = run(&[
        "provider",
        "rm",
        "univec",
        "--model",
        "univec-snowflake-arctic-embed-l-v2.0",
        "--path",
        root_arg,
        "--acknowledge-in-use",
        "--yes",
    ]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    let text = format!("{}{}", stdout(&removed), stderr(&removed));
    assert!(!text.contains("drift"), "{text}");
    assert!(!text.contains("handed"), "{text}");

    let body =
        std::fs::read_to_string(root.path().join("providers.d").join("univec.toml")).unwrap();
    assert!(body.contains("kind = \"convert\""), "{body}");
    assert!(
        !body.contains("univec-snowflake-arctic-embed-l-v2.0"),
        "{body}"
    );
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

/// A hand-edited file the host refuses reads as a refusal in `ls`, not
/// as "reload the host". Same question doctor asks via the loader rules.
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

/// The probe is a paid call, so it must not be spent on a file or model
/// the serving host would refuse.
///
/// Hermetic: the mock must see zero requests. Multi-threaded runtime so
/// the accept loop actually runs.
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
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
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

/// Unknown-dimension models stay in the document under validation so they
/// can still reach the probe. Structural rules still see them.
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
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
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

/// Rendered documents are size-checked the same way as files on disk.
#[test]
fn a_composed_file_over_the_byte_ceiling_is_refused_before_anything_is_spent() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let mock = runtime.block_on(providers::testing::always(
        200,
        r#"{"data":[{"embedding":[0.1,0.2],"index":0}]}"#,
    ));

    let root = provider_root();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);
    let file = providers_d.join("openai.toml");

    // Just *under* the ceiling, so the file itself loads: the overflow has to
    // come from what this run adds.
    let ceiling = providers::config::MAX_FILE_BYTES as usize;
    let head = format!(
        "provider = \"openai\"\napi_key_env = \"POSTVEC_CEILING_KEY\"\nbase_url = \"{}/\
         PADDING\"\n\n[[models]]\nname = \"openai-text-embedding-3-small\"\n\
         provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
        mock.url
    );
    let padding = "a".repeat(ceiling - head.len() - 60);
    let body = head.replace("PADDING", &padding);
    assert!(body.len() < ceiling, "the starting file must be legal");
    std::fs::write(&file, &body).expect("write");
    set_mode(&file, 0o600);

    let before = std::fs::read(&file).expect("read");
    let output = Command::new(binary())
        .args([
            "provider",
            "add",
            "openai",
            "--model",
            "text-embedding-3-large",
            "--path",
            root.path().to_str().unwrap(),
            "--acknowledge-in-use",
            "--yes",
        ])
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env("NO_COLOR", "1")
        .env("POSTVEC_CEILING_KEY", "not-a-real-key")
        .output()
        .expect("run postvec");

    assert_ne!(code(&output), 0);
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("byte ceiling"),
        "{}",
        &text[..text.len().min(400)]
    );
    assert_eq!(
        mock.request_count(),
        0,
        "a file the host would refuse must not be probed"
    );
    assert_eq!(
        std::fs::read(&file).expect("read"),
        before,
        "the working configuration must be left exactly as it was"
    );
}

/// Adding a valid file that would push the directory over a loader ceiling
/// must fail, including `--dry-run`.
#[test]
fn a_write_that_would_break_the_directory_is_refused_and_dry_run_says_so() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let mock = runtime.block_on(providers::testing::always(
        200,
        r#"{"data":[{"embedding":[0.1,0.2],"index":0}]}"#,
    ));

    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);

    // Siblings whose max_concurrent sums to exactly the 256 ceiling — legal
    // on their own, and one more file of any size tips the directory over.
    for i in 0..4 {
        let file = providers_d.join(format!("sibling{i}.toml"));
        std::fs::write(
            &file,
            format!(
                "provider = \"mistral\"\napi_key_env = \"POSTVEC_DIR_KEY\"\nmax_concurrent = 64\n\n\
                 [[models]]\nname = \"mistral-m{i}\"\nprovider_model_id = \"m{i}\"\ndim = 4\n"
            ),
        )
        .expect("write");
        set_mode(&file, 0o600);
    }
    // Connector files only: the advisory `.lock` the command takes is not
    // one, and is expected to appear.
    let tomls = |dir: &std::path::Path| -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "toml")
            })
            .count()
    };
    let tomls_before = tomls(&providers_d);

    let add = |extra: &[&str]| {
        let mut args = vec![
            "provider",
            "add",
            "openai",
            "--model",
            "text-embedding-3-small",
            "--api-key-env",
            "POSTVEC_DIR_KEY",
            "--base-url",
            mock.url.as_str(),
            "--path",
            root_arg,
            "--acknowledge-in-use",
            "--yes",
        ];
        args.extend_from_slice(extra);
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .env("POSTVEC_DIR_KEY", "not-a-real-key")
            .output()
            .expect("run postvec")
    };

    // The new file defaults to max_concurrent = 4, which is four too many.
    // Dry run first: it must refuse, for the same reason the real run would.
    let output = add(&["--dry-run"]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("as a whole"), "{text}");

    // The real run: refused before any probe, nothing written.
    let output = add(&[]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("total outbound concurrency"), "{text}");
    assert_eq!(mock.request_count(), 0, "refused before the probe");
    assert_eq!(
        tomls(&providers_d),
        tomls_before,
        "nothing may be added to a directory this would break"
    );
}

/// An oversized file is refused, not read as a truncated prefix.
#[test]
fn an_oversized_connector_file_is_refused_not_truncated() {
    let root = provider_root();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);
    let file = providers_d.join("openai.toml");

    // A legal prefix, then a tail that takes it over the ceiling. If the
    // reader truncated, the prefix alone would parse and the command would
    // proceed to rewrite the file without the tail.
    let prefix = "provider = \"openai\"\napi_key_env = \"POSTVEC_TRUNC_KEY\"\n\n[[models]]\n\
                  name = \"openai-text-embedding-3-small\"\n\
                  provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n";
    let tail = format!(
        "\n# {}\n",
        "x".repeat(providers::config::MAX_FILE_BYTES as usize)
    );
    let body = format!("{prefix}{tail}");
    std::fs::write(&file, &body).expect("write");
    set_mode(&file, 0o600);
    let before = std::fs::read(&file).expect("read");

    for verb in [
        vec![
            "provider",
            "add",
            "openai",
            "--model",
            "text-embedding-3-large",
            "--path",
            root.path().to_str().unwrap(),
            "--no-verify",
            "--acknowledge-in-use",
            "--yes",
        ],
        vec![
            "provider",
            "test",
            "openai",
            "--path",
            root.path().to_str().unwrap(),
        ],
        vec!["provider", "ls", "--path", root.path().to_str().unwrap()],
    ] {
        let output = Command::new(binary())
            .args(&verb)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .env("POSTVEC_TRUNC_KEY", "not-a-real-key")
            .output()
            .expect("run postvec");
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            text.contains("larger than") || text.contains("ceiling"),
            "{}: {}",
            verb[1],
            &text[..text.len().min(300)]
        );
    }
    assert_eq!(
        std::fs::read(&file).expect("read"),
        before,
        "the file must be byte-identical: no verb may rewrite a truncated prefix"
    );
}

/// Removing one claimant of a contested name hands the name to the surviving
/// file. With no host to say what is served under that name today, disk
/// identity alone cannot establish that the survivor writes into the same
/// space — the host may hold an older snapshot — so this is refused outright,
/// not acknowledged; the survivor's *other* model is the ordinary activation
/// and would have taken the recipient acknowledgement.
#[test]
fn removing_a_contested_claimant_without_a_host_is_refused() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);

    // Two files claim `shared-name`; the second also carries an unrelated
    // model that is refused along with it while the contest stands.
    let write = |stem: &str, body: &str| {
        let path = providers_d.join(format!("{stem}.toml"));
        std::fs::write(&path, body).expect("write");
        set_mode(&path, 0o600);
    };
    write(
        "alpha",
        "provider = \"openai\"\napi_key = \"inline-test-key\"\n\n[[models]]\nname = \"shared-name\"\n\
         provider_model_id = \"a\"\ndim = 4\n",
    );
    // Same connector, endpoint, upstream model id and width — only the file
    // (and so possibly the account) differs: the same vector space from a
    // different recipient, a handoff an acknowledgement covers.
    write(
        "beta",
        "provider = \"openai\"\napi_key = \"inline-test-key\"\n\n[[models]]\nname = \"shared-name\"\n\
         provider_model_id = \"a\"\ndim = 4\n\n[[models]]\nname = \"beta-only\"\n\
         provider_model_id = \"c\"\ndim = 4\n",
    );

    let rm = |extra: &[&str]| {
        let mut args = vec!["provider", "rm", "alpha", "--path", root_arg, "--yes"];
        args.extend_from_slice(extra);
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .output()
            .expect("run postvec")
    };

    for flags in [&[][..], &["--acknowledge-in-use"][..]] {
        let output = rm(flags);
        assert_ne!(code(&output), 0, "{flags:?}: {}", stdout(&output));
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            text.contains("shared-name") && text.contains("no running host answered"),
            "{flags:?}: {text}"
        );
        assert!(
            providers_d.join("alpha.toml").exists(),
            "{flags:?}: no flag approves a handoff the host cannot vouch for"
        );
    }
}

/// No host: the files say a sibling is served, but the host may have loaded
/// before it existed or refused its secret, and the reload this command asks
/// for would bring it online. A sibling untouched by the removal is therefore
/// treated as *starting* — the recipient acknowledgement, with databases
/// UNKNOWN — and `--yes` alone must not proceed.
#[test]
fn without_a_host_an_untouched_sibling_takes_the_recipient_acknowledgement() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);
    let write = |stem: &str, body: &str| {
        let path = providers_d.join(format!("{stem}.toml"));
        std::fs::write(&path, body).expect("write");
        set_mode(&path, 0o600);
    };
    write(
        "gamma",
        "provider = \"openai\"\napi_key = \"inline-test-key\"\n\n[[models]]\nname = \"openai-x\"\n\
         provider_model_id = \"x\"\ndim = 4\n",
    );
    write(
        "beta",
        "provider = \"mistral\"\napi_key = \"inline-test-key\"\n\n[[models]]\nname = \"beta-only\"\n\
         provider_model_id = \"c\"\ndim = 4\n",
    );
    let rm = |extra: &[&str]| {
        let mut args = vec!["provider", "rm", "gamma", "--path", root_arg, "--yes"];
        args.extend_from_slice(extra);
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .output()
            .expect("run postvec")
    };
    let output = rm(&[]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("beta-only") && text.contains("treated as starting"),
        "{text}"
    );
    assert!(providers_d.join("gamma.toml").exists());

    let output = rm(&["--acknowledge-in-use"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(!providers_d.join("gamma.toml").exists());
    assert!(providers_d.join("beta.toml").exists());
}

/// The surviving claimant declares the same public name with a different
/// semantic identity — upstream model, width, connector or endpoint. That is
/// not a handoff an acknowledgement can cover: the vectors already stored
/// under the name can no longer be assumed to match the ones written next,
/// and nothing migrates them. Refused with every flag, and the fix points at
/// a new public name plus migrate().
#[test]
fn a_repair_that_changes_the_vector_space_under_a_name_is_refused() {
    for (provider, base_url, model_id, dim) in [
        ("openai", "", "b", 4u32),
        ("openai", "", "a", 8u32),
        ("mistral", "", "a", 4u32),
        (
            "openai",
            "base_url = \"https://proxy.example.net/v1\"\n",
            "a",
            4u32,
        ),
    ] {
        let root = provider_root();
        let root_arg = root.path().to_str().unwrap();
        let providers_d = root.path().join("providers.d");
        std::fs::create_dir_all(&providers_d).expect("mkdir");
        set_mode(&providers_d, 0o700);
        let write = |stem: &str, body: &str| {
            let path = providers_d.join(format!("{stem}.toml"));
            std::fs::write(&path, body).expect("write");
            set_mode(&path, 0o600);
        };
        write(
            "alpha",
            "provider = \"openai\"\napi_key = \"inline-test-key\"\n\n[[models]]\nname = \"shared-name\"\n\
             provider_model_id = \"a\"\ndim = 4\n",
        );
        write(
            "beta",
            &format!(
                "provider = \"{provider}\"\napi_key = \"inline-test-key\"\n{base_url}\n[[models]]\n\
                 name = \"shared-name\"\nprovider_model_id = \"{model_id}\"\ndim = {dim}\n"
            ),
        );
        let output = Command::new(binary())
            .args([
                "provider",
                "rm",
                "alpha",
                "--path",
                root_arg,
                "--yes",
                "--acknowledge-in-use",
            ])
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .output()
            .expect("run postvec");
        assert_ne!(
            code(&output),
            0,
            "{provider}/{model_id}/{dim}: {}",
            stdout(&output)
        );
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            text.contains("shared-name") && text.contains("migrate()"),
            "{provider}/{model_id}/{dim}: {text}"
        );
        assert!(
            providers_d.join("alpha.toml").exists(),
            "{provider}/{model_id}/{dim}: no flag removes a file whose removal changes a vector space"
        );
    }
}

/// No host answers and the file no longer declares any model — edited since
/// the host loaded it, or its models section gone. The unreachable host may
/// still be serving routes from the version it read, and nothing on disk can
/// name them. The file itself is then the route whose loss is acknowledged:
/// `--yes` alone must not remove it.
#[test]
fn removing_a_file_with_no_recoverable_routes_and_no_host_needs_the_acknowledgement() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);
    let path = providers_d.join("bare.toml");
    std::fs::write(
        &path,
        "provider = \"openai\"\napi_key = \"inline-test-key\"\n",
    )
    .expect("write");
    set_mode(&path, 0o600);

    let rm = |extra: &[&str]| {
        let mut args = vec!["provider", "rm", "bare", "--path", root_arg, "--yes"];
        args.extend_from_slice(extra);
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .output()
            .expect("run postvec")
    };
    let output = rm(&[]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("bare.toml") && text.contains("--acknowledge-in-use"),
        "{text}"
    );
    assert!(
        text.contains("every route the unreachable host may still serve"),
        "{text}"
    );
    assert!(path.exists(), "nothing removed without the acknowledgement");

    // Interactively the operator types the route names back, so the
    // synthetic one must be a single token: the file name, nothing more.
    let output = Command::new(binary())
        .args([
            "provider",
            "rm",
            "bare",
            "--path",
            root_arg,
            "--dry-run",
            "--format",
            "json",
        ])
        .env_remove("POSTVEC_DATABASE_URL")
        .env_remove("POSTVEC_PATH")
        .env_remove("POSTVEC_PROVIDERS_PATH")
        .env("NO_COLOR", "1")
        .output()
        .expect("run postvec");
    let envelope: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("one JSON object");
    let acknowledged: Vec<&str> = envelope["plan"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(|step| step["model"].as_str())
        .collect();
    assert_eq!(
        acknowledged,
        vec!["bare.toml"],
        "the acknowledged route is exactly the file name: {envelope}"
    );

    let output = rm(&["--acknowledge-in-use"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(!path.exists());
}

/// The same bare file beside a sibling that serves afterwards: one of the
/// routes the unreachable host still serves from the bare file may share a
/// public name with the sibling's under a different vector identity, which
/// neither acknowledgement states or prevents. Refused with every flag until
/// a host answers.
#[test]
fn a_bare_file_beside_a_surviving_route_is_refused_without_a_host() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);
    let write = |stem: &str, body: &str| {
        let path = providers_d.join(format!("{stem}.toml"));
        std::fs::write(&path, body).expect("write");
        set_mode(&path, 0o600);
    };
    write(
        "bare",
        "provider = \"openai\"\napi_key = \"inline-test-key\"\n",
    );
    write(
        "beta",
        "provider = \"mistral\"\napi_key = \"inline-test-key\"\n\n[[models]]\nname = \"shared\"\n\
         provider_model_id = \"mistral-embed\"\ndim = 1024\n",
    );
    for flags in [&[][..], &["--acknowledge-in-use"][..]] {
        let mut args = vec!["provider", "rm", "bare", "--path", root_arg, "--yes"];
        args.extend_from_slice(flags);
        let output = Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .output()
            .expect("run postvec");
        assert_ne!(code(&output), 0, "{flags:?}: {}", stdout(&output));
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            text.contains("no running host answered") && text.contains("rm(1)"),
            "{flags:?}: {text}"
        );
        assert!(providers_d.join("bare.toml").exists(), "{flags:?}");
    }
}

/// Removing a file can *repair* a directory the host refuses as a whole — and
/// then every remaining provider comes online at once. That is the largest
/// recipient change this command can cause, and it must take the recipient
/// acknowledgement rather than sail through on `--yes`.
#[test]
fn removing_the_file_that_repairs_an_over_ceiling_directory_needs_the_acknowledgement() {
    let root = provider_root();
    let root_arg = root.path().to_str().unwrap();
    let providers_d = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_d).expect("mkdir");
    set_mode(&providers_d, 0o700);

    // Five files at the per-file maximum: 320 total, over the 256 ceiling.
    for i in 0..5 {
        let path = providers_d.join(format!("p{i}.toml"));
        std::fs::write(
            &path,
            format!(
                "provider = \"openai\"\napi_key = \"inline-test-key\"\nmax_concurrent = 64\n\n\
                 [[models]]\nname = \"openai-m{i}\"\nprovider_model_id = \"m{i}\"\ndim = 4\n"
            ),
        )
        .expect("write");
        set_mode(&path, 0o600);
    }

    let rm = |extra: &[&str]| {
        let mut args = vec!["provider", "rm", "p4", "--path", root_arg, "--yes"];
        args.extend_from_slice(extra);
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env("NO_COLOR", "1")
            .output()
            .expect("run postvec")
    };

    let output = rm(&[]);
    assert_ne!(code(&output), 0, "{}", stdout(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    for name in ["openai-m0", "openai-m1", "openai-m2", "openai-m3"] {
        assert!(
            text.contains(name),
            "{name} comes online and must be named: {text}"
        );
    }
    // And the removed file's own model is not described as losing a route:
    // it was never served.
    assert!(
        !text.contains("lose their embedding route"),
        "nothing was served before, so nothing is lost: {text}"
    );
    assert!(providers_d.join("p4.toml").exists());

    let output = rm(&["--acknowledge-in-use"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
}

// ---- `provider add univec` discovery, `provider ls --available` ----------
//
// A path-aware mock aphex on loopback serves the four routes the flow uses:
// the public catalogue, the free identity check, and the two billed probes.
// Every test counts requests per route, so "no billed call" claims are
// checked against a mock that can observe one (positive controls below).

mod univec_discovery {
    use super::*;
    use providers::testing::{routes, Mock, Route};

    /// Two embeds (dims 3 and 4), three converters, one bridge entry.
    const CATALOGUE: &str = r#"{"success":true,"data":[
      {"name":"cheap","modelType":"embed","executionProvider":"cpu","targetModel":"cheap","targetDim":3,"sequenceLen":256},
      {"name":"cheap-sku-v2","modelType":"embed","executionProvider":"cpu","targetModel":"cheap","targetDim":3,"sequenceLen":256},
      {"name":"big","modelType":"embed","executionProvider":"cpu","targetModel":"big","targetDim":4,"sequenceLen":8192},
      {"name":"convert-src-to-big","modelType":"convert","sourceModel":"src","targetModel":"big","sourceDim":1536,"targetDim":4,"eval":{"cosine_mean":0.9}},
      {"name":"convert-other-to-big","modelType":"convert","sourceModel":"other","targetModel":"big","sourceDim":8,"targetDim":4},
      {"name":"convert-src-to-cheap","modelType":"convert","sourceModel":"src","targetModel":"cheap","sourceDim":1536,"targetDim":3},
      {"name":"embed-bridge","modelType":"embed-bridge","restrictedTargets":["x"]}
    ]}"#;
    const KEY: &str = "uv_test_login_key_value";
    const EMBED_3: &str = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
    const EMBED_4: &str = r#"{"data":[{"embedding":[0.1,0.2,0.3,0.4],"index":0}]}"#;
    const CONVERT_4: &str = r#"{"success":true,"data":{"embeddings":[[0.1,0.2,0.3,0.4]]}}"#;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn mock(
        rt: &tokio::runtime::Runtime,
        catalogue: (u16, &str),
        embed: &str,
        identity: (u16, &str),
    ) -> Mock {
        rt.block_on(routes(vec![
            Route::new("GET", "/v1/models", catalogue.0, catalogue.1),
            Route::new("POST", "/v1/embeddings", 200, embed),
            Route::new("POST", "/v1/convert", 200, CONVERT_4),
            Route {
                bearer: Some(KEY.to_string()),
                ..Route::new("GET", "/v1/registry/index.json", identity.0, identity.1)
            },
        ]))
    }

    fn standard(rt: &tokio::runtime::Runtime) -> Mock {
        mock(rt, (200, CATALOGUE), EMBED_3, (200, "{}"))
    }

    fn add(mock: &Mock, root: &std::path::Path, extra: &[&str]) -> Output {
        let mut args = vec!["provider", "add", "univec"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&[
            "--base-url",
            &mock.url,
            "--path",
            root.to_str().unwrap(),
            "--acknowledge-in-use",
            "--yes",
        ]);
        // A rerun on an existing file passes no key source (the file's
        // own is reused, so nothing counts as a credential change).
        if !extra
            .iter()
            .any(|a| a.starts_with("--api-key") || *a == "--existing-key")
        {
            args.extend_from_slice(&["--api-key-env", "POSTVEC_UNIVEC_TEST_KEY"]);
        }
        args.retain(|a| *a != "--existing-key");
        Command::new(binary())
            .args(&args)
            .env_remove("POSTVEC_DATABASE_URL")
            .env_remove("POSTVEC_PATH")
            .env_remove("POSTVEC_PROVIDERS_PATH")
            .env_remove("POSTVEC_API_KEY")
            .env("NO_COLOR", "1")
            .env("POSTVEC_UNIVEC_TEST_KEY", KEY)
            .output()
            .expect("run postvec")
    }

    fn text(output: &Output) -> String {
        format!("{}{}", stdout(output), stderr(output))
    }

    fn file(root: &std::path::Path) -> String {
        std::fs::read_to_string(root.join("providers.d/univec.toml")).unwrap_or_default()
    }

    /// Scenarios 1, 8 and the `--no-verify` rule: the zero-argument add
    /// materialises every catalogue embed with catalogue dims and
    /// `max_tokens`, bills exactly one embed against the cheapest ADDED
    /// model, and a rerun spends nothing.
    #[test]
    fn a_zero_argument_add_writes_every_embed_and_bills_one_probe() {
        let rt = runtime();
        let mock = standard(&rt);
        let root = provider_root();
        let out = add(&mock, root.path(), &[]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        let body = file(root.path());
        for line in [
            "name = \"univec-cheap\"",
            "provider_model_id = \"cheap\"",
            "dim = 3",
            "max_tokens = 256",
            "name = \"univec-big\"",
            "dim = 4",
            "max_tokens = 8192",
        ] {
            assert!(body.contains(line), "missing {line}:\n{body}");
        }
        assert!(
            !body.contains("kind = \"convert\""),
            "no converters by default:\n{body}"
        );
        assert!(
            !body.contains("cheap-sku-v2"),
            "an alias SKU collapses onto its targetModel:\n{body}"
        );
        assert_eq!(mock.path_count("/v1/embeddings"), 1, "one billed embed");
        assert_eq!(mock.path_count("/v1/convert"), 0);
        assert_eq!(
            mock.path_count("/v1/registry/index.json"),
            1,
            "the free identity check ran"
        );
        assert!(
            mock.last_request().contains("\"model\":\"cheap\""),
            "the cheapest added embed is probed: {}",
            mock.last_request()
        );
        let t = text(&out);
        assert!(
            t.contains("catalogue: 2 embed, 0 convert added, 0 already present"),
            "{t}"
        );
        assert!(t.contains("--convert-to"), "the opt-in hint:\n{t}");

        // Rerun: already present, zero new upstream calls.
        let out = add(&mock, root.path(), &["--existing-key"]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        assert!(text(&out).contains("already declared"), "{}", text(&out));
        assert_eq!(mock.path_count("/v1/embeddings"), 1);
        assert_eq!(mock.path_count("/v1/registry/index.json"), 1);

        // `--no-verify` with catalogue dims does not demand --dim.
        let fresh = provider_root();
        let out = add(&mock, fresh.path(), &["--no-verify"]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        assert!(file(fresh.path()).contains("dim = 4"));
        assert_eq!(
            mock.path_count("/v1/embeddings"),
            1,
            "--no-verify spent nothing"
        );
    }

    /// Scenarios 2 and 10: a convert-only add writes both vocabularies from
    /// the catalogue, bills one convert probe and no embed probe.
    #[test]
    fn convert_to_adds_every_route_into_the_target_with_one_convert_probe() {
        let rt = runtime();
        let mock = standard(&rt);
        let root = provider_root();
        let out = add(&mock, root.path(), &["--convert-to", "big"]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        let body = file(root.path());
        for line in [
            "name = \"univec-convert-src-to-big\"",
            "name = \"univec-convert-other-to-big\"",
            "kind = \"convert\"",
            "provider_source_id = \"src\"",
            "source_model = \"src\"",
            "target_model = \"big\"",
            "source_dim = 1536",
            "source_dim = 8",
            "dim = 4",
        ] {
            assert!(body.contains(line), "missing {line}:\n{body}");
        }
        assert!(!body.contains("univec-convert-src-to-cheap"), "{body}");
        assert_eq!(mock.path_count("/v1/registry/index.json"), 1);
        assert_eq!(
            mock.path_count("/v1/embeddings"),
            0,
            "convert-only: no embed probe"
        );
        assert_eq!(mock.path_count("/v1/convert"), 1);
        let t = text(&out);
        assert!(t.contains("catalogue: 0 embed, 2 convert added"), "{t}");
        assert!(t.contains("local embed model of the target"), "{t}");

        // The union: --model plus a single --convert with a custom name,
        // one billed call of each kind.
        let fresh = provider_root();
        let out = add(
            &mock,
            fresh.path(),
            &[
                "--model",
                "cheap",
                "--convert",
                "other:big",
                "--converter-name",
                "route-x",
            ],
        );
        assert_eq!(code(&out), 0, "{}", text(&out));
        let body = file(fresh.path());
        assert!(
            body.contains("name = \"route-x\"") && body.contains("name = \"univec-cheap\""),
            "{body}"
        );
        assert_eq!(
            (
                mock.path_count("/v1/embeddings"),
                mock.path_count("/v1/convert")
            ),
            (1, 2)
        );
    }

    /// Scenarios 3 and 9: selections the catalogue cannot satisfy are
    /// refused before anything is written, naming the way out.
    #[test]
    fn unlisted_selections_are_refused_with_the_fallback_named() {
        let rt = runtime();
        let mock = standard(&rt);
        let root = provider_root();
        let out = add(&mock, root.path(), &["--convert", "a:b"]);
        assert_ne!(code(&out), 0);
        assert!(text(&out).contains("--convert-source"), "{}", text(&out));
        let out = add(&mock, root.path(), &["--convert-to", "nowhere"]);
        assert_ne!(code(&out), 0);
        assert!(
            text(&out).contains("targets are: big, cheap"),
            "{}",
            text(&out)
        );
        assert!(!root.path().join("providers.d/univec.toml").exists());
        assert_eq!(
            mock.path_count("/v1/embeddings") + mock.path_count("/v1/convert"),
            0
        );
    }

    /// A catalogue that breaks its contract (a known kind missing a
    /// dimension) is refused whole — even for a `--model` add that could
    /// have fallen back — with the row named, nothing written, nothing billed.
    #[test]
    fn a_catalogue_that_breaks_its_contract_is_refused_whole() {
        let rt = runtime();
        let broken = CATALOGUE.replace(
            r#""targetModel":"big","targetDim":4,"sequenceLen":8192"#,
            r#""targetModel":"big","sequenceLen":8192"#,
        );
        let mock = mock(&rt, (200, &broken), EMBED_3, (200, "{}"));
        let root = provider_root();
        for extra in [&[][..], &["--model", "cheap"][..]] {
            let out = add(&mock, root.path(), extra);
            assert_ne!(code(&out), 0, "{}", text(&out));
            let t = text(&out);
            assert!(
                t.contains("violates its contract") && t.contains("targetDim is missing"),
                "{t}"
            );
        }
        assert!(!root.path().join("providers.d/univec.toml").exists());
        assert_eq!(
            mock.path_count("/v1/embeddings") + mock.path_count("/v1/registry/index.json"),
            0
        );
        // A configured file pointing at the broken catalogue: `ls --available`
        // names the row (a bare lookup would go to the real API).
        assert_eq!(
            code(&add(
                &mock,
                root.path(),
                &["--no-catalog", "--model", "x", "--dim", "3", "--no-verify"]
            )),
            0
        );
        let out = run(&[
            "provider",
            "ls",
            "--available",
            "univec",
            "--path",
            root.path().to_str().unwrap(),
            "--format",
            "json",
        ]);
        assert_ne!(code(&out), 0);
        let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
        assert_eq!(doc["providers"][0]["outcome"], "failed", "{doc}");
        assert!(
            doc["providers"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("targetDim"),
            "{doc}"
        );
    }

    /// Scenario 4: a dead catalogue stops a run that needs it and only
    /// warns a run that named its models.
    #[test]
    fn an_unreachable_catalogue_fails_only_selections_that_need_it() {
        let rt = runtime();
        let mock = mock(&rt, (500, r#"{"error":"down"}"#), EMBED_3, (200, "{}"));
        let root = provider_root();
        let out = add(&mock, root.path(), &[]);
        assert_ne!(code(&out), 0);
        assert!(text(&out).contains("/v1/models"), "{}", text(&out));
        let out = add(
            &mock,
            root.path(),
            &["--model", "x", "--dim", "3", "--no-verify"],
        );
        assert_eq!(code(&out), 0, "{}", text(&out));
        assert!(text(&out).contains("unreachable"), "{}", text(&out));
        assert!(file(root.path()).contains("univec-x"));
        // --no-catalog + --no-verify still needs --dim: nothing supplied one.
        let out = add(
            &mock,
            root.path(),
            &["--no-catalog", "--model", "y", "--no-verify"],
        );
        assert_eq!(code(&out), 2, "{}", text(&out));
        assert!(text(&out).contains("--dim"), "{}", text(&out));
    }

    /// Scenarios 5, 6 and 11: `ls --available` per file, joined against
    /// what is configured, and honest about connectors that cannot list.
    #[test]
    fn ls_available_lists_per_file_and_names_what_cannot_list() {
        let rt = runtime();
        let mock = standard(&rt);
        let other = standard(&rt);
        let root = provider_root();
        assert_eq!(
            code(&add(
                &mock,
                root.path(),
                &["--model", "cheap", "--convert", "src:big", "--no-verify"]
            )),
            0
        );
        let out = add(
            &other,
            root.path(),
            &["--name", "univec-staging", "--model", "big", "--no-verify"],
        );
        assert_eq!(code(&out), 0, "{}", text(&out));

        let root_arg = root.path().to_str().unwrap();
        let out = run(&[
            "provider",
            "ls",
            "--available",
            "univec",
            "--path",
            root_arg,
        ]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        let t = text(&out);
        assert!(t.contains("yes (univec-cheap)"), "{t}");
        assert!(t.contains("yes (univec-convert-src-to-big)"), "{t}");
        assert!(t.contains("cos 0.900"), "{t}");
        assert!(
            t.contains("univec-staging"),
            "two files, two catalogues:\n{t}"
        );

        let out = run(&[
            "provider",
            "ls",
            "--available",
            "--path",
            root_arg,
            "--format",
            "json",
            "--to",
            "big",
        ]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
        let providers = doc["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 2, "{doc}");
        assert!(providers.iter().all(|p| p["outcome"] == "entries"), "{doc}");
        let main = providers.iter().find(|p| p["name"] == "univec").unwrap();
        let models = main["models"].as_array().unwrap();
        assert_eq!(models.len(), 2, "--to big: {doc}");
        assert!(
            models
                .iter()
                .any(|m| m["configured"] == true
                    && m["configured_name"] == "univec-convert-src-to-big"),
            "{doc}"
        );

        let out = run(&[
            "provider",
            "ls",
            "--available",
            "openai",
            "--path",
            root_arg,
            "--format",
            "json",
        ]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
        assert_eq!(doc["providers"][0]["outcome"], "unsupported", "{doc}");
        assert!(
            doc["providers"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("--model"),
            "{doc}"
        );
        // Filters need --available; a listing failure is a named error.
        assert_eq!(
            code(&run(&["provider", "ls", "--to", "big", "--path", root_arg])),
            2
        );
        let dead = rt.block_on(routes(vec![Route::new("GET", "/v1/models", 503, "{}")]));
        let out = run(&[
            "provider",
            "add",
            "univec",
            "--name",
            "dead",
            "--model",
            "m",
            "--dim",
            "3",
            "--no-verify",
            "--base-url",
            &dead.url,
            "--path",
            root_arg,
            "--api-key-env",
            "X",
            "--acknowledge-in-use",
            "--yes",
        ]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        let out = run(&["provider", "ls", "--available", "dead", "--path", root_arg]);
        assert_ne!(code(&out), 0);
        assert!(
            text(&out).contains("listing failed for dead"),
            "{}",
            text(&out)
        );
    }

    /// A configured file that cannot be loaded is a named `failed` result
    /// in both formats, the other files still list, and a request for that
    /// stem does not degrade into a bare-connector lookup.
    #[test]
    fn ls_available_names_a_malformed_configured_file() {
        let rt = runtime();
        let mock = standard(&rt);
        let root = provider_root();
        assert_eq!(
            code(&add(
                &mock,
                root.path(),
                &["--model", "cheap", "--no-verify"]
            )),
            0
        );
        let providers_d = root.path().join("providers.d");
        std::fs::write(
            providers_d.join("univec-staging.toml"),
            "provider = \"univec\"\nnot toml at all [[[",
        )
        .unwrap();
        set_mode(&providers_d.join("univec-staging.toml"), 0o600);
        std::os::unix::fs::symlink(
            providers_d.join("univec.toml"),
            providers_d.join("linked.toml"),
        )
        .unwrap();
        let root_arg = root.path().to_str().unwrap();

        let out = run(&[
            "provider",
            "ls",
            "--available",
            "--path",
            root_arg,
            "--format",
            "json",
        ]);
        assert_ne!(code(&out), 0, "{}", text(&out));
        let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
        let by = |name: &str| {
            doc["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("no {name} in {doc}"))
        };
        assert_eq!(by("univec")["outcome"], "entries");
        assert_eq!(by("univec-staging")["outcome"], "failed");
        assert!(
            by("univec-staging")["reason"]
                .as_str()
                .unwrap()
                .contains("parse"),
            "{doc}"
        );
        assert_eq!(by("linked")["outcome"], "failed");
        assert!(
            by("linked")["reason"].as_str().unwrap().contains("symlink"),
            "{doc}"
        );

        // Naming the broken stem: one failed result, no bare lookup.
        let out = run(&[
            "provider",
            "ls",
            "--available",
            "univec-staging",
            "--path",
            root_arg,
        ]);
        assert_ne!(code(&out), 0);
        let t = text(&out);
        assert!(
            t.contains("univec-staging") && t.contains("cannot list"),
            "{t}"
        );
        assert!(
            !t.contains("public catalogue"),
            "must not fall back to the bare connector:\n{t}"
        );

        // An unreadable directory is a failed result too (not as root).
        if unsafe { libc::geteuid() } != 0 {
            set_mode(&providers_d, 0o000);
            let out = run(&[
                "provider",
                "ls",
                "--available",
                "--path",
                root_arg,
                "--format",
                "json",
            ]);
            set_mode(&providers_d, 0o700);
            assert_ne!(code(&out), 0);
            let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
            assert!(
                doc["providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["outcome"] == "failed"
                        && p["reason"].as_str().unwrap().contains("scan")),
                "{doc}"
            );
        }
    }

    /// Scenario 7: a converter whose stated width contradicts the listed
    /// embed of the same name is refused with both numbers.
    #[test]
    fn a_catalogue_that_contradicts_itself_is_refused() {
        let rt = runtime();
        let contradicting = CATALOGUE.replace(
            r#""targetModel":"cheap","targetDim":3,"sequenceLen":256"#,
            r#""targetModel":"cheap","targetDim":9,"sequenceLen":256"#,
        );
        let mock = mock(&rt, (200, &contradicting), EMBED_3, (200, "{}"));
        let root = provider_root();
        let out = add(&mock, root.path(), &["--convert", "src:cheap"]);
        assert_ne!(code(&out), 0);
        let t = text(&out);
        assert!(
            t.contains('9') && t.contains("target dimension of 3"),
            "{t}"
        );
        assert_eq!(mock.path_count("/v1/convert"), 0);
    }

    /// Scenario 12: the free identity check. A 401 stops the run before a
    /// billed call; a 500 on that route is a warning and the probe decides.
    #[test]
    fn the_identity_check_stops_a_bad_key_and_tolerates_an_outage() {
        let rt = runtime();
        let mock = standard(&rt);
        let root = provider_root();
        let out = Command::new(binary())
            .args([
                "provider",
                "add",
                "univec",
                "--base-url",
                &mock.url,
                "--path",
                root.path().to_str().unwrap(),
                "--api-key-env",
                "WRONG",
                "--acknowledge-in-use",
                "--yes",
            ])
            .env_remove("POSTVEC_API_KEY")
            .env("NO_COLOR", "1")
            .env("WRONG", "uv_not_the_registered_key")
            .output()
            .unwrap();
        assert_ne!(code(&out), 0);
        assert!(text(&out).contains("nothing was billed"), "{}", text(&out));
        assert_eq!(
            mock.path_count("/v1/embeddings"),
            0,
            "refused before the billed probe"
        );
        assert!(!root.path().join("providers.d/univec.toml").exists());
        // Positive control on the same mock: the right key is billed once.
        assert_eq!(code(&add(&mock, root.path(), &[])), 0);
        assert_eq!(mock.path_count("/v1/embeddings"), 1);

        for (status, expect) in [(500, "answered 500"), (429, "answered 429")] {
            let flaky = mock_with_identity(&rt, (status, "{}"));
            let fresh = provider_root();
            let out = add(&flaky, fresh.path(), &[]);
            assert_eq!(code(&out), 0, "{}", text(&out));
            assert!(text(&out).contains(expect), "{}", text(&out));
            assert_eq!(flaky.path_count("/v1/embeddings"), 1);
        }
        // Route absent (a front without the registry feature): best effort.
        let absent = rt.block_on(routes(vec![
            Route::new("GET", "/v1/models", 200, CATALOGUE),
            Route::new("POST", "/v1/embeddings", 200, EMBED_3),
        ]));
        let fresh = provider_root();
        let out = add(&absent, fresh.path(), &[]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        assert!(text(&out).contains("answered 404"), "{}", text(&out));
        assert_eq!(absent.path_count("/v1/embeddings"), 1);
    }

    fn mock_with_identity(rt: &tokio::runtime::Runtime, identity: (u16, &str)) -> Mock {
        mock(rt, (200, CATALOGUE), EMBED_3, identity)
    }

    /// Scenario 13: the probe contradicting the catalogue is a refusal,
    /// never a silent patch; and `--dim` cannot override the catalogue.
    #[test]
    fn a_probe_that_disagrees_with_the_catalogue_is_refused() {
        let rt = runtime();
        let mock = mock(&rt, (200, CATALOGUE), EMBED_4, (200, "{}"));
        let root = provider_root();
        // Only `cheap` (3) is added, so `cheap` is probed and measures 4.
        let out = add(&mock, root.path(), &["--model", "cheap"]);
        assert_ne!(code(&out), 0);
        let t = text(&out);
        assert!(t.contains("lists 3") && t.contains("measured 4"), "{t}");
        assert!(!root.path().join("providers.d/univec.toml").exists());
        assert_eq!(mock.path_count("/v1/embeddings"), 1, "the probe did run");
        let out = add(&mock, root.path(), &["--model", "big", "--dim", "5"]);
        assert_eq!(code(&out), 2, "{}", text(&out));
        assert!(text(&out).contains("lists big at 4"), "{}", text(&out));
        // The same catalogue with a probe that agrees with `big`.
        let out = add(&mock, root.path(), &["--model", "big"]);
        assert_eq!(code(&out), 0, "{}", text(&out));
    }

    /// The login-key reuse, scripted: the key is COPIED into keys/univec.key
    /// (0600) and referenced, never written inline; without a credential the
    /// flag is a usage error.
    #[test]
    fn api_key_from_login_copies_the_stored_key_into_a_referenced_file() {
        use std::os::unix::fs::PermissionsExt;
        let rt = runtime();
        let mock = standard(&rt);
        let root = provider_root();
        let out = Command::new(binary())
            .args([
                "provider",
                "add",
                "univec",
                "--api-key-from-login",
                "--base-url",
                &mock.url,
                "--path",
                root.path().to_str().unwrap(),
                "--acknowledge-in-use",
                "--yes",
            ])
            .env("NO_COLOR", "1")
            .env("POSTVEC_API_KEY", KEY)
            .output()
            .unwrap();
        assert_eq!(code(&out), 0, "{}", text(&out));
        let key_file = root.path().join("keys/univec.key");
        assert_eq!(std::fs::read_to_string(&key_file).unwrap(), KEY);
        assert_eq!(
            std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let body = file(root.path());
        assert!(
            body.contains(&format!("api_key_file = \"{}\"", key_file.display())),
            "{body}"
        );
        assert!(!body.contains("api_key = "), "never inline:\n{body}");
        assert!(
            text(&out).contains("copied the postvec login key"),
            "{}",
            text(&out)
        );
        assert!(
            text(&out).contains(&format!("create {}", key_file.display())),
            "the plan names the key file:\n{}",
            text(&out)
        );
        assert_eq!(mock.path_count("/v1/embeddings"), 1);

        // A second connector file gets its own key file, never the first's.
        let out = Command::new(binary())
            .args([
                "provider",
                "add",
                "univec",
                "--name",
                "univec-staging",
                "--convert-to",
                "big",
                "--api-key-from-login",
                "--base-url",
                &mock.url,
                "--path",
                root.path().to_str().unwrap(),
                "--acknowledge-in-use",
                "--yes",
            ])
            .env("NO_COLOR", "1")
            .env("POSTVEC_API_KEY", KEY)
            .output()
            .unwrap();
        assert_eq!(code(&out), 0, "{}", text(&out));
        let staging_key = root.path().join("keys/univec-staging.key");
        assert!(staging_key.is_file());
        assert!(
            std::fs::read_to_string(root.path().join("providers.d/univec-staging.toml"))
                .unwrap()
                .contains("univec-staging.key")
        );

        // An existing key file with a different key is never replaced.
        std::fs::write(&staging_key, "uv_someone_elses_key").unwrap();
        let out = Command::new(binary())
            .args([
                "provider",
                "add",
                "univec",
                "--name",
                "univec-staging",
                "--convert-to",
                "big",
                "--api-key-from-login",
                "--base-url",
                &mock.url,
                "--path",
                root.path().to_str().unwrap(),
                "--acknowledge-in-use",
                "--yes",
            ])
            .env("NO_COLOR", "1")
            .env("POSTVEC_API_KEY", KEY)
            .output()
            .unwrap();
        assert_ne!(code(&out), 0);
        assert!(text(&out).contains("different key"), "{}", text(&out));
        assert_eq!(
            std::fs::read_to_string(&staging_key).unwrap(),
            "uv_someone_elses_key"
        );

        // `rm` retains the copied key and says so by name.
        let out = run(&[
            "provider",
            "rm",
            "univec",
            "--path",
            root.path().to_str().unwrap(),
            "--acknowledge-in-use",
            "--yes",
        ]);
        assert_eq!(code(&out), 0, "{}", text(&out));
        assert!(
            text(&out).contains("univec.key") && text(&out).contains("retained"),
            "{}",
            text(&out)
        );
        assert!(key_file.is_file());

        let out = Command::new(binary())
            .args([
                "provider",
                "add",
                "univec",
                "--api-key-from-login",
                "--base-url",
                &mock.url,
                "--path",
                root.path().to_str().unwrap(),
                "--yes",
            ])
            .env("NO_COLOR", "1")
            .env_remove("POSTVEC_API_KEY")
            .env(
                "XDG_CONFIG_HOME",
                root.path().join("nostore").to_str().unwrap(),
            )
            .output()
            .unwrap();
        assert_eq!(code(&out), 2, "{}", text(&out));
        assert!(
            text(&out).contains("--api-key-from-login"),
            "{}",
            text(&out)
        );
        // Other connectors do not get the flag.
        assert_eq!(
            code(&run(&[
                "provider",
                "add",
                "openai",
                "--model",
                "m",
                "--api-key-from-login",
                "--path",
                root.path().to_str().unwrap(),
                "--yes"
            ])),
            2
        );
    }

    /// The manual converter flags stay usable and hidden; the selectors
    /// are documented; a bare `provider add univec` is accepted by clap.
    #[test]
    fn help_shows_the_selectors_and_hides_the_manual_flags() {
        let help = stdout(&run(&["provider", "add", "--help"]));
        for flag in [
            "--convert <SRC:DST>",
            "--convert-to",
            "--convert-from",
            "--all-converters",
            "--api-key-from-login",
            "--no-catalog",
        ] {
            assert!(help.contains(flag), "missing {flag}:\n{help}");
        }
        assert!(!help.contains("--convert-source"), "{help}");
        assert!(stdout(&run(&["provider", "ls", "--help"])).contains("--available"));
        // Reaches run(): the refusal is about the missing root, not clap.
        assert_ne!(
            code(&run(&[
                "provider",
                "add",
                "univec",
                "--path",
                "/nonexistent-root",
                "--yes"
            ])),
            2
        );
    }
}

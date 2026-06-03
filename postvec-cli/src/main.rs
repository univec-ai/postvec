//! Binary entry: clap parse, then the two pre-runtime paths (__db-agent,
//! merge-preload) before the async command runtime.

use clap::Parser;
use postvec_cli::cli::{Cli, Command};
use postvec_cli::error::Exit;
use postvec_cli::{commands, config, db};

/// Name the flag whose value arrived empty.
///
/// `--path "$ROOT"` with ROOT unset is a real empty argv token. Clap then
/// says the flag was missing, which looks like a bare `--path`. The fix is
/// in the caller's shell.
fn empty_flag_value(args: &[std::ffi::OsString]) -> Option<String> {
    args.windows(2).find_map(|pair| {
        let (flag, value) = (pair[0].to_str()?, &pair[1]);
        (value.is_empty() && flag.starts_with('-') && flag.len() > 1).then(|| flag.to_string())
    })
}

fn main() -> std::process::ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // Only ever consulted once clap has already refused: an empty
            // value is a legitimate string elsewhere, and this must not
            // change what parses.
            if e.kind() == clap::error::ErrorKind::InvalidValue {
                if let Some(flag) = empty_flag_value(&std::env::args_os().collect::<Vec<_>>()) {
                    eprintln!("postvec: {flag} was given an empty value");
                    eprintln!(
                        "postvec:   fix: a shell variable in the command is unset or empty — \
                         check it, or drop {flag} to use the default"
                    );
                    return std::process::ExitCode::from(Exit::Usage.code() as u8);
                }
            }
            // Clap already renders help/usage; keep its output and map its
            // "this was informational" cases to success.
            let _ = e.print();
            return match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    std::process::ExitCode::SUCCESS
                }
                _ => std::process::ExitCode::from(Exit::Usage.code() as u8),
            };
        }
    };

    // The privilege-dropped database agent is a plain synchronous stdin/stdout
    // loop; it must not inherit the parent's signal handling or output
    // conventions, so it is dispatched before anything else is set up.
    if let Command::DbAgent(args) = &cli.command {
        return db::agent::serve(args.drop_to.as_deref());
    }

    // Pure string work for the container entrypoint: no cluster, no database,
    // no async runtime. It lives here rather than in a shell function because
    // `shared_preload_libraries` uses the server's quoting and case-folding
    // grammar, and one implementation of that is enough.
    if let Command::PreloadMerge(args) = &cli.command {
        // Validate before merging. Repairing malformed syntax here would turn
        // a value the postmaster refuses into one it accepts, and the operator
        // would never learn their input was wrong.
        if let Err(e) = config::guc::validate_library_list(&args.value) {
            eprintln!("postvec: {e}");
            eprintln!("postvec:   value: {}", args.value);
            return std::process::ExitCode::from(Exit::Usage.code() as u8);
        }
        let items = config::guc::parse_library_list(&args.value);
        println!(
            "{}",
            config::guc::render_library_list(&config::guc::merge_postvec(&items))
        );
        return std::process::ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("postvec: cannot start async runtime: {e}");
            return std::process::ExitCode::from(Exit::Failure.code() as u8);
        }
    };

    let exit = runtime.block_on(commands::dispatch(cli));
    std::process::ExitCode::from(exit.code() as u8)
}

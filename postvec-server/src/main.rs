//! Binary entry. The library owns serve and the node-local subcommands.

use std::process::ExitCode;

fn main() -> ExitCode {
    postvec_server::run()
}

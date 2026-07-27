// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Binary entry. The library owns serve and the node-local subcommands.

use std::process::ExitCode;

fn main() -> ExitCode {
    postvec_server::run()
}

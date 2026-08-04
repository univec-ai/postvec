// SPDX-License-Identifier: BUSL-1.1

mod install;
mod platform;

use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install the plain SQL schema, or verify an existing installation.
    Install(ConnectionArgs),
    /// Show schema version, platform, heartbeat and queue counts as JSON.
    Status(ConnectionArgs),
    /// Remove managed objects, preserving user tables and vector columns.
    Uninstall(ConnectionArgs),
}

#[derive(Debug, Args)]
pub struct ConnectionArgs {
    #[arg(long, value_name = "DSN")]
    pub dsn: String,
    /// Read the database password from a regular file with mode 0600.
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
    /// Connection and statement timeout in seconds.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=3600))]
    pub timeout: u32,
}

pub async fn run(command: Command) -> anyhow::Result<i32> {
    install::run(command).await?;
    Ok(0)
}

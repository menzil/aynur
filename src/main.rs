mod app;
mod cli;
mod commands;
mod daemon;
mod env_file;
mod ipc;
mod paths;
mod process;

use anyhow::Context;
use clap::Parser;
use cli::{AynurCli, Command};
use commands::{ExecuteCommand, print_version};
use paths::AynurPaths;

fn main() -> anyhow::Result<()> {
    let cli = AynurCli::parse();
    if cli.show_version || matches!(&cli.command, Some(Command::Version)) {
        print_version();
        return Ok(());
    }

    let paths = AynurPaths::from_env()?;
    paths.ensure()?;

    let command = cli
        .command
        .context("missing command; run `aynur help` for usage")?;
    command.execute(&paths)
}

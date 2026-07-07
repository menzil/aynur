use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "aynur")]
#[command(
    about = "A minimal pm2-like process guardian for Rust binaries",
    long_about = "aynur keeps local Rust binaries running with a small daemon, pm2-like commands, local JSON state, and stdout/stderr log files.\n\nState is stored in ~/.aynur by default. Set AYNUR_HOME to use another directory.",
    after_help = "Examples:\n  aynur start ./target/release/api -- --port 3000\n  aynur start ./target/release/worker -n worker --cwd /srv/app --env-file .env\n  aynur list\n  aynur reload api --update-env\n  aynur logs api\n  aynur stop api\n  aynur delete api"
)]
pub struct AynurCli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Start and supervise a binary",
        long_about = "Start a local executable under the aynur daemon. The daemon writes app config to AYNUR_HOME/apps, redirects stdout/stderr to AYNUR_HOME/logs, and restarts the process when it exits unexpectedly.\n\nArguments after -- are passed directly to the managed binary.",
        after_help = "Examples:\n  aynur start ./target/release/api -- --port 3000\n  aynur start /srv/app/bin/worker -n worker --cwd /srv/app --env-file /srv/app/.env"
    )]
    Start {
        #[arg(help = "Path to the executable to supervise")]
        binary: PathBuf,
        #[arg(
            short = 'n',
            long,
            help = "App name used by stop/restart/reload/logs/delete; defaults to the binary file name"
        )]
        name: Option<String>,
        #[arg(long, help = "Working directory for the managed process")]
        cwd: Option<PathBuf>,
        #[arg(
            long = "env-file",
            help = "KEY=VALUE env file merged over the current environment"
        )]
        env_file: Option<PathBuf>,
        #[arg(last = true, help = "Arguments passed to the managed binary after --")]
        args: Vec<String>,
    },
    #[command(
        about = "Stop a supervised app",
        long_about = "Stop a managed app by name. aynur sends SIGTERM to the app process group, waits briefly, then sends SIGKILL if it is still running.",
        after_help = "Example:\n  aynur stop api"
    )]
    Stop {
        #[arg(help = "App name")]
        name: String,
    },
    #[command(
        about = "Restart a supervised app",
        long_about = "Stop and start a managed app with its saved binary path, args, cwd, and environment.",
        after_help = "Example:\n  aynur restart api"
    )]
    Restart {
        #[arg(help = "App name")]
        name: String,
    },
    #[command(
        about = "Reload an app and refresh its environment",
        long_about = "Reload a managed app with a controlled restart. By default, aynur uses the saved environment. With --update-env, it refreshes the saved environment from the current shell and the configured --env-file before restarting.",
        after_help = "Examples:\n  aynur reload api\n  aynur reload api --update-env"
    )]
    Reload {
        #[arg(help = "App name")]
        name: String,
        #[arg(
            long = "update-env",
            help = "Refresh environment before restarting the app"
        )]
        update_env: bool,
    },
    #[command(
        about = "List supervised apps",
        long_about = "Show app name, pid, status, restart count, uptime, and binary path.",
        after_help = "Example:\n  aynur list"
    )]
    List,
    #[command(
        about = "Alias for list",
        long_about = "Show the same process table as `aynur list`.",
        after_help = "Example:\n  aynur status"
    )]
    Status,
    #[command(
        about = "Print stdout and stderr logs for an app",
        long_about = "Print the app stdout and stderr log files from AYNUR_HOME/logs.",
        after_help = "Example:\n  aynur logs api"
    )]
    Logs {
        #[arg(help = "App name")]
        name: String,
    },
    #[command(
        about = "Stop an app and remove its saved config",
        long_about = "Stop the managed app if it is running, then delete its saved app config from AYNUR_HOME/apps.",
        after_help = "Example:\n  aynur delete api"
    )]
    Delete {
        #[arg(help = "App name")]
        name: String,
    },
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

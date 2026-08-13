# aynur

`aynur` is a minimal pm2-like process guardian for local Rust binaries.

It starts a small local daemon, keeps configured processes running, redirects stdout/stderr to log files, and exposes familiar commands such as `start`, `stop`, `restart`, `reload`, `list`, and `logs`.

## Install

```sh
cargo install aynur
```

## Quick Start

Start a binary:

```sh
aynur start ./target/release/gateway
```

By default, the app name is the binary file name. For example, the command above registers the app as `gateway`.

Set an explicit name:

```sh
aynur start ./target/release/gateway -n api
```

Pass arguments to the managed binary after `--`:

```sh
aynur start ./target/release/gateway -n api -- --port 3000
```

List apps:

```sh
aynur list
```

Stop, restart, reload, and delete:

```sh
aynur stop api
aynur restart api
aynur reload api
aynur reload api --update-env
aynur delete api
```

View logs:

```sh
aynur logs api
```

The command continues printing new stdout and stderr output until interrupted with `Ctrl+C`.

Clear logs for an app:

```sh
aynur flush api
```

## Commands

```sh
aynur start <binary> [-n <name>] [--cwd <path>] [--env-file <path>] [-- <args>...]
aynur stop <name>
aynur restart <name>
aynur reload <name> [--update-env]
aynur list
aynur status
aynur logs <name>
aynur flush <name>
aynur delete <name>
```

`status` is an alias for `list`.

## Environment

State is stored in `~/.aynur` by default:

```text
~/.aynur/apps/
~/.aynur/logs/
~/.aynur/daemon.pid
~/.aynur/daemon.sock
```

Use `AYNUR_HOME` to choose another state directory:

```sh
AYNUR_HOME=/srv/aynur aynur list
```

At `start` time, aynur captures the current process environment. If `--env-file` is provided, values from that file are merged over the current environment.

```sh
aynur start ./target/release/gateway -n api --env-file .env
```

Reload with the saved environment:

```sh
aynur reload api
```

Refresh the saved environment from the current shell and the configured env file before restarting:

```sh
aynur reload api --update-env
```

## Behavior

- Linux/macOS only.
- Managed processes run in their own process group.
- `stop` sends `SIGTERM`, waits briefly, then sends `SIGKILL`.
- Unexpected exits are restarted automatically.
- Fast crash loops are marked as `errored` instead of restarting forever.
- stdout and stderr are appended to separate log files.

## Scope

`aynur` intentionally keeps the first version small. It does not implement pm2 ecosystem files, cluster mode, startup integration, remote management, dashboards, or Windows support.

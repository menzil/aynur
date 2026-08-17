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

Save the current online app snapshot:

```sh
aynur save
```

Only apps that are `online` when `aynur save` runs are restored the next time the daemon starts. `start`, `stop`, and `delete` do not update this snapshot automatically; run `aynur save` again after changing the desired restart set.

Saving the snapshot does not install system startup. Run `aynur startup` once to restore the daemon automatically after the next user login.

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

Install user-level startup for the daemon:

```sh
aynur startup
```

Remove user-level startup:

```sh
aynur unstartup
```

## Commands

```sh
aynur start <binary> [-n <name>] [--cwd <path>] [--env-file <path>] [-- <args>...]
aynur stop <name>
aynur restart <name>
aynur reload <name> [--update-env]
aynur list
aynur save
aynur logs <name>
aynur flush <name>
aynur delete <name>
aynur startup
aynur unstartup
```

## Environment

State is stored in `~/.aynur` by default:

```text
~/.aynur/apps/
~/.aynur/logs/
~/.aynur/saved.json
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

## Startup

`aynur startup` installs the daemon as a user-level startup service using the current `aynur` executable path and current `AYNUR_HOME`.

On Linux, aynur writes `~/.config/systemd/user/aynur.service`, runs `systemctl --user daemon-reload`, and enables the service with `systemctl --user enable`. The service starts at the next user session; the command prints a `loginctl enable-linger <user>` hint for boot-before-login behavior, but does not run it with sudo.

On macOS, aynur writes `~/Library/LaunchAgents/cn.aynur.daemon.plist` and enables the LaunchAgent. It starts at the next user login, without interrupting a daemon that is already running.

If `aynur startup` is run from a debug build such as `target/debug/aynur`, the startup service uses that resolved executable path. Install the release binary first when that is the path you want restored after reboot.

## Behavior

- Linux/macOS only.
- Managed processes run in their own process group.
- `stop` sends `SIGTERM`, waits briefly, then sends `SIGKILL`.
- Unexpected exits are restarted automatically.
- Fast crash loops are marked as `errored` instead of restarting forever.
- stdout and stderr are appended to separate log files.
- The daemon restores apps listed in `~/.aynur/saved.json` when it starts.

## Scope

`aynur` intentionally keeps the first version small. It does not implement pm2 ecosystem files, cluster mode, remote management, dashboards, or Windows support.

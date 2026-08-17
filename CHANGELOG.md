# Changelog

## [0.3.0] - 2026-08-17

### Added

- Added `aynur save` to persist the online app set for daemon restart recovery.
- Added `aynur startup` and `aynur unstartup` for user-level systemd and launchd startup configuration.

### Changed

- The daemon restores saved apps at startup and reports restore failures in `daemon.err.log`.
- Daemon IPC connections now have bounded read and write timeouts, and malformed connections no longer terminate the daemon.
- Startup configuration is installed for the next user session without starting a second daemon in the current session.

## [0.2.1] - 2026-08-13

### Changed

- Removed the `aynur status` command; use `aynur list` instead.
- Added a `memory` column to `aynur list` showing resident memory for each running main process.

## [0.2.0] - 2026-08-13

### Changed

- `aynur logs <name>` now follows stdout and stderr until interrupted with `Ctrl+C`, including after the logs are cleared. Scripts that need a one-time snapshot should read the app's log files from `AYNUR_HOME/logs` directly.

### Added

- Added `aynur flush <name>` to clear one app's stdout and stderr logs without interrupting the managed process.

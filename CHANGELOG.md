# Changelog

## [0.2.1] - 2026-08-13

### Changed

- Removed the `aynur status` command; use `aynur list` instead.
- Added a `memory` column to `aynur list` showing resident memory for each running main process.

## [0.2.0] - 2026-08-13

### Changed

- `aynur logs <name>` now follows stdout and stderr until interrupted with `Ctrl+C`, including after the logs are cleared. Scripts that need a one-time snapshot should read the app's log files from `AYNUR_HOME/logs` directly.

### Added

- Added `aynur flush <name>` to clear one app's stdout and stderr logs without interrupting the managed process.

# Changelog

## [0.2.0] - 2026-08-13

### Changed

- `aynur logs <name>` now follows stdout and stderr until interrupted with `Ctrl+C`, including after the logs are cleared. Scripts that need a one-time snapshot should read the app's log files from `AYNUR_HOME/logs` directly.

### Added

- Added `aynur flush <name>` to clear one app's stdout and stderr logs without interrupting the managed process.

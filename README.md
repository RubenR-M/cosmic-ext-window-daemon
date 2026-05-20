# cosmic-ext-window-daemon

A systemd-managed Wayland daemon for the COSMIC desktop that automatically routes new application windows onto workspaces.

## What it does

`cosmic-ext-window-daemon` listens for new toplevel windows via the COSMIC compositor's extension protocols (`zcosmic_toplevel_info_v1`, `zcosmic_toplevel_manager_v1`, `ext_workspace_manager_v1`) and moves each window to an appropriate workspace based on placement policy. It exits immediately on misconfiguration (wrong env vars, missing protocols), supervised-reconnects on compositor crash using exponential backoff, and runs as a systemd user unit tied to `graphical-session.target`.

## Quickstart

```sh
# Clone
git clone https://github.com/RubenR-M/cosmic-ext-window-daemon
cd cosmic-ext-window-daemon

# Build + install (writes ~/.local/bin/cosmic-ext-window-daemon + service file)
make install

# Enable the user service (daemon-reload already done by make install)
systemctl --user enable --now cosmic-ext-window-daemon.service

# Confirm
systemctl --user status cosmic-ext-window-daemon.service
journalctl --user -fu cosmic-ext-window-daemon.service
```

## Uninstall

```sh
make uninstall
```

`make uninstall` stops and disables the service, then removes the binary and the unit file.

## Requirements

| Requirement | Why |
|-------------|-----|
| COSMIC desktop (cosmic-comp >= 1.0.0-alpha.4) | Daemon requires `zcosmic_toplevel_info_v1`, `zcosmic_toplevel_manager_v1`, and `ext_workspace_manager_v1` protocols |
| systemd user session | Daemon ships as a systemd user unit with `PartOf=graphical-session.target` |
| Rust 1.95.0 (pinned via `rust-toolchain.toml`) | Build-only requirement |
| `make` | Install/uninstall helper |

## Configuration

All configuration is via environment variables, read at startup. Unknown or malformed values exit 1 immediately.

| Variable | Default | Description |
|----------|---------|-------------|
| `WAYLAND_DISPLAY` | (inherited from session) | Wayland compositor socket. **Required**: if neither this nor `WAYLAND_SOCKET` is set to a non-empty value, daemon exits 1 (FR-001). |
| `RUST_LOG` | `info` | `tracing-subscriber` filter. Use `cosmic_ext_window_daemon=debug` for verbose output. |
| `WORKSPACE_MODE` | `next-free` | `next-free` — place on the first empty workspace; `new-each` — request a new workspace per toplevel (degrades to `next-free` when unsupported); `same` — do not move the window. |
| `SWITCH_TO_WORKSPACE` | `0` | If `1`, activate the destination workspace after placing the toplevel. |
| `MAXIMIZE` | `0` | If `1`, maximize the toplevel after placing it. |
| `SWITCH_VERIFY_TIMEOUT_MS` | `250` | Workspace activation confirmation timeout in milliseconds. `0` disables the verification timer. |
| `WORKSPACE_OUTPUT` | (none) | Override: name of the output whose workspace group receives all toplevels (e.g. `DP-1`). Falls back to per-toplevel output selection when the named output is absent; emits WARN at most once. |
| `EXCLUDED_APP_IDS` | (none) | Comma-separated `app_id` values to skip (case-sensitive, e.g. `org.kde.dolphin,foot`). |
| `EXCLUDED_TITLE_REGEX` | (none) | Regular expression matched against toplevel titles. Toplevels whose title matches are skipped. Invalid regex exits 1 at startup. |

### Drop-in override pattern

Use `systemctl --user edit` to set variables without modifying the installed unit file:

```sh
systemctl --user edit cosmic-ext-window-daemon.service
```

This creates `~/.config/systemd/user/cosmic-ext-window-daemon.service.d/override.conf`. Example content:

```ini
[Service]
Environment=SWITCH_TO_WORKSPACE=1
Environment=MAXIMIZE=1
Environment=EXCLUDED_APP_IDS=foot,org.kde.dolphin
```

Then reload:

```sh
systemctl --user restart cosmic-ext-window-daemon.service
```

The unit's `PassEnvironment=` whitelists `RUST_LOG`, `WAYLAND_DISPLAY`, and `XDG_RUNTIME_DIR` from the session environment. All other variables (`SWITCH_TO_WORKSPACE`, `MAXIMIZE`, `WORKSPACE_MODE`, etc.) must be set via drop-in `Environment=` directives.

## Operator behavior

### Reconnect supervisor (FR-021)

On compositor disconnect, the daemon drops all Wayland state and retries with a bounded exponential backoff ladder:

**1s → 2s → 5s → 10s → 30s** (capped at 30s, retries forever until SIGTERM)

After a successful reconnect the backoff cursor resets to 1s.

### Signal handling (FR-022)

`SIGTERM` and `SIGINT` trigger clean shutdown within ~50ms even during a backoff sleep. The systemd unit declares `SuccessExitStatus=SIGTERM SIGINT` so `systemctl --user stop` reports `Result=success`.

### Fail-fast diagnostics (FR-001, FR-002)

| Condition | Exit code | Log message |
|-----------|-----------|-------------|
| No Wayland session env vars | 1 | `neither WAYLAND_DISPLAY nor WAYLAND_SOCKET is set to a non-empty value; cosmic-ext-window-daemon requires a Wayland session` |
| Non-COSMIC compositor (missing protocol) | 1 | `cosmic toplevel-info extension (zcosmic_toplevel_info_v1) not advertised by the compositor...` |
| Config parse error (e.g., bad regex) | 1 | error message names the variable and the bad value |

## Troubleshooting

The daemon writes all output to the systemd journal (`StandardOutput=journal` / `StandardError=journal`). Use `journalctl --user -u cosmic-ext-window-daemon.service` (add `-f` to follow) to inspect.

### Daemon exits immediately

Run `journalctl --user -u cosmic-ext-window-daemon.service -n 50`. The fail-fast table above maps diagnostics to causes.

### `cargo install` does not take effect

The unit's `ExecStart=%h/.local/bin/cosmic-ext-window-daemon` points to `~/.local/bin`. `cargo install --path .` writes to `~/.cargo/bin`. **Use `make install`**; it copies the release binary to `~/.local/bin/`.

### Windows not placing as expected

Check `journalctl --user -fu cosmic-ext-window-daemon.service`. The daemon logs `toplevel placed on workspace app_id=... workspace_id=...` per placement. If that line is absent, inspect:

- Is the app in `EXCLUDED_APP_IDS` or matched by `EXCLUDED_TITLE_REGEX`?
- Does the toplevel have any output advertised? (Skipped with WARN if not.)
- Is the `zcosmic_toplevel_info_v1` protocol present? (Daemon exits 1 at startup if not.)

### Verifying the systemd unit file

Run after `make install` (the binary path in `ExecStart` must exist for `systemd-analyze verify` to succeed):

```sh
make verify-unit   # runs: systemd-analyze verify contrib/cosmic-ext-window-daemon.service
```

## Development

```sh
# Build
cargo build --release --locked

# Full test suite — 101 effective tests
# (100 unit tests + 1 trybuild compile_fail integration test
#  with 3 sub-cases that enforce the D15 Layer 1 commit gate)
cargo test --locked

# Lint (D15 Layer 2 gate — must be clean before pushing)
cargo clippy --all-targets -- -D warnings
```

### Architecture overview

| File | Responsibility |
|------|----------------|
| `src/config.rs` | Pure config parsing from env (21 tests) |
| `src/placement.rs` | Pure placement decision engine (24 tests) |
| `src/verify.rs` | Two-tier workspace activation verifier (17 tests) |
| `src/reconnect.rs` | `BackoffState` + `Supervisor<F>` outer reconnect loop (13 tests) |
| `src/app.rs` | `connect_and_run`: one Wayland session; `check_required_globals` (D8); `walk_for_io_errno` + `map_calloop_error` (A22) |
| `src/runtime.rs` | Placement pipeline integration |
| `src/wayland/` | Smithay handlers + cosmic-client-toolkit delegate macros |
| `src/wayland/workspace.rs` | `WorkspaceManager::transaction` — the only authorized call site for raw workspace protocol methods (D15 Layer 1) |
| `contrib/cosmic-ext-window-daemon.service` | systemd user unit |
| `Makefile` | `install` / `uninstall` / `enable` / `disable` / `verify-unit` |

The two-layer D15 enforcement (`WorkspaceManager::transaction` + `clippy::disallowed_methods`) prevents raw `ExtWorkspaceHandleV1::activate()` calls outside `wayland::workspace`. The disallowed paths are declared in `clippy.toml`; the CI gate (`cargo clippy -- -D warnings`) blocks merges that bypass the boundary.

## License

GPL-3.0-only. See [LICENSE](LICENSE).

## Acknowledgements

See [ACKNOWLEDGEMENTS.md](ACKNOWLEDGEMENTS.md).

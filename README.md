# cosmic-ext-window-daemon

A long-running Wayland daemon for the [COSMIC desktop](https://github.com/pop-os/cosmic-epoch) (System76) that automatically places new application windows onto workspaces.

## Status

**Project bootstrap in progress.** The full implementation lands across a chained-PR series; each PR is reviewable on its own. Roadmap:

| PR | Branch | Scope |
|----|--------|-------|
| 1a | `phase-0-bootstrap` → `main` | Toolchain pin, `Cargo.toml`, `clippy.toml`, module skeleton. |
| 1b | `phase-1-pure-logic` → `phase-0-bootstrap` | Pure-logic modules (`config`, `placement`, `verify`, `reconnect`) with unit tests. |
| 2  | `phase-2-d15-enforcement` → `main` | Two-layer commit enforcement for the workspace-transaction type (compile-time inside the module + CI lint gate codebase-wide). Includes the test PR that proves the gate blocks merge. |
| 3  | `phase-3-wayland` → `main` | COSMIC protocol integration: `toplevel-info`, `toplevel-management`, `workspace`, `calloop` event loop. First runnable binary. |
| 4  | `phase-4-daemon` → `main` | `main` entrypoint, systemd `--user` unit, install instructions. |
| 5  | `phase-5-review-and-tests` → `main` | Self-review + manual tests against a live COSMIC compositor. |
| 6  | `phase-6-docs-license` → `main` | Final README and `LICENSE` (GPL-3.0-only). |

## License

**GPL-3.0-only.** This is a legal fact, not a preference: the binary links against [`cosmic-protocols`](https://github.com/pop-os/cosmic-protocols) and [`cosmic-client-toolkit`](https://github.com/pop-os/cosmic-protocols/tree/main/client-toolkit), both GPL-3.0-only, and any derivative work inherits that license. The full `LICENSE` file arrives in PR 6.

## Acknowledgement

Conceptually inspired by [`lapause/cosmic-ext-window-helper`](https://github.com/lapause/cosmic-ext-window-helper) (Python, MIT). This project shares no code with that one — the acknowledgement is recognition, not a license basis.

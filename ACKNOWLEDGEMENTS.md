# Acknowledgements

cosmic-ext-window-daemon stands on the shoulders of:

| Project | What it provides |
|---------|------------------|
| [pop-os/cosmic-protocols](https://github.com/pop-os/cosmic-protocols) | Wayland protocol definitions for the COSMIC extension (toplevel-info, toplevel-management, workspace). Pinned at `=0.2.0`. |
| [pop-os/cosmic-client-toolkit](https://github.com/pop-os/cosmic-protocols/tree/160b086abe03cd34a8a375d7fbe47b24308d1f38/client-toolkit) (commit 160b086 — v0.2.0 release on crates.io, 2026-01-08) | Rust toolkit for COSMIC client protocols (`ToplevelInfoState`, `ToplevelManagerState`, `WorkspaceState`). Pinned at `=0.2.0`. |
| [smithay/smithay-client-toolkit](https://github.com/Smithay/client-toolkit) | Generic Wayland client toolkit (registry, output, seat handlers). |
| [smithay/calloop](https://github.com/Smithay/calloop) | Event loop crate. The reconnect supervisor wraps `calloop::EventLoop`. |
| [Smithay/calloop-wayland-source](https://github.com/Smithay/calloop-wayland-source) | Bridge between `calloop` and `wayland-client`. |
| [wayland-rs](https://github.com/Smithay/wayland-rs) | `wayland-client` + `wayland-protocols`. |
| [tracing](https://github.com/tokio-rs/tracing) | Structured logging via `tracing` + `tracing-subscriber`. |
| [nix](https://github.com/nix-rust/nix) | `sigaction` for the process-level shutdown handler (FR-022 / A15). |

The protocol source-of-truth constraint (Proposal §11, Constraint E) anchors crate selection: `docs.rs` is dead for `cosmic-protocols 0.2.0` and `cosmic-client-toolkit 0.2.0`; the authoritative sources are the upstream tarballs on `crates.io` and the documentation at `pop-os.github.io`.

## License compatibility

All upstream crates listed above are MIT- or Apache-2.0-licensed and are compatible with GPL-3.0-only redistribution. Bundled binaries of `cosmic-ext-window-daemon` are GPL-3.0-only.

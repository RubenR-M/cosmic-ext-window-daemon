// cosmic-ext-window-daemon
// SPDX-License-Identifier: GPL-3.0-only
//
// D15 Layer 2: the #![deny(clippy::disallowed_methods)] attribute is added in PR 2 (T-010)
// once the CI gate is in place to enforce it. The disallowed-methods list in clippy.toml
// is already active at warn level — that wiring is established in PR 1a.

mod config;
mod placement;
mod reconnect;
mod state;
mod verify;
mod wayland;

fn main() {
    todo!("implement main entrypoint — T-021")
}

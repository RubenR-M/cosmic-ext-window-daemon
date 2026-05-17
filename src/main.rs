// cosmic-ext-window-daemon — binary entry point.
// SPDX-License-Identifier: GPL-3.0-only
//
// D15 Layer 2: `#![deny(clippy::disallowed_methods)]` is added in T-010
// alongside the CI gate (T-011) and the SC-018 test PR (T-012). The
// `#![allow(clippy::disallowed_methods)]` inner attribute on
// `wayland::workspace` in the library crate is the sole authorized exception.

fn main() {
    todo!("implement main entrypoint — T-021")
}

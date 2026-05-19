# Makefile — build and install targets for cosmic-ext-window-daemon.
# SPDX-License-Identifier: GPL-3.0-only
#
# Usage:
#   make install       — build release binary and install service + binary
#   make uninstall     — remove binary and service; disable and stop the unit
#   make enable        — enable + start the service (after install)
#   make disable       — stop + disable the service

CARGO       ?= cargo
INSTALL     ?= install
PREFIX      ?= $(HOME)/.local
BINDIR      ?= $(PREFIX)/bin
SYSTEMD_DIR ?= $(HOME)/.config/systemd/user
SERVICE     := cosmic-ext-window-daemon.service
BINARY      := cosmic-ext-window-daemon

.PHONY: build install uninstall enable disable verify-unit

build:
	$(CARGO) build --release

install: build
	@mkdir -p $(BINDIR) $(SYSTEMD_DIR)
	$(INSTALL) -m 755 target/release/$(BINARY) $(BINDIR)/$(BINARY)
	$(INSTALL) -m 644 contrib/$(SERVICE) $(SYSTEMD_DIR)/$(SERVICE)
	systemctl --user daemon-reload
	@echo "Installed. Run 'make enable' to start the service, or:"
	@echo "  systemctl --user enable --now $(SERVICE)"

uninstall:
	-systemctl --user stop $(SERVICE)
	-systemctl --user disable $(SERVICE)
	rm -f $(BINDIR)/$(BINARY)
	rm -f $(SYSTEMD_DIR)/$(SERVICE)
	systemctl --user daemon-reload
	@echo "Uninstalled."

enable:
	systemctl --user enable --now $(SERVICE)

disable:
	systemctl --user stop $(SERVICE)
	systemctl --user disable $(SERVICE)

# Lint the systemd unit file. Run locally before pushing.
# systemd-analyze verify also checks that ExecStart points to an existing
# executable, so it cannot run in CI where the binary is not installed.
# Run locally: `make verify-unit` — exit 0 means the unit file is clean.
verify-unit:
	systemd-analyze verify contrib/$(SERVICE)

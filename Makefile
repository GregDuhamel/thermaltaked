.PHONY: build check install uninstall install-udev uninstall-udev install-unit uninstall-unit install-config clean

# The binary goes system-wide, beside the other daemons of this machine, and
# the unit that runs it is a user unit. Targets that write under PREFIX or
# /etc need sudo; targets that manage your user units or your configuration
# must run as yourself. Cargo never runs under sudo: `build` is yours, and
# `install` only copies what `build` left in target/.
PREFIX         ?= /usr/local
BINDIR         := $(PREFIX)/bin
BIN            := $(BINDIR)/thermaltaked
UNIT_DIR       := $(HOME)/.config/systemd/user
UNIT           := $(UNIT_DIR)/thermaltaked.service
CONF_DIR       := $(HOME)/.config/thermaltaked
CONF           := $(CONF_DIR)/config.toml
UDEV_RULES_DIR := /etc/udev/rules.d
UDEV_RULES     := $(UDEV_RULES_DIR)/70-thermaltake-lcd.rules

# The unit hard-codes /usr/local/bin; follow PREFIX when it differs.
install_unit = sed 's|/usr/local/bin|$(BINDIR)|g' $(1) | install -Dm 0644 /dev/stdin $(2)

require_root = @if [ "$$(id -u)" != 0 ]; then echo "✗ '$@' writes to the system — run it with sudo"; exit 1; fi
require_user = @if [ "$$(id -u)" = 0 ]; then echo "✗ '$@' is yours, not root's — run it without sudo"; exit 1; fi
require_bin  = @if [ ! -x $(BIN) ]; then echo "✗ $(BIN) not found — run 'make build && sudo make install' first"; exit 1; fi

build:
	cargo build --release --locked

# What CI runs, locally.
check:
	cargo fmt --all -- --check
	cargo check --locked --all-targets
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked --all-targets
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items

# No dependency on `build`: this runs under sudo, and cargo must not.
install:
	$(require_root)
	@if [ ! -f target/release/thermaltaked ]; then echo "✗ target/release/thermaltaked not found — run 'make build' first (without sudo)"; exit 1; fi
	install -Dm 0755 target/release/thermaltaked $(BIN)
	@echo "✓ installed: $(BIN)"
	@echo "  Then, as yourself: make install-unit   (and 'sudo make install-udev' the first time)"

uninstall:
	$(require_root)
	rm -f $(BIN)
	@echo "✓ removed: $(BIN) (the udev rule and your unit are kept: uninstall-udev, uninstall-unit)"

# The udev rule hands the panel's hidraw nodes to the logged-in user. The
# trigger re-applies it to the panel if it is plugged in.
install-udev:
	$(require_root)
	install -Dm 0644 udev/70-thermaltake-lcd.rules $(UDEV_RULES)
	udevadm control --reload-rules
	udevadm trigger --subsystem-match=hidraw
	@echo "✓ udev rule installed: $(UDEV_RULES)"
	@echo "  Verify with: thermaltaked info"

uninstall-udev:
	$(require_root)
	rm -f $(UDEV_RULES)
	udevadm control --reload-rules
	udevadm trigger --subsystem-match=hidraw
	@echo "✓ udev rule removed"

# The user unit, started with the graphical session. Run as yourself. A
# running service is restarted, so this is also how a new binary is taken up.
install-unit:
	$(require_user)
	$(require_bin)
	$(call install_unit,systemd/thermaltaked.service,$(UNIT))
	systemctl --user daemon-reload
	@if systemctl --user is-active --quiet thermaltaked.service; then \
		systemctl --user restart thermaltaked.service; \
		echo "✓ thermaltaked.service restarted"; \
	else \
		systemctl --user enable --now thermaltaked.service; \
		echo "✓ thermaltaked.service enabled and started"; \
	fi
	@echo "  Logs: journalctl --user -u thermaltaked"

uninstall-unit:
	$(require_user)
	-systemctl --user disable --now thermaltaked.service
	rm -f $(UNIT)
	systemctl --user daemon-reload
	@echo "✓ thermaltaked.service removed"

# The example configuration, as a starting point; an existing one is kept.
install-config:
	$(require_user)
	@if [ -f $(CONF) ]; then \
		echo "  kept: $(CONF) (the current example is config.example.toml)"; \
	else \
		install -Dm 0644 config.example.toml $(CONF); \
		echo "✓ installed: $(CONF)"; \
	fi

clean:
	cargo clean

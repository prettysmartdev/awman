BINARY     := amux
INSTALL_PATH ?= /usr/local/bin
# Honour CARGO_TARGET_DIR if set in the environment. Falls back to the cargo
# default of `target`.
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)

.PHONY: all build install test test-fast test-full clean release architecture-lint pre-push

all: build

build:
	cargo build --release

install: build
	install -m 755 $(TARGET_DIR)/release/$(BINARY) $(INSTALL_PATH)/$(BINARY)

test:
	cargo test --quiet

test-fast:
	cargo test --quiet -- --skip docker --skip real_git --skip real_network

test-full:
	cargo test --quiet

architecture-lint:
	@bash tools/architecture-lint.sh

pre-push: architecture-lint
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test --quiet

clean:
	cargo clean

release:
	@if [ -z "$(VERSION)" ]; then \
		echo "Usage: make release VERSION=vx.y.z"; \
		exit 1; \
	fi
	@bash scripts/release.sh "$(VERSION)"

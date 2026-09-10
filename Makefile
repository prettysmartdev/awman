BINARY     := awman
INSTALL_PATH ?= /usr/local/bin
# Honour CARGO_TARGET_DIR if set in the environment. Falls back to the cargo
# default of `target`.
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
# Keep test fixtures out of the shared host /tmp. Long-running or concurrent
# test jobs can exhaust /tmp's directory-link limit before any test body runs.
# Do not use the workspace/target directory here: local Git remotes in the
# smoke tests need the native filesystem's atomic object writes.
AWMAN_TEST_TMPROOT ?= /var/tmp/test-fixtures

.PHONY: all build install test test-fast test-full clean release architecture-lint pre-push

all: build

build:
	cargo build --release

install: build
	install -m 755 $(TARGET_DIR)/release/$(BINARY) $(INSTALL_PATH)/$(BINARY)

# A failed `mktemp` must stop the run, not fall through with an empty TMPDIR.
# Rust's `std::env::temp_dir()` honours TMPDIR verbatim, so TMPDIR="" makes
# every `tempfile::tempdir()` a *relative* path: the fixtures land in the
# checked-out repo instead of the fixture root, polluting the tree the teardown
# then commits, and crawling when the workspace is a bind mount rather than the
# container's own filesystem.
test:
	@set -e; \
		mkdir -p "$(AWMAN_TEST_TMPROOT)"; \
		awman_test_tmpdir="$$(mktemp -d "$(AWMAN_TEST_TMPROOT)/test-run.XXXXXX")"; \
		if [ -z "$$awman_test_tmpdir" ] || [ ! -d "$$awman_test_tmpdir" ]; then \
			echo "make test: no fixture directory under $(AWMAN_TEST_TMPROOT)" >&2; \
			exit 1; \
		fi; \
		trap 'rm -rf "$$awman_test_tmpdir"' EXIT; \
		TMPDIR="$$awman_test_tmpdir" cargo test --quiet

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

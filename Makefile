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

.PHONY: all build install test test-fast test-full test-builtin payloads clean release architecture-lint pre-push docs-reference

all: build

build:
	@if bash tools/msb-payloads/verify.sh "$$(rustc -vV | sed -n 's/^host: //p')" >/dev/null 2>&1; then \
		cargo build --release --features builtin-runtime; \
	else \
		echo 'Builtin payloads unavailable for this host; building existing backends'; \
		cargo build --release; \
	fi

payloads:
	bash tools/msb-payloads/fetch.sh "$$(rustc -vV | sed -n 's/^host: //p')"
	@case "$$(uname -s)" in Linux) bash third_party/native/libcap-ng/build.sh "$$(rustc -vV | sed -n 's/^host: //p')";; esac

install: build
	install -m 755 $(TARGET_DIR)/release/$(BINARY) $(INSTALL_PATH)/$(BINARY)

# Every test target runs through tools/isolated-test.sh, which keeps the suite
# off the developer's own machine: a throwaway HOME, no global git config, no
# inherited awman variables, stdin from /dev/null, a per-run TMPDIR under
# AWMAN_TEST_TMPROOT, and AWMAN_TEST_ISOLATION=1 so awman itself uses an
# in-memory keychain and clipboard, never launchd or systemd, and no network
# beyond loopback. See the script for why each one matters.
#
# The real container and sandbox CLIs are off too, unless opted into with
# AWMAN_TEST_DOCKER=1, AWMAN_TEST_APPLE_CONTAINER=1 or AWMAN_TEST_SBX=1: their
# tests build, run and remove images and containers in your own daemon.
# `test-full` (the CI Docker job) opts into Docker; `test` and `test-fast`
# never touch it.
test:
	@AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh --quiet

test-fast:
	@AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh --quiet -- --skip docker --skip real_git --skip real_network --skip builtin_hw

# Only the targets that exercise the builtin runtime: every test executable links the
# whole embedded VM stack (hundreds of MB each in debug), so building all of them
# with the feature is needlessly slow and can exhaust memory or disk on CI runners.
# `--examples` builds the real-guest driver used by the builtin_hw_* tests.
test-builtin:
	@AWMAN_TEST_BUILTIN=1 AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh --features builtin-runtime --lib --bins --examples --test builtin_runtime --test oci_import --test data_layer --quiet

test-full:
	@AWMAN_TEST_DOCKER=1 AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh --quiet

architecture-lint:
	@bash tools/architecture-lint.sh

# Rewrite docs/14-command-reference.md from CommandCatalogue. The doc is
# generated, never hand-edited; `markdown_reference_matches_committed_docs`
# fails whenever a catalogue change leaves it stale, and this is the fix.
docs-reference:
	cargo test --lib \
		command::dispatch::projections::markdown::tests::regenerate_command_reference \
		-- --ignored

# The test step goes through the `test` target rather than calling `cargo
# test` directly, so the pre-push gate gets the same per-run fixture root.
# Running the suite against a shared /tmp is what the `test` target was
# hardened against, and pre-push is the one that runs in a container where
# a work item has already left tens of thousands of fixture directories
# behind: every test body after the handful that need no temp directory
# then dies on, or crawls against, a /tmp that cannot take another one.
pre-push: architecture-lint
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	$(MAKE) --no-print-directory test

clean:
	cargo clean

release:
	@if [ -z "$(VERSION)" ]; then \
		echo "Usage: make release VERSION=vx.y.z"; \
		exit 1; \
	fi
	@bash scripts/release.sh "$(VERSION)"

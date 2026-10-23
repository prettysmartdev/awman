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

.PHONY: all build sign-macos install test test-fast test-full test-builtin test-builtin-hw payloads payloads-verify clean release architecture-lint pre-push docs-reference reproducible-build-audit native-builtin-ci

all: build

# The workspace has one distribution executable (awman-cli's awman target).
# awman and awman-host-shims are libraries, including the private re-exec worker.

# On Apple Silicon the experimental builtin runtime is included when its
# payload verifies, and the binary is then ad-hoc signed with the hypervisor
# entitlement (tools/macos-sign.sh): macOS refuses to start a VM otherwise.
# Other hosts build the existing backends only; the builtin runtime is released
# for macOS on Apple Silicon only (WI 0124 tracks Linux).
build:
	@host="$$(rustc -vV | sed -n 's/^host: //p')"; \
	if [ "$$host" = aarch64-apple-darwin ] && bash tools/msb-payloads/verify.sh "$$host" >/dev/null 2>&1; then \
		cargo build --workspace --bins --release --features builtin-runtime && \
		bash tools/macos-sign.sh $(TARGET_DIR)/release/$(BINARY); \
	else \
		echo 'Builtin runtime not included for this host; building existing backends'; \
		cargo build --workspace --bins --release; \
	fi

# Re-sign an existing build, e.g. after stripping or copying it elsewhere:
# `make sign-macos BIN=/usr/local/bin/awman`.
sign-macos:
	bash tools/macos-sign.sh "$(or $(BIN),$(TARGET_DIR)/release/$(BINARY))"

payloads:
	bash tools/msb-payloads/fetch.sh "$$(rustc -vV | sed -n 's/^host: //p')"

# Native, one-time promotion of a new payload pin: fetches the payload, writes
# a candidate verified record, runs the hardware/clean-checkout/release-artifact
# gates, and keeps the record only if all pass. Needs a clean checkout, HVF or
# KVM, and a running Docker or Apple Containers engine for the guest fixtures.
payloads-verify:
	bash tools/msb-payloads/payloads-verify.sh "$$(rustc -vV | sed -n 's/^host: //p')"

# Explicit verification entrypoints; native hardware targets fail closed when
# KVM/HVF or the fixture is absent. `TARGET` is a Rust target triple.
reproducible-build-audit:
	bash tools/reproducible-build-audit.sh "$(or $(OUTPUT),reproducible-build-audit)"

native-builtin-ci:
	bash tools/native-builtin-ci.sh "$(TARGET)"

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
# Keep Cargo's build progress visible; only libtest's per-test output is quiet.
test:
	@AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh -- --quiet

test-fast:
	@AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh -- --quiet --skip real_git --skip real_network --skip builtin_hw

# Only the targets that exercise the builtin runtime: every test executable links the
# whole embedded VM stack (hundreds of MB each in debug), so building all of them
# with the feature is needlessly slow and can exhaust memory or disk on CI runners.
# `--examples` builds the real-guest driver used by the builtin_hw_* tests.
# On macOS the awman binary and guest drivers are signed with the hypervisor
# entitlement between building and running (tools/macos-sign-test-artifacts.sh).
BUILTIN_TEST_ARGS := --workspace --features builtin-runtime --lib --bins --examples --test builtin_runtime --test oci_import --test data_layer
test-builtin:
	@AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh $(BUILTIN_TEST_ARGS) --no-run --quiet
	@# `cargo test --examples` builds the examples as test targets; the guest
	@# drivers the hardware tests launch from target/debug/examples/ need a build.
	@cargo build --quiet --features builtin-runtime --example builtin_hw_driver --example builtin_net_driver
	@bash tools/macos-sign-test-artifacts.sh $(TARGET_DIR)/debug
	@# The old-to-new upgrade test also boots the awman that last shipped the
	@# protocol this build upgrades from. Build it from git history (cached under
	@# target/previous-builtin/) on hosts that can boot a guest, unless
	@# AWMAN_TEST_BUILTIN_PREVIOUS already names one.
	@previous="$${AWMAN_TEST_BUILTIN_PREVIOUS:-}"; \
	if [ -z "$$previous" ]; then previous=$$(bash tools/build-previous-builtin-awman.sh --if-hypervisor) || exit 1; fi; \
	if [ -n "$$previous" ]; then export AWMAN_TEST_BUILTIN_PREVIOUS="$$previous"; fi; \
	AWMAN_TEST_BUILTIN=1 AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh $(BUILTIN_TEST_ARGS) --quiet
	@AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/test-builtin-network.sh

# Everything above with real guests required, on a Mac (Apple Silicon) or a
# Linux KVM host, with no setup: fetches the payload, makes the guest fixtures
# with skopeo under Docker or Apple Containers, loads the fixture into Apple
# Containers on macOS, turns on the network, pressure and measurement suites,
# then runs the catalog compatibility check. See tools/test-builtin-hw.sh.
test-builtin-hw:
	@bash tools/test-builtin-hw.sh

test-full:
	@AWMAN_TEST_DOCKER=1 AWMAN_TEST_TMPROOT="$(AWMAN_TEST_TMPROOT)" bash tools/isolated-test.sh -- --quiet

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
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	$(MAKE) --no-print-directory test

clean:
	cargo clean

release:
	@if [ -z "$(VERSION)" ]; then \
		echo "Usage: make release VERSION=vx.y.z"; \
		exit 1; \
	fi
	@bash scripts/release.sh "$(VERSION)"

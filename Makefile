BINARY     := awman
INSTALL_PATH ?= /usr/local/bin
# Honour CARGO_TARGET_DIR if set in the environment. Falls back to the cargo
# default of `target`.
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)

.PHONY: all build sign-macos install payloads payloads-verify clean release apple-silicon-prerelease architecture-lint docs-reference

all: build

# The workspace has one distribution executable (awman-cli's awman target).
# awman and awman-host-shims are libraries, including the private re-exec worker.

# On Apple Silicon the experimental builtin runtime requires a verified
# payload, and the binary is then ad-hoc signed with the hypervisor
# entitlement (tools/macos-sign.sh): macOS refuses to start a VM otherwise.
# Other hosts build the existing backends only; the builtin runtime is released
# for macOS on Apple Silicon only (WI 0124 tracks Linux).
build:
	@host="$$(rustc -vV | sed -n 's/^host: //p')"; \
	if [ "$$host" = aarch64-apple-darwin ]; then \
		bash tools/msb-payloads/verify.sh "$$host" || { echo 'Apple Silicon builtin payload is unavailable; run make payloads before installing' >&2; exit 1; }; \
		cargo build -p awman-cli --bin $(BINARY) --release --features builtin-runtime && \
		bash tools/macos-sign.sh "$(TARGET_DIR)/release/$(BINARY)"; \
	else \
		echo 'Builtin runtime not included for this host; building existing backends'; \
		cargo build -p awman-cli --bin $(BINARY) --release; \
	fi

# Re-sign an existing build, e.g. after stripping or copying it elsewhere:
# `make sign-macos BIN=/usr/local/bin/awman`.
sign-macos:
	bash tools/macos-sign.sh "$(or $(BIN),$(TARGET_DIR)/release/$(BINARY))"

payloads:
	bash tools/msb-payloads/fetch.sh "$$(rustc -vV | sed -n 's/^host: //p')"

# Native, one-time promotion of a new payload pin: fetches the payload, writes
# a candidate verified record, runs the guest and artifact tiers, and keeps the
# record only if all pass. Needs a clean checkout, HVF or KVM, and a running
# Docker or Apple Containers engine for the guest fixtures.
payloads-verify:
	bash tools/msb-payloads/payloads-verify.sh "$$(rustc -vV | sed -n 's/^host: //p')"

# Build and sign as the current user. Only the final copy may need sudo.
install: build
	@bash tools/install.sh "$(TARGET_DIR)" "$(INSTALL_PATH)"

# ── Tests ────────────────────────────────────────────────────────────────────
# Every target below is a thin call into `cargo xtask`, which reads the suites,
# tiers and per-test timeouts from ci/gates.toml and runs them with the pinned
# cargo-nextest and the same profile CI uses (.config/nextest.toml). Test
# isolation (private HOME, XDG and AWMAN_HOME, supervised children, owned
# ports, no real engine or keychain) lives in crates/awman-test-support, so a
# direct `cargo test` still includes retained inline tests; prefer the runner below.
#
# cargo-nextest is required: `cargo xtask test install-runner` installs the
# pinned, SHA-256-verified release into target/xtask-tools/.
.PHONY: check test pre-push test-unit test-component test-boundary test-guest test-artifact gates

# Fast inner loop: every lint, then the unit tier of the crates you changed.
check:
	cargo xtask lint all
	cargo xtask test run --tier unit --changed

# unit + component for this host; parity suites exercise builtin and daemon. Never
# touches a real container engine, guest or the network.
test:
	cargo xtask test run --gate local

# The same selection as the pr gate's host cells.
pre-push:
	$(MAKE) check test

test-unit:
	cargo xtask test run --tier unit

test-component:
	cargo xtask test run --tier component

# Real engines and services, only the ones you name, e.g.
# `make test-boundary AWMAN_TEST_HOST_RESOURCES=docker=unix:///var/run/docker.sock,git-transports`.
# A test whose capability is not named reports BLOCKED; it never passes.
test-boundary:
	@echo "Host runtime grants: $(AWMAN_TEST_HOST_RESOURCES)"
	AWMAN_TEST_HOST_RESOURCES="$(AWMAN_TEST_HOST_RESOURCES)" cargo xtask test run --tier boundary

# Real guests (HVF/KVM, verified payload, pinned fixtures). Fails BLOCKED
# without a hypervisor or with an unpinned fixture.
test-guest:
	cargo xtask fixtures fetch --tier guest
	cargo xtask test run --tier guest

# A release executable: `make test-artifact ARTIFACT=/abs/path/awman`.
test-artifact:
	AWMAN_TEST_ARTIFACT="$(ARTIFACT)" cargo xtask test run --tier artifact

# Native Apple Silicon acceptance, including preparation and a portable evidence
# packet. See aspec/devops/testing.md for prerequisites and packet review.
.PHONY: validate-native
validate-native:
	CARGO_TARGET_DIR=target/native-validation-tools cargo xtask native-validation run

gates:
	cargo xtask gates check

# Tests are selected exclusively through the manifest tiers above.

# CI runs the structural rules and the hygiene suite once each.
.PHONY: architecture-rules
architecture-rules:
	@bash tools/architecture-lint.sh

architecture-lint: architecture-rules
	@cargo xtask lint hygiene

# Rewrite docs/14-command-reference.md from CommandCatalogue. The doc is
# generated, never hand-edited; the component golden test
# `command::catalogue` fails whenever a catalogue change leaves it stale.
docs-reference:
	cargo run --locked -p awman-cli --bin awman-command-reference -- docs/14-command-reference.md

# Separate from the binary gates. Missing web/edge artifacts are a hard error,
# not an implicit skip or a transport-only acceptance pass.
.PHONY: web/dev web/reinstall web/test-setup test-web test-web-check
web/dev:
	node tools/web-dev.mjs

web/reinstall:
	node tools/web-reinstall.mjs

web/test-setup:
	@bash tools/web-test-env.sh bash tools/web-test-setup.sh $(WEB_TEST_SETUP_ARGS)

test-web:
	@bash tools/web-test-env.sh $(MAKE) --no-print-directory test-web-check

# Keep every gate in one place. The wrapper only prepares executable/library
# lookup; it never substitutes for a check or treats missing tooling as a pass.
test-web-check:
	@test -f web/package.json && test -f web/edge/package.json || { echo 'WI 0159: web/edge implementation is missing' >&2; exit 1; }
	npm --prefix web run check
	npm --prefix web/edge run check
	cargo build -p awman-cli --bin $(BINARY)
	npm --prefix web/e2e ci --ignore-scripts
	npm --prefix web/e2e run check
	npm --prefix web/e2e test

clean:
	cargo clean

# ── Release ──────────────────────────────────────────────────────────────────
# Dispatch and watch the authenticated candidate rehearsal for current main.
# For a local Apple Silicon acceptance packet, use validate-native instead.
apple-silicon-prerelease:
	@cargo xtask release validate

# Release VERSION. A managed Apple Silicon runner verifies the manifest fixtures
# in its private runner directory, registers for the candidate job, and
# deregisters after the pipeline finishes.
# RELEASE_RUNNER=auto|local|none (see aspec/devops/release.md).
RELEASE_RUNNER ?= auto
release:
	@if [ -z "$(VERSION)" ]; then \
		echo "Usage: make release VERSION=vx.y.z [RELEASE_RUNNER=auto|local|none]"; \
		exit 1; \
	fi
	@RELEASE_RUNNER="$(RELEASE_RUNNER)" cargo xtask release run "$(VERSION)"

.PHONY: release-status release-abandon
release-status:
	@cargo xtask release status "$(VERSION)"

release-abandon:
	@cargo xtask release abandon "$(VERSION)"

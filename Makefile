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

gates:
	cargo xtask gates check

# Legacy integration targets kept until their replacements have shadow
# evidence (ci/gates.toml [[legacy_fallback]], aspec/devops/replacement-ledger.md).
# They still rely on the tools/isolated-test.sh wrapper for isolation; each
# target is deleted, with its row, when its cutover condition closes.
# LEGACY_DOCKER=1 also runs their real-Docker cases (the CI Docker job).
LEGACY_TEST_TMPROOT ?= /var/tmp/test-fixtures
.PHONY: legacy-test
legacy-test:
	@AWMAN_TEST_DOCKER="$(LEGACY_DOCKER)" AWMAN_TEST_TMPROOT="$(LEGACY_TEST_TMPROOT)" bash tools/isolated-test.sh -- --quiet --skip real_network --skip builtin_hw --skip latency::

# CI runs the structural rules and the hygiene suite once each.
.PHONY: architecture-rules
architecture-rules:
	@bash tools/architecture-lint.sh
	@bash tools/test-architecture-lint.sh

architecture-lint: architecture-rules
	@cargo xtask lint hygiene

# Rewrite docs/14-command-reference.md from CommandCatalogue. The doc is
# generated, never hand-edited; the component golden test
# `command::catalogue` fails whenever a catalogue change leaves it stale.
docs-reference:
	cargo run --locked -p awman-cli --bin awman-command-reference -- docs/14-command-reference.md

# Separate from the binary gates. Missing web/edge artifacts are a hard error,
# not an implicit skip or a transport-only acceptance pass.
.PHONY: web/dev web/reinstall test-web
web/dev:
	node tools/web-dev.mjs

web/reinstall:
	node tools/web-reinstall.mjs

test-web:
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
# Run every local/native Apple Silicon gate against the final clean main commit,
# including the exact signed release artifact, without creating a tag.
apple-silicon-prerelease:
	@cargo xtask release validate

# Release VERSION. On an Apple Silicon Mac with Hypervisor.framework, the
# release job's /opt/awman scripts are installed or refreshed and the Mac is
# registered as a temporary GitHub Actions runner for the Apple Silicon
# release job, then removed when the Release workflow finishes.
# RELEASE_RUNNER=auto|local|none (see scripts/release.sh).
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

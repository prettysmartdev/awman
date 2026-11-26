BINARY     := awman
INSTALL_PATH ?= /usr/local/bin
# Honour CARGO_TARGET_DIR if set in the environment. Falls back to the cargo
# default of `target`.
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)

.PHONY: all build sign-macos install payloads payloads-verify clean architecture-lint docs-reference

all: build

# The workspace has one distribution executable (awman-cli's awman target).
# awman and awman-host-shims are libraries, including the private re-exec worker.

# Released builtin hosts embed the pinned payload. Fetch missing inputs during
# the normal build, so a fresh checkout supports `make install` directly.
# Apple Silicon also requires an ad-hoc hypervisor entitlement signature.
build:
	@set -eu; host="$$(rustc -vV | sed -n 's/^host: //p')"; \
	case "$$host" in aarch64-apple-darwin|x86_64-unknown-linux-gnu) \
		bash tools/msb-payloads/verify.sh "$$host" || bash tools/msb-payloads/fetch.sh "$$host"; \
		cargo build --locked -p awman-cli --bin $(BINARY) --release --features builtin-runtime; \
		if [ "$$host" = aarch64-apple-darwin ]; then bash tools/macos-sign.sh "$(TARGET_DIR)/release/$(BINARY)"; fi ;; \
	*) \
		echo 'Builtin runtime not included for this host; building existing backends'; \
		cargo build --locked -p awman-cli --bin $(BINARY) --release ;; \
	esac

# Re-sign an existing build, e.g. after stripping or copying it elsewhere:
# `make sign-macos BIN=/usr/local/bin/awman`.
sign-macos:
	bash tools/macos-sign.sh "$(or $(BIN),$(TARGET_DIR)/release/$(BINARY))"

payloads:
	bash tools/msb-payloads/fetch.sh "$$(rustc -vV | sed -n 's/^host: //p')"

# Verify bytes against a reviewed payload record. Native validation is separate;
# this target never promotes an unverified payload record.
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
	@set -eu; digest="$$(shasum -a 256 "$(ARTIFACT)" | cut -d' ' -f1)"; \
	cargo xtask test run --tier artifact --artifact "$(ARTIFACT)" --artifact-sha256 "$$digest"

# Broad native diagnostic (guest, boundary and soak breadth) on this host: an
# Apple Silicon Mac or a Linux KVM machine. Not part of a release; its
# evidence directory is checked with `cargo xtask native-validation verify DIR`.
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
# One command starts or resumes a release from the Apple Silicon Mac: it
# dispatches the hosted release gate and asset builds, runs the two native
# checks (this Mac and LINUX_HOST over SSH), seals the draft, asks once for
# confirmation, then lands, tags, publishes and verifies. After any stop,
# run the same command again: completed work is kept. See
# aspec/devops/release.md.
#
#   make release [VERSION=vX.Y.Z] [LINUX_HOST=user@host] [YES=1]
#                [SUPERSEDE=1] [TAKEOVER=<owner>] [REHEARSE=1]
RELEASE_ARGS = $(if $(VERSION),--version $(VERSION)) $(if $(LINUX_HOST),--linux-host $(LINUX_HOST))
.PHONY: release release-status release-doctor release-abandon release-rehearse fixtures-pin
# One-time fixture preparation, separate from the timed release. Publishes a
# fixtures-<main commit> prerelease, verifies both architectures and commits pins.
fixtures-pin:
	@bash tools/fixtures/publish-and-pin.sh

release:
	@cargo xtask release run $(RELEASE_ARGS) $(if $(DECIDED_AT),--decided-at $(DECIDED_AT)) $(if $(REHEARSAL_REPO),--rehearsal-repo $(REHEARSAL_REPO)) $(if $(filter 1,$(YES)),--yes) $(if $(filter 1,$(SUPERSEDE)),--supersede) $(if $(TAKEOVER),--takeover $(TAKEOVER)) $(if $(filter 1,$(REHEARSE)),--rehearse)

# Read-only: phase table, first error and its recovery.
release-status:
	@cargo xtask release status --version "$(VERSION)" $(if $(filter 1,$(REHEARSE)),--rehearse) $(if $(REHEARSAL_REPO),--rehearsal-repo $(REHEARSAL_REPO))

# Prerequisites only (tools, GitHub capabilities, workflows, both native
# hosts, pinned inputs); changes nothing.
release-doctor:
	@cargo xtask release doctor $(RELEASE_ARGS)

# Delete this attempt's own draft, claim and private state (refused once tagged).
release-abandon:
	@cargo xtask release abandon --version "$(VERSION)" $(if $(filter 1,$(REHEARSE)),--rehearse) $(if $(REHEARSAL_REPO),--rehearsal-repo $(REHEARSAL_REPO))

# Fault-injected rehearsal of the production state machine; no network.
release-rehearse:
	@cargo xtask release rehearse $(if $(REPEAT),--repeat $(REPEAT))

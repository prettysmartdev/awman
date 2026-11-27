FROM docker.io/sbx/shell-image:latest
USER root
ARG TARGETARCH
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
# Fail the build if this base cannot execute the policy protocol.
RUN for helper in base64 sha256sum install mv rm runuser id mktemp dirname chmod env; do \
      command -v "$helper" >/dev/null || { echo "awman: required helper $helper missing" >&2; exit 1; }; \
    done; test "$(id -u agent)" = 1000
RUN <<'INSTALL_OPENCODE'
set -euo pipefail
OC_VERSION=1.19.6
case "$(uname -m)" in
  x86_64|amd64)  OC_ARCH=x64 ;;
  aarch64|arm64) OC_ARCH=arm64 ;;
  *) echo "awman: opencode ${OC_VERSION} has no Linux package for architecture '$(uname -m)' (supported: x86_64, aarch64)" >&2; exit 1 ;;
esac
# Capture ldd output first: musl ldd may exit nonzero when printing its version.
OC_LDD="$(ldd --version 2>&1 || true)"
case "$OC_LDD" in *musl*) OC_LIBC=-musl ;; *) OC_LIBC= ;; esac
case "${OC_ARCH}${OC_LIBC}" in
  x64) OC_SHA=47734e08968b30cc22a0bffb6d9feeed76ad4aa743213a12122040392d09a057 ;;
  arm64) OC_SHA=5b68c1f4c5e75b4134e664cc8f36d634d64208d4fcfe5bcf3b48bab699c2dbac ;;
  x64-musl) OC_SHA=2a22da39d0b69681272f4daeb51316244eda5c3d606d9e507d61bbf5c1b868a2 ;;
  arm64-musl) OC_SHA=45407388777dfc1894591c9727c169f202ad92c75757038950630a0fbb12ca97 ;;
esac
OC_ASSET="opencode-linux-${OC_ARCH}${OC_LIBC}.tar.gz"
curl -fsSL --retry 5 --retry-delay 2 --retry-max-time 300 -o "/tmp/${OC_ASSET}" \
  "https://github.com/anomalyco/opencode/releases/download/v${OC_VERSION}/${OC_ASSET}" \
  || { echo "awman: opencode ${OC_VERSION} could not download Linux package ${OC_ASSET} for ${OC_ARCH}${OC_LIBC}" >&2; exit 1; }
echo "${OC_SHA}  /tmp/${OC_ASSET}" | sha256sum -c -
tar -xzf "/tmp/${OC_ASSET}" -C /tmp opencode
install -m 0755 /tmp/opencode /usr/local/bin/opencode
rm -f /tmp/opencode "/tmp/${OC_ASSET}"
# Assert as the runtime user, without inheriting installation credentials.
runuser -u agent -- env -i HOME=/home/agent PATH=/usr/local/bin:/usr/bin:/bin bash -eo pipefail -c '
v="$(opencode --version 2>/dev/null | grep -Eo '\''[0-9]+\.[0-9]+\.[0-9]+'\'' | head -n 1)" || v=
[ "${v%%.*}" = "1" ] \
  || { echo "awman: expected opencode 1.x in this image, found '\''${v:-nothing}'\''" >&2; exit 1; }
'

INSTALL_OPENCODE
COPY --chmod=0755 rootfs/usr/local/lib/awman/awman-apply rootfs/usr/local/lib/awman/awman-launch /usr/local/lib/awman/
COPY --chmod=0644 rootfs/usr/local/lib/awman/kit-id /usr/local/lib/awman/kit-id
RUN install -d -m 0755 /var/lib/awman
USER agent
RUN command -v opencode >/dev/null && opencode --version
ENTRYPOINT ["/usr/local/lib/awman/awman-launch", "opencode"]
CMD []

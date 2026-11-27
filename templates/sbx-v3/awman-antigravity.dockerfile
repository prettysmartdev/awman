FROM docker.io/sbx/shell-image:latest
USER root
ARG TARGETARCH
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
# Fail the build if this base cannot execute the policy protocol.
RUN for helper in base64 sha256sum install mv rm runuser id mktemp dirname chmod env; do \
      command -v "$helper" >/dev/null || { echo "awman: required helper $helper missing" >&2; exit 1; }; \
    done; test "$(id -u agent)" = 1000
# Same official installer as Dockerfile.antigravity; verify its bytes before execution.
RUN curl -fsSL --retry 5 --retry-delay 2 --retry-max-time 60 \
        https://antigravity.google/cli/install.sh -o /tmp/agy-install.sh \
    && echo '62966c07365423bd4dc209355060744058fb30d60f5323e2d360e39de64e5042  /tmp/agy-install.sh' | sha256sum -c - \
    && HOME=/root bash /tmp/agy-install.sh \
    && cp /root/.local/bin/agy /usr/local/bin/agy \
    && rm /tmp/agy-install.sh
COPY --chmod=0755 rootfs/usr/local/lib/awman/awman-apply rootfs/usr/local/lib/awman/awman-launch /usr/local/lib/awman/
COPY --chmod=0644 rootfs/usr/local/lib/awman/kit-id /usr/local/lib/awman/kit-id
RUN install -d -m 0755 /var/lib/awman
USER agent
RUN command -v agy >/dev/null && agy --version | grep -Eq '(^|[^0-9])1\.5\.18([^0-9]|$)'
ENTRYPOINT ["/usr/local/lib/awman/awman-launch", "agy"]
CMD []

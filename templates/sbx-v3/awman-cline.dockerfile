FROM docker.io/sbx/shell-image:latest
USER root
ARG TARGETARCH
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
# Fail the build if this base cannot execute the policy protocol.
RUN for helper in base64 sha256sum install mv rm runuser id mktemp dirname chmod env; do \
      command -v "$helper" >/dev/null || { echo "awman: required helper $helper missing" >&2; exit 1; }; \
    done; test "$(id -u agent)" = 1000
RUN command -v npm >/dev/null || { echo "awman: npm is required to install cline" >&2; exit 1; }; \
    n=0; until npm i -g --ignore-scripts cline@3.0.76; do \
      n=$((n+1)); [ "$n" -lt 5 ] || { echo "awman: cline installation failed after five attempts" >&2; exit 1; }; \
      echo "cline install attempt $n failed; retrying..." >&2; sleep 3; \
    done
COPY --chmod=0755 rootfs/usr/local/lib/awman/awman-apply rootfs/usr/local/lib/awman/awman-launch /usr/local/lib/awman/
COPY --chmod=0644 rootfs/usr/local/lib/awman/kit-id /usr/local/lib/awman/kit-id
RUN install -d -m 0755 /var/lib/awman
USER agent
RUN command -v cline >/dev/null && cline --version | grep -Eq '(^|[^0-9])3\.0\.76([^0-9]|$)'
ENTRYPOINT ["/usr/local/lib/awman/awman-launch", "cline"]
CMD []

FROM scratch
COPY --chmod=0755 rootfs/usr/local/lib/awman/awman-apply /usr/local/lib/awman/awman-apply
COPY --chmod=0644 rootfs/usr/local/lib/awman/kit-id /usr/local/lib/awman/kit-id

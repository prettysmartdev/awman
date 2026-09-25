libcap-ng 0.8.3 source: https://people.redhat.com/sgrubb/libcap-ng/libcap-ng-0.8.3.tar.gz

SHA256: bed6f6848e22bb2f83b5f764b2aef0ed393054e803a8e3a8711cb2a39e6b492d

Run `bash third_party/native/libcap-ng/build.sh <host-triple>` on a native Linux builder. The pinned source is built static only; Cargo's `LIBCAPNG_LINK_TYPE=static` and `LIBCAPNG_LIB_PATH` select the resulting archive. The tarball and archives are generated build inputs, not committed. See NOTICE.third-party.md for distribution obligations.

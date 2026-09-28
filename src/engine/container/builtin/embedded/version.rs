//! Binary-only metadata used by the SDK's bounded, non-executing version reader.
// SAFETY: this is immutable data in a dedicated read-only metadata section;
// it introduces no exported symbol, executable code or runtime pointer.
#[allow(unsafe_code)]
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__TEXT,__msbver"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".msbver"))]
static MSB_VERSION: [u8; 5] = *b"0.7.2";

pub fn retain() {
    std::hint::black_box(&MSB_VERSION);
}

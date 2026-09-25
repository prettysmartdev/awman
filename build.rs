use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn field<'a>(table: &'a toml::value::Table, key: &str) -> &'a str {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("payload manifest missing {key}"))
}

fn integer(table: &toml::value::Table, key: &str) -> u64 {
    table
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or_else(|| panic!("payload manifest missing {key}"))
}

fn checked_bytes(path: &PathBuf, expected: &str, limit: u64, hint: &str) -> Vec<u8> {
    println!("cargo:rerun-if-changed={}", path.display());
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}; run {hint}", path.display()));
    assert!(
        !bytes.is_empty() && bytes.len() as u64 <= limit,
        "{} exceeds payload bounds",
        path.display()
    );
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        actual,
        expected,
        "{} checksum mismatch; run {hint}",
        path.display()
    );
    bytes
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(awman_builtin)");
    println!("cargo:rerun-if-changed=third_party/msb-payloads/manifest.toml");
    if env::var_os("CARGO_FEATURE_BUILTIN_RUNTIME").is_none() {
        return;
    }
    let target = env::var("TARGET").expect("Cargo TARGET");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo arch");
    if !matches!(
        target.as_str(),
        "aarch64-apple-darwin" | "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu"
    ) {
        println!("cargo:warning=builtin runtime unsupported on {target}; existing backends remain available");
        return;
    }
    let hint = format!("tools/msb-payloads/fetch.sh {target}");
    let manifest_source =
        fs::read_to_string("third_party/msb-payloads/manifest.toml").expect("payload manifest");
    let manifest: toml::Value = toml::from_str(&manifest_source).expect("valid payload manifest");
    let record = manifest
        .get("targets")
        .and_then(|v| v.get(&target))
        .and_then(toml::Value::as_table)
        .expect("target payload record");
    assert_eq!(
        field(record, "status"),
        "verified",
        "{target} payload is unverified; run {hint} natively and record hashes"
    );
    let root = PathBuf::from("third_party/msb-payloads");
    let kernel_path = root.join(&target).join("kernel.bin");
    let agent_path = root.join(&arch).join("agentd");
    let size = integer(record, "size");
    assert!(
        (64 * 1024..=64 * 1024 * 1024).contains(&size),
        "{target} kernel size outside bounds"
    );
    let kernel = checked_bytes(&kernel_path, field(record, "kernel_sha256"), size, &hint);
    assert_eq!(kernel.len() as u64, size, "kernel size mismatch");
    let metadata = root.join(&target).join("kernel.meta");
    println!("cargo:rerun-if-changed={}", metadata.display());
    let metadata = fs::read_to_string(&metadata)
        .unwrap_or_else(|e| panic!("kernel metadata: {e}; run {hint}"));
    for (key, value) in [
        ("size", size),
        ("guest_address", integer(record, "guest_address")),
        ("entry_address", integer(record, "entry_address")),
    ] {
        assert!(
            metadata
                .lines()
                .any(|line| line == format!("{key}={value}")),
            "kernel {key} mismatch"
        );
    }
    let agent = checked_bytes(
        &agent_path,
        field(record, "agent_sha256"),
        64 * 1024 * 1024,
        &hint,
    );
    assert!(agent.len() >= 20, "agent ELF header is truncated");
    let machine = u16::from_le_bytes([agent[18], agent[19]]);
    let wanted = if arch == "aarch64" { 183 } else { 62 };
    assert!(
        agent.starts_with(b"\x7fELF") && machine == wanted,
        "agent architecture mismatch for {target}"
    );
    if target.ends_with("linux-gnu") {
        let native = PathBuf::from("third_party/native/libcap-ng/lib/libcap-ng.a");
        assert!(
            native.is_file(),
            "{} missing; run third_party/native/libcap-ng/build.sh",
            native.display()
        );
        println!("cargo:rerun-if-changed={}", native.display());
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let source = format!("#[repr(align(65536))] pub struct AlignedKernel(pub [u8; {size}]);\n#[used] pub static KERNEL: AlignedKernel = AlignedKernel(*include_bytes!({:?}));\npub const GUEST_ADDRESS: u64 = {};\npub const ENTRY_ADDRESS: u64 = {};\npub const KERNEL_SHA256: &str = {:?};\n", fs::canonicalize(&kernel_path).expect("kernel path").to_string_lossy(), integer(record, "guest_address"), integer(record, "entry_address"), field(record, "kernel_sha256"));
    fs::write(out.join("msb_kernel.rs"), source).expect("write kernel table");
    println!("cargo:rustc-cfg=awman_builtin");
}

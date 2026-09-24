use std::{env, fs, path::PathBuf};

fn main() {
    let kernel = fs::canonicalize(env::var_os("SPIKE_KERNEL").expect("set SPIKE_KERNEL"))
        .expect("kernel file must exist");
    let metadata_path =
        PathBuf::from(env::var_os("SPIKE_KERNEL_METADATA").expect("set SPIKE_KERNEL_METADATA"));
    let metadata = fs::read_to_string(&metadata_path).expect("kernel metadata");
    let field = |name: &str| {
        metadata
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .expect("missing kernel field")
            .parse::<u64>()
            .expect("invalid kernel field")
    };
    let size = field("size");
    assert_eq!(fs::metadata(&kernel).unwrap().len(), size);
    let source = format!(
        "#[repr(align(65536))]\nstruct AlignedKernel([u8; {size}]);\nstatic KERNEL: AlignedKernel = AlignedKernel(*include_bytes!({kernel:?}));\nconst GUEST_ADDRESS: u64 = {};\nconst ENTRY_ADDRESS: u64 = {};\n",
        field("guest_address"),
        field("entry_address"),
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("kernel.rs"),
        source,
    )
    .unwrap();
    println!("cargo:rerun-if-env-changed=SPIKE_KERNEL");
    println!("cargo:rerun-if-env-changed=SPIKE_KERNEL_METADATA");
    println!("cargo:rerun-if-changed={}", kernel.display());
    println!("cargo:rerun-if-changed={}", metadata_path.display());
    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "linux" => println!("cargo:rustc-link-arg=-Wl,--export-dynamic"),
        "macos" => println!("cargo:rustc-link-arg=-Wl,-export_dynamic"),
        other => panic!("unsupported host: {other}"),
    }
}

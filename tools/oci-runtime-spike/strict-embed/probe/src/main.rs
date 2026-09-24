use std::{error::Error, process::Command};

mod firmware;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--worker") => {
            let root = args.get(2).ok_or("--worker requires a rootfs path")?;
            let vm = msb_krun::VmBuilder::new()
                .machine(|machine| machine.vcpus(1).memory_mib(256))
                .kernel(|kernel| kernel.krunfw_path("@awman-embedded"))
                .fs(|filesystem| filesystem.root(root))
                .build()?;
            vm.enter()?;
        }
        Some("--reexec") => {
            let root = args.get(2).ok_or("--reexec requires a rootfs path")?;
            let output = Command::new(std::env::current_exe()?)
                .args(["--worker", root])
                .env_clear()
                .output()?;
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            println!("parent-survived child-status={}", output.status);
            std::process::exit(output.status.code().unwrap_or(1));
        }
        _ => return Err("usage: awman-strict-embed-probe --reexec|--worker ROOTFS".into()),
    }
    Ok(())
}

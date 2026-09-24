use clap::{Parser, Subcommand};
use microsandbox_cli::{commands, machine_cmd};

#[path = "../../probe/src/firmware.rs"]
mod firmware;

#[used]
#[cfg_attr(target_os = "macos", link_section = "__TEXT,__msbver")]
#[cfg_attr(target_os = "linux", link_section = ".msbver")]
static MSB_VERSION: [u8; 5] = *b"0.7.2";

#[derive(Parser)]
#[command(version = "0.7.2")]
struct Arguments {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Machine(Box<machine_cmd::MachineArgs>),
    Run(commands::run::RunArgs),
    Image(commands::image::ImageArgs),
    Remove(commands::remove::RemoveArgs),
}

fn main() -> anyhow::Result<()> {
    std::hint::black_box(&MSB_VERSION);
    let executable = std::env::current_exe()?;
    std::env::set_var("MSB_PATH", &executable);
    std::env::set_var("MSB_LIBKRUNFW_PATH", &executable);
    let args = Arguments::parse();
    if let Operation::Machine(machine) = args.command {
        machine_cmd::run(*machine);
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        match args.command {
            Operation::Run(args) => commands::run::run(args, None).await,
            Operation::Image(args) => commands::image::run(args).await,
            Operation::Remove(args) => commands::remove::run(args).await,
            Operation::Machine(_) => unreachable!(),
        }
    })
}

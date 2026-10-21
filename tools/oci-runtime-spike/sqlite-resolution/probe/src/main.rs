use clap::{Parser, Subcommand};
use microsandbox_cli::{commands, machine_cmd};

mod sqlite;


#[derive(Parser)]
#[command(version = "0.7.8")]
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
    SqliteCheck,
}

fn main() -> anyhow::Result<()> {
    awman_host_shims::prepare_builtin_worker().map_err(anyhow::Error::msg)?;
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
            Operation::SqliteCheck => sqlite::check().await,
            Operation::Machine(_) => unreachable!(),
        }
    })
}

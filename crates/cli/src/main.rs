//! Soroban Forge developer CLI.
//!
//! ```text
//! soroban-forge build
//! soroban-forge build --wasm --check-size
//! soroban-forge test --package soroban-forge-escrow
//! soroban-forge lint --fix
//! soroban-forge deploy path/to/escrow.wasm --network testnet
//! ```

mod cli;
mod commands;

use clap::{Parser, Subcommand};
use cli::{BuildArgs, DeployArgs, LintArgs, NewArgs, TestArgs};

#[derive(Parser, Debug)]
#[command(
    name = "soroban-forge",
    about = "Developer CLI for Soroban Forge",
    version,
    author
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Build(BuildArgs),
    Lint(LintArgs),
    Test(TestArgs),
    Deploy(DeployArgs),
    New(NewArgs),
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => commands::build::run(args)?,
        Commands::Lint(args) => commands::lint::run(args)?,
        Commands::Test(args) => commands::test::run(args)?,
        Commands::Deploy(args) => commands::deploy::run(args)?,
        Commands::New(args) => commands::new::run(args)?,
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands};
    use clap::{CommandFactory, Parser};

    #[test]
    fn build_wasm_and_size_flags_parse() {
        let cli = Cli::try_parse_from([
            "soroban-forge",
            "build",
            "--wasm",
            "--check-size",
            "--package",
            "soroban-forge-escrow",
        ])
        .unwrap();

        let Commands::Build(args) = cli.command else {
            panic!("expected build command");
        };
        assert!(args.wasm);
        assert!(args.check_size);
        assert_eq!(args.package.as_deref(), Some("soroban-forge-escrow"));
    }

    #[test]
    fn build_help_documents_wasm_and_size_flags() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("build")
            .expect("build subcommand must exist")
            .render_long_help()
            .to_string();
        assert!(help.contains("--wasm"));
        assert!(help.contains("--check-size"));
    }
}

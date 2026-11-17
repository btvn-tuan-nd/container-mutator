mod config;
mod error;
mod extract;
mod install;
mod lifecycle;
mod paths;
mod registry;
mod releases;
mod update;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "mutator",
    version,
    about = "Apply application snapshot updates under native s6"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Action {
    /// Initialize release state before mutable services start (s6 oneshot).
    Prepare,
    /// Watch the configured image and switch mutable services (s6 longrun).
    Watch,
    /// Generate native s6 definitions at image build time.
    InstallService(install::Options),
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Action::Prepare => update::prepare(),
        Action::Watch => update::watch(),
        Action::InstallService(options) => install::install(&options),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mutator: {error:#}");
            ExitCode::FAILURE
        }
    }
}

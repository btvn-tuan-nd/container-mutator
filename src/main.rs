mod boot;
mod cli;
mod launch;
mod state;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let result = match cli::Cli::parse().command {
        cli::Action::Boot { command } => boot::boot(command),
        cli::Action::Launch => launch::launch(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mutator: {error:#}");
            ExitCode::FAILURE
        }
    }
}

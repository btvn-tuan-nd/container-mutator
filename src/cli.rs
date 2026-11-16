use std::ffi::OsString;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "mutator", version, about = "Run a container command under s6")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Action,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Action {
    /// Save the application context and hand PID 1 to s6.
    #[command(
        after_help = "Must run as PID 1: do not use Docker --init or Compose init: true.\n\
        /run must be writable by the container user. For a read-only root, set\n\
        S6_READ_ONLY_ROOT=1 and mount an executable tmpfs: --tmpfs /run:exec.\n\
        Arguments, environment, and working directory are restored on every restart."
    )]
    Boot {
        /// Foreground application command and arguments, following --.
        #[arg(last = true, required = true, num_args = 1.., value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Execute the saved application context (used internally by s6).
    Launch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn boot_requires_a_command_after_the_separator() {
        for args in [vec!["boot"], vec!["boot", "echo"], vec!["boot", "--"]] {
            assert!(Cli::try_parse_from(std::iter::once("mutator").chain(args)).is_err());
        }
    }

    #[test]
    fn trailing_arguments_remain_verbatim() {
        let arguments: Vec<OsString> = [
            "--help",
            "",
            "two words",
            "--",
            "key=value",
            "line\nbreak",
            "quote'\"",
        ]
        .into_iter()
        .map(OsString::from)
        .chain([OsString::from_vec(b"non-utf8-\xff".to_vec())])
        .collect();
        let cli = Cli::try_parse_from(
            ["mutator", "boot", "--"]
                .map(OsString::from)
                .into_iter()
                .chain(arguments.clone()),
        )
        .unwrap();
        assert_eq!(cli.command, Action::Boot { command: arguments });
    }

    #[test]
    fn launch_rejects_application_arguments() {
        assert!(Cli::try_parse_from(["mutator", "launch", "extra"]).is_err());
    }
}

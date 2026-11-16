use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::{self, Command};

use anyhow::{Context, Result, ensure};

use crate::state::{self, LaunchState};

/// Save the application context, then hand PID 1 to s6 without forwarding CMD.
pub fn boot(command: Vec<OsString>) -> Result<()> {
    ensure!(
        process::id() == 1,
        "mutator boot must run as PID 1; remove Docker --init / Compose init: true"
    );
    let state = LaunchState {
        version: state::VERSION,
        command,
        cwd: std::env::current_dir()
            .context("cannot read the container working directory")?
            .into_os_string(),
        environment: std::env::vars_os().collect(),
    };
    state::save(&state::default_state_path(), &state)?;

    // Apply the startup failure policy only to s6, not the saved app environment.
    let error = Command::new("/init")
        .env("S6_BEHAVIOUR_IF_STAGE2_FAILS", "2")
        .exec();
    Err(error).context("cannot execute /init")
}

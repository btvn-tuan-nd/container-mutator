use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, Result};

use crate::state;

pub fn launch() -> Result<()> {
    let state = state::load(&state::default_state_path())?;
    // exec uses the saved PATH and cwd and leaves no wrapper process behind.
    let error = Command::new(&state.command[0])
        .args(&state.command[1..])
        .env_clear()
        .envs(state.environment.iter().map(|(key, value)| (key, value)))
        .current_dir(&state.cwd)
        .exec();
    Err(error)
        .with_context(|| format!("cannot execute {:?} from {:?}", state.command[0], state.cwd))
}

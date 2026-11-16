use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

// Ephemeral launch state is replaced on every boot.
pub const VERSION: u32 = 1;
pub const STATE_FILE_NAME: &str = "boot.json";

pub fn default_state_path() -> PathBuf {
    Path::new("/run/mutator").join(STATE_FILE_NAME)
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchState {
    pub version: u32,
    pub command: Vec<OsString>,
    // Serde's OsString representation preserves Unix bytes, unlike PathBuf's
    // UTF-8-only serialization. Command::current_dir accepts this directly.
    pub cwd: OsString,
    pub environment: Vec<(OsString, OsString)>,
}

impl LaunchState {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported launch state version {}",
            self.version
        );
        ensure!(
            self.command.first().is_some_and(|name| !name.is_empty()),
            "launch state requires a nonempty command"
        );
        ensure!(
            Path::new(&self.cwd).is_absolute(),
            "launch state working directory must be absolute"
        );
        for value in self.command.iter().chain(std::iter::once(&self.cwd)) {
            ensure!(
                !value.as_bytes().contains(&0),
                "launch state command or working directory contains a NUL byte"
            );
        }
        for (key, value) in &self.environment {
            ensure!(
                !key.as_bytes().contains(&b'='),
                "launch state environment name contains '='"
            );
            ensure!(
                !key.as_bytes().contains(&0) && !value.as_bytes().contains(&0),
                "launch state environment contains a NUL byte"
            );
        }
        Ok(())
    }
}

/// Publish a complete, owner-private state file in the same filesystem.
/// NamedTempFile handles unique names and cleanup if writing or publication fails.
pub fn save(path: &Path, state: &LaunchState) -> Result<()> {
    state.validate()?;
    let directory = path.parent().context("launch state path has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .with_context(|| {
            format!(
                "cannot create {}; /run must be writable before boot",
                directory.display()
            )
        })?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .context("cannot make launch state directory private")?;
    let mut file = NamedTempFile::new_in(directory).context("cannot create launch state file")?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut file, state).context("cannot serialize launch state")?;
    file.as_file()
        .sync_all()
        .context("cannot flush launch state")?;
    file.persist(path)
        .with_context(|| format!("cannot publish {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<LaunchState> {
    let encoded = fs::read(path).with_context(|| {
        format!(
            "cannot read {}; start with mutator boot -- <command>",
            path.display()
        )
    })?;
    let state: LaunchState = serde_json::from_slice(&encoded)
        .with_context(|| format!("invalid launch state {}", path.display()))?;
    state.validate()?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn sample_state() -> LaunchState {
        LaunchState {
            version: VERSION,
            command: [
                "/bin/echo",
                "",
                "two words",
                "quote'\"",
                "key=value",
                "line\nbreak",
            ]
            .into_iter()
            .map(OsString::from)
            .chain([OsString::from_vec(b"non-utf8-\xff".to_vec())])
            .collect(),
            cwd: OsString::from_vec(b"/srv/work-\xfe".to_vec()),
            environment: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("EMPTY".into(), "".into()),
                ("SPECIAL".into(), "  a\nb=\"quoted\"  ".into()),
                (
                    OsString::from_vec(b"KEY-\xff".to_vec()),
                    OsString::from_vec(b"\xff\xfe".to_vec()),
                ),
            ],
        }
    }

    #[test]
    fn round_trip_preserves_unix_bytes_and_private_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        let expected = sample_state();
        save(&path, &expected).unwrap();
        assert_eq!(load(&path).unwrap(), expected);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn replacement_publishes_new_state_without_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        save(&path, &sample_state()).unwrap();
        let mut replacement = sample_state();
        replacement.command = vec!["/bin/true".into()];
        save(&path, &replacement).unwrap();
        assert_eq!(load(&path).unwrap(), replacement);
        let names: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from(STATE_FILE_NAME)]);
    }

    #[test]
    fn malformed_states_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        let corruptions: [fn(&mut LaunchState); 7] = [
            |state| state.version = 0,
            |state| state.command.clear(),
            |state| state.command[0] = "".into(),
            |state| state.cwd = "relative".into(),
            |state| state.command.push("nul\0argument".into()),
            |state| state.environment.push(("KEY".into(), "nul\0value".into())),
            |state| state.environment.push(("A=B".into(), "value".into())),
        ];
        for corrupt in corruptions {
            let mut state = sample_state();
            corrupt(&mut state);
            fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
            assert!(load(&path).is_err());
        }
        fs::write(&path, b"{\"version\":").unwrap();
        assert!(load(&path).is_err());
        let mut state = serde_json::to_value(sample_state()).unwrap();
        state["extra"] = true.into();
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(load(&path).is_err());
    }
}

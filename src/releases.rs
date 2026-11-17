use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempDir};

pub fn ensure_space(path: &Path, bytes: u64) -> Result<()> {
    if let Ok(available) = fs2::available_space(path) {
        ensure!(
            available >= bytes.saturating_add(64 * 1024 * 1024),
            "insufficient staging space at {path:?}; 64 MiB headroom required"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub version: u32,
    pub active: String,
    pub previous: Option<String>,
    pub digest: Option<String>,
    pub rejected: Option<String>,
    pub pending: Option<Pending>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub release: String,
    pub digest: String,
}

pub struct Store {
    app: PathBuf,
    root: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            app: "/app".into(),
            root: "/var/lib/mutator".into(),
        }
    }
}

impl Store {
    fn release(&self, name: &str) -> Result<PathBuf> {
        ensure!(
            crate::lifecycle::safe_name(name)
                && (name == "bootstrap" || name.starts_with("release-")),
            "invalid release name"
        );
        Ok(self.root.join(name).join("app"))
    }

    /// Only opt-in update mode changes /app. Bootstrap can resume after a process crash.
    pub fn initialize(&self) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&self.root)?;
        ensure!(
            fs::symlink_metadata(&self.root)?.is_dir(),
            "release storage must not be a symlink"
        );
        ensure!(
            fs::metadata(&self.root)?.dev()
                == fs::metadata(self.app.parent().context("/app has no parent")?)?.dev(),
            "release storage must be on the container's writable filesystem"
        );
        if !self.root.join("selection.json").exists() {
            let bootstrap = self.release("bootstrap")?;
            if !bootstrap.exists() {
                ensure!(
                    fs::symlink_metadata(&self.app)?.file_type().is_dir(),
                    "update mode requires /app to be a real directory, not a symlink"
                );
                fs::create_dir_all(bootstrap.parent().unwrap())?;
                if let Err(error) = fs::rename(&self.app, &bootstrap) {
                    if error.kind() != std::io::ErrorKind::CrossesDevices {
                        return Err(error).context("cannot move /app into release storage");
                    }
                    // OverlayFS may refuse to rename a lower-layer directory.
                    // Copy before publishing, so a crash never selects a partial tree.
                    let staged = self.stage()?;
                    let status = Command::new("/command/s6-hiercopy")
                        .arg(&self.app)
                        .arg(staged.path().join("app"))
                        .status()
                        .context("cannot copy bootstrap /app")?;
                    ensure!(status.success(), "bootstrap copy failed: {status}");
                    sync_tree(&staged.path().join("app"))?;
                    fs::rename(staged.path().join("app"), &bootstrap)?;
                } else {
                    sync_tree(&bootstrap)?;
                }
            } else {
                sync_tree(&bootstrap)?;
            }
            File::open(bootstrap.parent().unwrap())?.sync_all()?;
            self.save(&Selection {
                version: 1,
                active: "bootstrap".into(),
                previous: None,
                digest: None,
                rejected: None,
                pending: None,
            })?;
        }
        let mut selection = self.load()?;
        // Metadata is committed before removing the original lower-layer tree.
        // This also resumes a bootstrap interrupted during that removal.
        if fs::symlink_metadata(&self.app).is_ok_and(|metadata| metadata.is_dir()) {
            fs::remove_dir_all(&self.app).context("cannot replace original /app directory")?;
        }
        self.activate(&selection.active)?;
        if selection.pending.is_some() {
            self.reject_pending(&mut selection)?;
        }
        Ok(())
    }

    pub fn lock(&self) -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.root.join("updater.lock"))?;
        file.try_lock()
            .context("another updater already holds the release lock")?;
        Ok(file)
    }

    pub fn load(&self) -> Result<Selection> {
        let state: Selection =
            serde_json::from_slice(&fs::read(self.root.join("selection.json"))?)?;
        ensure!(state.version == 1, "unsupported release state version");
        ensure!(
            self.release(&state.active)?.is_dir(),
            "active release is missing"
        );
        if let Some(previous) = &state.previous {
            self.release(previous)?;
        }
        if let Some(pending) = &state.pending {
            self.release(&pending.release)?;
            ensure!(
                pending.release != state.active,
                "pending release is already committed"
            );
        }
        Ok(state)
    }

    pub fn save(&self, state: &Selection) -> Result<()> {
        let mut file = NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut file, state)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("selection.json"))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    /// Quarantine a pending trial's digest: it never became ready, so its
    /// release directory is garbage and its digest must not be retried.
    pub fn reject_pending(&self, selection: &mut Selection) -> Result<()> {
        if let Some(pending) = selection.pending.take() {
            selection.rejected = Some(pending.digest);
            self.save(selection)?;
        }
        Ok(())
    }

    pub fn is_active(&self, name: &str) -> Result<bool> {
        Ok(matches!(fs::read_link(&self.app), Ok(target) if target == self.release(name)?))
    }

    /// Renaming a symlink in /app's parent atomically selects the release.
    pub fn activate(&self, name: &str) -> Result<()> {
        let target = self.release(name)?;
        ensure!(
            fs::symlink_metadata(&target)?.is_dir(),
            "release /app must be a directory"
        );
        let temporary = tempfile::Builder::new()
            .prefix(".switch-")
            .tempdir_in(&self.root)?;
        let link = temporary.path().join("app");
        symlink(&target, &link)?;
        fs::rename(link, &self.app).context("cannot atomically replace /app symlink")?;
        File::open(self.app.parent().context("/app has no parent")?)?.sync_all()?;
        Ok(())
    }

    pub fn stage(&self) -> Result<TempDir> {
        let directory = tempfile::Builder::new()
            .prefix("release-")
            .tempdir_in(&self.root)?;
        fs::create_dir(directory.path().join("app"))?;
        Ok(directory)
    }

    pub fn publish(&self, staged: TempDir) -> Result<String> {
        fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o755))?;
        sync_tree(staged.path())?;
        let name = staged
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .context("invalid staging name")?
            .to_owned();
        let _ = staged.keep();
        File::open(&self.root)?.sync_all()?;
        Ok(name)
    }

    pub fn cleanup(&self, selection: &Selection) -> Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if (name == "bootstrap" || name.starts_with("release-") || name.starts_with(".switch-"))
                && name != selection.active
                && selection.previous.as_deref() != Some(name)
                && selection
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.release != name)
                && entry.file_type()?.is_dir()
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }
}

/// Flush payload contents before a journal record can select them. Never follow links.
fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store {
            app: root.path().join("app"),
            root: root.path().join("releases"),
        };
        fs::create_dir(&store.app).unwrap();
        fs::write(store.app.join("version"), "original").unwrap();
        (root, store)
    }

    #[test]
    fn uncommitted_switch_recovers_previous_application_on_boot() {
        let (_root, store) = fixture();
        store.initialize().unwrap();
        let staged = store.stage().unwrap();
        fs::write(staged.path().join("app/version"), "candidate").unwrap();
        let candidate = store.publish(staged).unwrap();
        store.activate(&candidate).unwrap();
        assert_eq!(
            fs::read_to_string(store.app.join("version")).unwrap(),
            "candidate"
        );
        store.initialize().unwrap();
        assert_eq!(
            fs::read_to_string(store.app.join("version")).unwrap(),
            "original"
        );
    }

    #[test]
    fn interrupted_trial_restores_and_quarantines_before_app_start() {
        for switched in [false, true] {
            let (_root, store) = fixture();
            store.initialize().unwrap();
            let staged = store.stage().unwrap();
            fs::write(staged.path().join("app/version"), "candidate").unwrap();
            let candidate = store.publish(staged).unwrap();
            let mut selection = store.load().unwrap();
            selection.pending = Some(Pending {
                release: candidate.clone(),
                digest: "sha256:interrupted".into(),
            });
            store.save(&selection).unwrap();
            if switched {
                store.activate(&candidate).unwrap();
            }
            store.initialize().unwrap();
            let recovered = store.load().unwrap();
            assert_eq!(
                fs::read_to_string(store.app.join("version")).unwrap(),
                "original"
            );
            assert_eq!(recovered.rejected.as_deref(), Some("sha256:interrupted"));
            assert!(recovered.pending.is_none());
            store.cleanup(&recovered).unwrap();
            assert!(!store.release(&candidate).unwrap().exists());
        }
    }

    #[test]
    fn committed_switch_survives_restart_and_keeps_previous_release() {
        let (_root, store) = fixture();
        store.initialize().unwrap();
        let staged = store.stage().unwrap();
        fs::write(staged.path().join("app/version"), "new").unwrap();
        let candidate = store.publish(staged).unwrap();
        let mut selection = store.load().unwrap();
        selection.previous = Some(selection.active.clone());
        selection.active = candidate;
        selection.digest = Some("sha256:example".into());
        store.activate(&selection.active).unwrap();
        store.save(&selection).unwrap();
        store.initialize().unwrap();
        store.cleanup(&selection).unwrap();
        let abandoned = store.publish(store.stage().unwrap()).unwrap();
        store.cleanup(&selection).unwrap();
        assert!(!store.release(&abandoned).unwrap().exists());
        assert_eq!(
            fs::read_to_string(store.app.join("version")).unwrap(),
            "new"
        );
        assert!(store.release("bootstrap").unwrap().join("version").exists());
    }

    #[test]
    fn bootstrap_rename_can_recover_before_metadata_is_written() {
        let (_root, store) = fixture();
        let bootstrap = store.release("bootstrap").unwrap();
        fs::create_dir_all(bootstrap.parent().unwrap()).unwrap();
        fs::rename(&store.app, &bootstrap).unwrap();
        store.initialize().unwrap();
        assert_eq!(
            fs::read_to_string(store.app.join("version")).unwrap(),
            "original"
        );
        assert!(store.release("../escape").is_err());
    }

    #[test]
    fn bootstrap_copy_recovers_before_original_directory_is_removed() {
        let (_root, store) = fixture();
        let bootstrap = store.release("bootstrap").unwrap();
        fs::create_dir_all(&bootstrap).unwrap();
        fs::write(bootstrap.join("version"), "complete copy").unwrap();
        store.initialize().unwrap();
        assert!(
            fs::symlink_metadata(&store.app)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(store.app.join("version")).unwrap(),
            "complete copy"
        );
    }

    #[test]
    fn only_one_updater_can_hold_the_release_lock() {
        let (_root, store) = fixture();
        store.initialize().unwrap();
        let lock = store.lock().unwrap();
        assert!(store.lock().is_err());
        drop(lock);
        let _next_owner = store.lock().unwrap();
    }
}

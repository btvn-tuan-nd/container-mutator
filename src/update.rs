//! Watch orchestration: poll the registry, stage and verify snapshots, and
//! switch the mutable bundle through journal-backed release transitions.

use std::fs;
use std::thread;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::extract;
use crate::lifecycle::{
    Lifecycle, clear_trial, clear_trial_after_healthy, mark_trial, trial_exists,
};
use crate::registry::Registry;
use crate::releases::{Pending, Selection, Store};

/// s6 oneshot: recover release state before every mutable service starts.
pub fn prepare() -> Result<()> {
    // The single-app archive opts into validation, including when updates are disabled.
    let preset = std::path::Path::new("/etc/s6-overlay/s6-rc.d/mutator-app/data");
    if preset.join("preset").exists() {
        crate::paths::from_env()?;
        anyhow::ensure!(
            std::process::Command::new(preset.join("dispatch"))
                .arg("validate")
                .status()
                .context("cannot validate single-app configuration")?
                .success(),
            "invalid single-app configuration"
        );
    }
    let Some(config) = Config::from_env()? else {
        clear_trial()?;
        return Ok(());
    };
    let store = Store::new(config.app_dir)?;
    store
        .initialize()
        .context("cannot initialize release state before services start")?;
    let selection = store.load()?;
    store.cleanup(&selection)?;
    // At boot no app service is running, so a marker from a killed watcher is stale.
    clear_trial()?;
    Ok(())
}

/// s6 longrun: own bundle transitions but never the normal service supervision.
pub fn watch() -> Result<()> {
    let Some(config) = Config::from_env()? else {
        eprintln!("mutator: updates disabled because MUTATOR_IMAGE is unset");
        idle()
    };
    std::env::set_current_dir("/").context("cannot change to /")?;
    let store = Store::new(config.app_dir.clone())?;
    let _lock = store
        .lock()
        .context("another updater owns the release lock")?;
    let lifecycle = Lifecycle::new(config.timeout)?;
    if let Err(error) = recover(&store, &lifecycle) {
        eprintln!("mutator: startup recovery failed: {error:#}");
    }
    let registry = Registry::new(&config)?;
    loop {
        if let Err(error) = poll(&registry, &store, &lifecycle, &config) {
            eprintln!("mutator: update failed: {error:#}");
        }
        thread::sleep(config.interval);
    }
}

fn idle() -> ! {
    loop {
        thread::park();
    }
}

/// Restore the committed release without swapping on a failed stop.
/// A pending record means the previous watcher may have died mid-transition,
/// even when /app still points at the committed release.
fn recover(store: &Store, lifecycle: &Lifecycle) -> Result<Selection> {
    let mut selection = store.load()?;
    if selection.pending.is_none() && store.is_active(&selection.active)? {
        // Reconcile the committed release: the watcher may have died between
        // the final ensure_up and marker removal, stranding the service
        // wanted-down; a finish may also have raced an earlier poll. Every
        // recover re-checks, so digest equality in poll cannot strand it.
        lifecycle.ensure_up()?;
        if trial_exists() {
            clear_trial_after_healthy(lifecycle)?;
        }
        // A watcher killed mid-staging leaves release-* directories that its
        // TempDir cleanup never removed; nothing else would collect them.
        store.cleanup(&selection)?;
        return Ok(selection);
    }

    if let Some(pending) = &selection.pending {
        eprintln!(
            "mutator: recovering interrupted trial {}; restoring {}",
            pending.digest, selection.active
        );
    }
    mark_trial()?;
    lifecycle.stop()?;
    store.activate(&selection.active)?;
    lifecycle.start()?;
    lifecycle.ensure_up()?;
    store.reject_pending(&mut selection)?;
    clear_trial_after_healthy(lifecycle)?;
    store.cleanup(&selection)?;
    Ok(selection)
}

fn poll(
    registry: &Registry<'_>,
    store: &Store,
    lifecycle: &Lifecycle,
    config: &Config,
) -> Result<()> {
    let mut selection = recover(store, lifecycle)?;
    let image = registry.resolve()?;
    if selection.digest.as_deref() == Some(&image.digest)
        || selection.rejected.as_deref() == Some(&image.digest)
    {
        return Ok(());
    }

    eprintln!("mutator: staging {}", image.digest);
    let release = match stage(registry, store, config, &image) {
        Ok(release) => release,
        Err(error) => {
            if error.is::<crate::error::InvalidRelease>() {
                eprintln!("mutator: rejecting {}: {error:#}", image.digest);
                selection.rejected = Some(image.digest);
                store
                    .save(&selection)
                    .context("cannot record rejected release")?;
            }
            return Err(error);
        }
    };

    let pending = Pending {
        release,
        digest: image.digest,
    };
    selection.pending = Some(pending.clone());
    store.save(&selection)?;
    eprintln!("mutator: switching to {}", pending.digest);
    mark_trial()?;
    if let Err(error) = switch(store, lifecycle, &pending) {
        eprintln!(
            "mutator: rejecting {}: {error:#}; restoring {}",
            pending.digest, selection.active
        );
        if let Err(recovery) = recover(store, lifecycle) {
            eprintln!("mutator: recovery failed: {recovery:#}");
        }
        return Err(error);
    }

    selection.pending = None;
    selection.previous = Some(std::mem::replace(&mut selection.active, pending.release));
    selection.digest = Some(pending.digest);
    selection.rejected = None;
    if let Err(error) = store.save(&selection) {
        // A directory fsync can fail after the rename became visible. Re-read the
        // authoritative journal instead of blindly undoing a possibly committed release.
        recover(store, lifecycle).context("cannot reconcile release commit failure")?;
        return Err(error).context("cannot commit release selection");
    }
    // Keep the trial marker until every service is normally wanted up again. If
    // a candidate crashed with finish 125, the live s6-rc state may still say up.
    lifecycle.ensure_up()?;
    clear_trial_after_healthy(lifecycle)?;
    eprintln!(
        "mutator: committed {}",
        selection.digest.as_deref().unwrap()
    );
    store.cleanup(&selection)
}

/// Download, validate, and publish a release directory for the candidate.
fn stage(
    registry: &Registry<'_>,
    store: &Store,
    config: &Config,
    image: &crate::registry::Candidate,
) -> Result<String> {
    let layer = registry.validate(image, &config.runtime, &config.limits)?;
    let staged = store.stage()?;
    let layer_path = staged.path().join("layer");
    registry.download_layer(&layer, &layer_path)?;
    extract::extract_snapshot(
        &layer_path,
        &layer.media_type,
        &staged.path().join("app"),
        &config.limits,
    )?;
    fs::remove_file(&layer_path)?;
    store.publish(staged)
}

/// Stop the mutable bundle, select the pending release, and prove it ready.
fn switch(store: &Store, lifecycle: &Lifecycle, pending: &Pending) -> Result<()> {
    // s6-rc serializes with an orphaned transition from a killed watcher.
    lifecycle.stop()?;
    store.activate(&pending.release)?;
    lifecycle.start()?;
    // A candidate that crashes after becoming ready must not commit.
    lifecycle.ready()
}

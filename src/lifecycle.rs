//! Native s6-rc transitions for the mutable application bundle: stop, start,
//! readiness, and the trial marker that keeps failed candidates down.

use std::os::unix::fs::DirBuilderExt;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

/// The only bundle watch may transition. Members must be mutable app longruns.
pub const BUNDLE: &str = "mutable-app";

const LIVE: &str = "/run/s6-rc";
const SERVICE_DIRS: &str = "/run/s6-rc/servicedirs";
const S6_RC: &str = "/command/s6-rc";
const S6_RC_DB: &str = "/command/s6-rc-db";
const S6_SVSTAT: &str = "/command/s6-svstat";
const S6_SVWAIT: &str = "/command/s6-svwait";
/// Present from before the first transition until the trial is committed or restored.
const TRIAL: &str = "/run/mutator/trial";

pub struct Lifecycle {
    timeout: Duration,
    longruns: Vec<Longrun>,
}

struct Longrun {
    name: String,
    dir: PathBuf,
}

impl Lifecycle {
    pub fn new(timeout: Duration) -> Result<Self> {
        let kind = s6_db(&["type", BUNDLE])?;
        ensure!(
            kind.len() == 1 && kind[0] == "bundle",
            "{BUNDLE} must be an s6-rc bundle"
        );
        let members: BTreeSet<_> = s6_db(&["atomics", BUNDLE])?.into_iter().collect();
        ensure!(
            !members.is_empty(),
            "bundle {BUNDLE} has no atomic services"
        );
        let longruns: BTreeSet<_> = s6_db(&["list", "longruns"])?.into_iter().collect();
        let closure: BTreeSet<_> = s6_db(&["-d", "all-dependencies", BUNDLE])?
            .into_iter()
            .collect();
        let outside: Vec<_> = closure.difference(&members).cloned().collect();
        ensure!(
            outside.is_empty(),
            "bundle {BUNDLE} would stop services outside it: {}",
            outside.join(", ")
        );

        let mut services = Vec::with_capacity(members.len());
        for name in members {
            ensure!(safe_name(&name), "invalid service name {name:?}");
            ensure!(
                longruns.contains(&name),
                "bundle {BUNDLE} contains non-longrun {name}"
            );
            let dir = Path::new(SERVICE_DIRS).join(&name);
            let notification = fs::read_to_string(dir.join("notification-fd"))
                .with_context(|| format!("service {name} must declare native readiness on fd 3"))?;
            ensure!(
                notification.trim() == "3",
                "service {name} must declare native readiness on fd 3"
            );
            services.push(Longrun { name, dir });
        }
        Ok(Self {
            timeout,
            longruns: services,
        })
    }

    /// s6-rc waits only for the daemon to die; wait for each finish script too,
    /// but never beyond the transition deadline.
    pub fn stop(&self) -> Result<()> {
        self.change("-d", "stop")?;
        let deadline = Instant::now() + self.timeout;
        // A timed-out readiness transition can leave an unready daemon running
        // while s6-rc records it as down. Reconcile the actual supervisors too.
        for service in &self.longruns {
            let status = Command::new("/command/s6-svc")
                .arg("-d")
                .arg(&service.dir)
                .status()
                .with_context(|| format!("cannot stop {}", service.name))?;
            ensure!(status.success(), "cannot stop {}: {status}", service.name);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "mutable service cleanup exceeded MUTATOR_TIMEOUT"
        );
        let timeout = remaining.as_millis().max(1).to_string();
        let mut command = Command::new(S6_SVWAIT);
        command.args(["-a", "-D", "-t", timeout.as_str()]);
        for service in &self.longruns {
            command.arg(&service.dir);
        }
        let status = command
            .status()
            .context("cannot wait for mutable service cleanup")?;
        ensure!(
            status.success(),
            "mutable service cleanup did not finish: {status}"
        );
        Ok(())
    }

    pub fn start(&self) -> Result<()> {
        self.change("-u", "start")
    }

    /// A candidate that crashes after becoming ready must not commit.
    pub fn ready(&self) -> Result<()> {
        for service in &self.longruns {
            let status = self.status("-o", "up,ready", service)?;
            ensure!(
                status == "true true",
                "service {} is not natively up and ready after start ({status})",
                service.name
            );
        }
        Ok(())
    }

    /// Committed services must be wanted up again even if finish raced the marker.
    pub fn ensure_up(&self) -> Result<()> {
        let mut unhealthy = Vec::new();
        for service in &self.longruns {
            let status = self.status("-o", "up,wantedup,normallyup", service)?;
            if status != "true true true" {
                unhealthy.push(format!("{} ({status})", service.name));
            }
        }
        if unhealthy.is_empty() {
            return Ok(());
        }
        eprintln!(
            "mutator: normalizing mutable services: {}",
            unhealthy.join(", ")
        );
        mark_trial()?;
        self.stop()?;
        self.start()?;
        self.ready()
    }

    /// s6-rc lock acquisition serializes with an orphaned transition from a killed watcher.
    fn change(&self, direction: &str, action: &str) -> Result<()> {
        let timeout = self.timeout.as_millis().to_string();
        let status = Command::new(S6_RC)
            .args([
                "-b",
                "-l",
                LIVE,
                "-t",
                timeout.as_str(),
                direction,
                "change",
                BUNDLE,
            ])
            .status()
            .with_context(|| format!("cannot run s6-rc to {action} {BUNDLE}"))?;
        ensure!(
            status.success(),
            "s6-rc could not {action} {BUNDLE}: {status}"
        );
        Ok(())
    }

    fn status(&self, option: &str, fields: &str, service: &Longrun) -> Result<String> {
        let output = Command::new(S6_SVSTAT)
            .args([option, fields])
            .arg(&service.dir)
            .output()
            .with_context(|| format!("cannot inspect service {}", service.name))?;
        ensure!(
            output.status.success(),
            "cannot inspect service {}: {}",
            service.name,
            output.status
        );
        Ok(String::from_utf8(output.stdout)
            .with_context(|| format!("service {} status is not UTF-8", service.name))?
            .trim()
            .to_owned())
    }
}

fn s6_db(args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new(S6_RC_DB)
        .args(["-b", "-l", LIVE])
        .args(args)
        .output()
        .with_context(|| format!("cannot run s6-rc-db {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "s6-rc-db {} failed: {}: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)
        .context("s6-rc-db output is not UTF-8")?
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect())
}

pub(crate) fn safe_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

pub fn mark_trial() -> Result<()> {
    let directory = Path::new(TRIAL).parent().expect("trial path has a parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    fs::write(TRIAL, b"").context("cannot create the trial marker")
}

pub fn clear_trial() -> Result<()> {
    match fs::remove_file(TRIAL) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("cannot remove the trial marker"),
    }
}

pub fn trial_exists() -> bool {
    Path::new(TRIAL).exists()
}

/// Remove the marker, then verify once more that a finish which raced removal
/// did not leave a service permanently down. Re-arm the marker for recovery.
pub fn clear_trial_after_healthy(lifecycle: &Lifecycle) -> Result<()> {
    clear_trial()?;
    lifecycle.ensure_up()?;
    clear_trial()
}

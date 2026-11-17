//! Build-time service wiring. Runtime supervision and readiness stay with s6.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use clap::Args;

#[derive(Debug, PartialEq, Eq, Args)]
pub struct Options {
    /// Unique application service name.
    #[arg(long)]
    name: String,
    /// Shell command that succeeds only when the application is ready.
    #[arg(long)]
    check: String,
    /// Optional idempotent shell command to clean up detached processes.
    /// Failed cleanup is retried; release switching waits for it to succeed.
    #[arg(long)]
    cleanup: Option<String>,
    /// Foreground application command and arguments, after --.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<String>,
}

pub fn install(options: &Options) -> Result<()> {
    crate::paths::from_env()?;
    install_at(Path::new("/etc/s6-overlay"), options)
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write(path: &Path, value: &str, mode: u32) -> Result<()> {
    fs::write(path, value)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn install_at(root: &Path, options: &Options) -> Result<()> {
    let name = &options.name;
    ensure!(
        !name.is_empty()
            && name.len() <= 200
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && !["base", "user", "user2", "mutable-app"].contains(&name.as_str())
            && !name.starts_with("mutator-"),
        "invalid or reserved service name"
    );
    ensure!(
        !options.check.trim().is_empty(),
        "readiness check must not be empty"
    );
    ensure!(
        !options.command.is_empty()
            && !options.command[0].is_empty()
            && !options.command[0].starts_with('-'),
        "application command is required"
    );
    ensure!(
        options
            .command
            .iter()
            .chain(std::iter::once(&options.check))
            .chain(options.cleanup.iter())
            .all(|value| !value.contains('\0')),
        "service commands must not contain NUL bytes"
    );
    let definitions = root.join("s6-rc.d");
    fs::create_dir_all(&definitions)?;
    let destination = definitions.join(name);
    ensure!(
        fs::symlink_metadata(&destination).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "service {name} already exists or cannot be inspected"
    );
    let bundles = root.join("user-bundles.d");
    for bundle in ["mutable-app", "user2"] {
        ensure!(
            !definitions.join(bundle).exists(),
            "{bundle} already exists in s6-rc.d; configure this bundle manually"
        );
        let directory = bundles.join(bundle);
        fs::create_dir_all(directory.join("contents.d"))?;
        let kind = directory.join("type");
        if kind.exists() {
            ensure!(
                fs::read_to_string(&kind)?.trim() == "bundle",
                "{bundle} must be a bundle"
            );
        } else {
            write(&kind, "bundle\n", 0o644)?;
        }
    }
    // Finish generating the service privately before exposing its definition.
    let staged = tempfile::tempdir_in(&definitions)?;
    let service = staged.path();
    fs::create_dir(service.join("data"))?;
    fs::create_dir(service.join("dependencies.d"))?;
    // The preset archive and named services use these same versioned files.
    for (file, content, mode) in [
        ("type", include_str!("service/type"), 0o644),
        (
            "notification-fd",
            include_str!("service/notification-fd"),
            0o644,
        ),
        ("timeout-kill", include_str!("service/timeout-kill"), 0o644),
        (
            "timeout-finish",
            include_str!("service/timeout-finish"),
            0o644,
        ),
        (
            "flag-timeout-killpg",
            include_str!("service/flag-timeout-killpg"),
            0o644,
        ),
        (
            "dependencies.d/base",
            include_str!("service/dependencies.d/base"),
            0o644,
        ),
        (
            "dependencies.d/mutator-prepare",
            include_str!("service/dependencies.d/mutator-prepare"),
            0o644,
        ),
        ("run", include_str!("service/run"), 0o755),
        ("data/launch", include_str!("service/data/launch"), 0o755),
        ("data/check", include_str!("service/data/check"), 0o755),
        (
            "data/dispatch",
            include_str!("service/data/dispatch"),
            0o755,
        ),
        ("finish", include_str!("service/finish"), 0o755),
    ] {
        write(&service.join(file), content, mode)?;
    }
    let command = options
        .command
        .iter()
        .map(|part| quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    write(
        &service.join("data/run-hook"),
        &format!("#!/bin/sh\nexec {command}\n"),
        0o755,
    )?;
    write(
        &service.join("data/check-hook"),
        &format!("#!/bin/sh\nexec /bin/sh -c {}\n", quote(&options.check)),
        0o755,
    )?;
    if let Some(cleanup) = &options.cleanup {
        write(
            &service.join("data/cleanup-hook"),
            &format!("#!/bin/sh\nexec /bin/sh -c {}\n", quote(cleanup)),
            0o755,
        )?;
    }
    fs::set_permissions(service, fs::Permissions::from_mode(0o755))?;
    fs::rename(service, &destination).context("cannot publish service definition")?;
    write(
        &bundles.join("mutable-app/contents.d").join(name),
        "",
        0o644,
    )?;
    write(&bundles.join("user2/contents.d/mutable-app"), "", 0o644)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn options(name: &str) -> Options {
        Options {
            name: name.into(),
            check: "test -f ready".into(),
            cleanup: None,
            command: vec!["/bin/echo".into(), "a 'quote' ; $(exit 9)".into()],
        }
    }

    #[test]
    fn generated_commands_preserve_arguments_and_follow_app_dir() {
        let root = tempfile::tempdir().unwrap();
        install_at(root.path(), &options("web")).unwrap();
        let service = root.path().join("s6-rc.d/web");
        let app = tempfile::tempdir().unwrap();
        fs::write(app.path().join("ready"), "").unwrap();
        let launched = Command::new(service.join("data/launch"))
            .env("MUTATOR_APP_DIR", app.path())
            .output()
            .unwrap();
        assert!(launched.status.success());
        assert_eq!(
            String::from_utf8(launched.stdout).unwrap(),
            "a 'quote' ; $(exit 9)\n"
        );
        let checked = Command::new(service.join("data/check"))
            .env("MUTATOR_APP_DIR", app.path())
            .status()
            .unwrap();
        assert!(checked.success());
        fs::remove_file(app.path().join("ready")).unwrap();
        assert!(
            !Command::new(service.join("data/check"))
                .env("MUTATOR_APP_DIR", app.path())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn preset_validates_configuration_and_runs_shell_commands() {
        let root = tempfile::tempdir().unwrap();
        install_at(root.path(), &options("web")).unwrap();
        let data = root.path().join("s6-rc.d/web/data");
        fs::remove_file(data.join("run-hook")).unwrap();
        fs::remove_file(data.join("check-hook")).unwrap();
        fs::write(data.join("preset"), "").unwrap();
        let app = tempfile::tempdir().unwrap();
        let run = |action: &str, command: &str, check: &str| {
            Command::new(data.join("dispatch"))
                .arg(action)
                .env("MUTATOR_APP_DIR", app.path())
                .env("MUTATOR_COMMAND", command)
                .env("MUTATOR_CHECK", check)
                .env("MUTATOR_CLEANUP", "")
                .output()
                .unwrap()
        };
        assert!(!run("validate", "", "true").status.success());
        assert!(!run("validate", "true", " \t").status.success());
        assert!(
            run(
                "validate",
                "exec printf '%s' ready > ready",
                "test -f ready"
            )
            .status
            .success()
        );
        assert!(!run("check", "true", "test -f ready").status.success());
        assert!(
            run("run", "exec printf '%s' ready > ready", "true")
                .status
                .success()
        );
        assert!(run("check", "true", "test -f ready").status.success());
        assert!(run("cleanup", "true", "true").status.success());
        // Per-service hooks have priority and must be executable.
        write(&data.join("run-hook"), "#!/bin/sh\nexit 7\n", 0o755).unwrap();
        assert_eq!(run("run", "true", "true").status.code(), Some(7));
        fs::set_permissions(data.join("run-hook"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!run("validate", "true", "true").status.success());
    }

    #[test]
    fn services_share_a_bundle_without_overwriting_existing_definitions() {
        let root = tempfile::tempdir().unwrap();
        install_at(root.path(), &options("web")).unwrap();
        install_at(root.path(), &options("worker")).unwrap();
        assert!(install_at(root.path(), &options("web")).is_err());
        for name in ["web", "worker"] {
            assert!(
                root.path()
                    .join("user-bundles.d/mutable-app/contents.d")
                    .join(name)
                    .exists()
            );
            assert!(
                root.path()
                    .join("s6-rc.d")
                    .join(name)
                    .join("dependencies.d/mutator-prepare")
                    .exists()
            );
        }
        assert!(
            root.path()
                .join("user-bundles.d/user2/contents.d/mutable-app")
                .exists()
        );
        assert!(
            !root
                .path()
                .join("user-bundles.d/user/contents.d/mutable-app")
                .exists()
        );
        for name in ["../bad", "mutator-watch", "mutable-app", "user2", "a b"] {
            assert!(install_at(root.path(), &options(name)).is_err());
        }
    }
}

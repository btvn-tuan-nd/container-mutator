//! Application paths are configurable; the supervisor and release store are not.
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

pub const STORE: &str = "/var/lib/mutator";

pub fn default_app_dir() -> PathBuf {
    PathBuf::from("/app")
}

pub fn from_env() -> Result<PathBuf> {
    let path = match std::env::var("MUTATOR_APP_DIR") {
        Ok(value) => PathBuf::from(value),
        Err(std::env::VarError::NotPresent) => default_app_dir(),
        Err(error) => return Err(error).context("MUTATOR_APP_DIR must be UTF-8"),
    };
    validate(&path)?;
    Ok(path)
}

pub fn validate(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "MUTATOR_APP_DIR must be absolute");
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
            && !path.as_os_str().as_encoded_bytes().contains(&0),
        "MUTATOR_APP_DIR must not contain parent components or NUL bytes"
    );
    for protected in [
        "/init",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/usr",
        "/etc",
        "/dev",
        "/proc",
        "/sys",
        "/run",
        "/nix",
        "/package",
        "/command",
        "/opt/mutator",
        STORE,
    ] {
        let protected = Path::new(protected);
        ensure!(
            !path.starts_with(protected) && !protected.starts_with(path),
            "MUTATOR_APP_DIR {} overlaps protected path {}",
            path.display(),
            protected.display()
        );
    }
    Ok(())
}

/// Reject aliases in existing parent directories, even when the final app
/// path is our own release symlink. Storage parents may not exist yet.
pub fn validate_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("application directory has no parent")?;
    for ancestor in parent.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "application and storage parents must be real directories, not symlinks: {}",
                ancestor.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect {}", ancestor.display()));
            }
        }
    }
    Ok(())
}

pub fn ensure_no_mounts(app: &Path) -> Result<()> {
    check_mounts(
        &fs::read_to_string("/proc/self/mountinfo")?,
        app,
        Path::new(STORE),
    )
}

fn check_mounts(mountinfo: &str, app: &Path, store: &Path) -> Result<()> {
    for line in mountinfo.lines() {
        let encoded = line
            .split_ascii_whitespace()
            .nth(4)
            .context("invalid mountinfo")?;
        // mountinfo escapes spaces, tabs, newlines and backslashes with octal.
        let mut decoded = Vec::new();
        let mut bytes = encoded.as_bytes().iter().copied();
        while let Some(byte) = bytes.next() {
            if byte == b'\\' {
                let digits: Vec<_> = bytes.by_ref().take(3).collect();
                ensure!(
                    digits.len() == 3 && digits.iter().all(|d| (b'0'..=b'7').contains(d)),
                    "invalid mount escape"
                );
                let value = digits.iter().fold(0u16, |n, d| n * 8 + u16::from(d - b'0'));
                decoded.push(u8::try_from(value).context("invalid mount escape")?);
            } else {
                decoded.push(byte);
            }
        }
        use std::os::unix::ffi::OsStrExt;
        let mount = Path::new(std::ffi::OsStr::from_bytes(&decoded));
        for root in [app, store] {
            ensure!(
                mount == Path::new("/") || (!mount.starts_with(root) && !root.starts_with(mount)),
                "update mode requires {} on the container root mount, without nested mounts (found {}); keep persistent data elsewhere",
                root.display(),
                mount.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_system_paths_and_ancestors_but_allows_application_trees() {
        for path in [
            "/",
            "/opt",
            "/var",
            "/var/lib",
            "/opt/mutator/app",
            "/etc/site",
            "/usr/share/nginx/html",
            "/command",
            "/package/admin",
            "/run/site",
            "/srv/../etc",
            "relative",
            "",
        ] {
            assert!(validate(Path::new(path)).is_err(), "{path}");
        }
        for path in [
            "/app",
            "/srv/site",
            "/opt/gateway",
            "/workspace",
            "/var/www/site",
            "/opt/mutator-app",
        ] {
            validate(Path::new(path)).unwrap();
        }
    }

    #[test]
    fn rejects_parent_aliases() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", root.path().join("alias")).unwrap();
        assert!(validate_parent(&root.path().join("alias/app")).is_err());
        validate_parent(&root.path().join("app")).unwrap();
    }

    #[test]
    fn mount_detection_decodes_paths_and_uses_components() {
        let app = Path::new("/srv/my site");
        let store = Path::new(STORE);
        assert!(
            check_mounts(
                "1 0 0:1 / /srv/my\\040site/data rw - tmpfs tmpfs rw",
                app,
                store
            )
            .is_err()
        );
        assert!(
            check_mounts("1 0 0:1 / /srv/my\\040site rw - tmpfs tmpfs rw", app, store).is_err()
        );
        assert!(check_mounts("1 0 0:1 / /srv rw - ext4 /dev/root rw", app, store).is_err());
        assert!(check_mounts("1 0 0:1 / /var rw - ext4 /dev/root rw", app, store).is_err());
        check_mounts("1 0 0:1 / / rw - overlay overlay rw", app, store).unwrap();
        check_mounts(
            "1 0 0:1 / /srv/my\\040site-other rw - tmpfs tmpfs rw",
            app,
            store,
        )
        .unwrap();
        assert!(
            check_mounts("1 0 0:1 / /var/lib/mutator rw - tmpfs tmpfs rw", app, store).is_err()
        );
    }
}

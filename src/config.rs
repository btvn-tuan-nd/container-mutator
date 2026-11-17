use std::time::Duration;

use anyhow::{Context, Result, ensure};

pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: u64,
}

/// Update settings read from the native inherited environment.
pub struct Config {
    pub image: String,
    pub runtime: String,
    pub limits: Limits,
    pub interval: Duration,
    pub timeout: Duration,
    pub insecure: bool,
}

impl Config {
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("{name} must be UTF-8")),
        })
    }

    fn from_vars(value: impl Fn(&str) -> Result<Option<String>>) -> Result<Option<Self>> {
        let Some(image) = value("MUTATOR_IMAGE")? else {
            return Ok(None);
        };
        ensure!(!image.is_empty(), "MUTATOR_IMAGE must not be empty");
        let runtime = value("MUTATOR_RUNTIME")?
            .filter(|runtime| !runtime.trim().is_empty())
            .context("MUTATOR_RUNTIME is required when updates are enabled")?;
        let positive = |name: &str, default: u64| -> Result<u64> {
            let number = value(name)?
                .map(|value| value.parse::<u64>())
                .transpose()
                .with_context(|| format!("{name} must be a positive integer"))?
                .unwrap_or(default);
            ensure!(number > 0, "{name} must be positive");
            Ok(number)
        };
        let timeout_seconds = positive("MUTATOR_TIMEOUT", 30)?;
        ensure!(
            timeout_seconds
                .checked_mul(1000)
                .is_some_and(|millis| millis <= u64::from(u32::MAX)),
            "MUTATOR_TIMEOUT exceeds the s6 timeout range"
        );
        Ok(Some(Self {
            image,
            runtime,
            limits: Limits {
                max_bytes: positive("MUTATOR_MAX_BYTES", 4 * 1024 * 1024 * 1024)?,
                max_entries: positive("MUTATOR_MAX_ENTRIES", 200_000)?,
            },
            interval: Duration::from_secs(positive("MUTATOR_POLL_INTERVAL", 60)?),
            timeout: Duration::from_secs(timeout_seconds),
            insecure: value("MUTATOR_INSECURE_REGISTRY")?
                .map(|value| value.parse::<bool>())
                .transpose()
                .context("MUTATOR_INSECURE_REGISTRY must be true or false")?
                .unwrap_or(false),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(values: &[(&str, &str)]) -> Result<Option<Config>> {
        Config::from_vars(|name| {
            Ok(values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned()))
        })
    }

    #[test]
    fn updates_are_opt_in_and_require_a_runtime() {
        assert!(
            configured(&[("MUTATOR_RUNTIME", "node22")])
                .unwrap()
                .is_none()
        );
        assert!(configured(&[("MUTATOR_IMAGE", "registry.example/app:live")]).is_err());
        assert!(configured(&[("MUTATOR_IMAGE", "")]).is_err());
        assert!(
            configured(&[
                ("MUTATOR_IMAGE", "registry.example/app:live"),
                ("MUTATOR_RUNTIME", " ")
            ])
            .is_err()
        );
    }

    #[test]
    fn timeout_and_resource_bounds_are_validated() {
        let update = |values: &[(&str, &str)]| {
            let mut all = vec![
                ("MUTATOR_IMAGE", "registry.example/app:live"),
                ("MUTATOR_RUNTIME", "node22"),
            ];
            all.extend_from_slice(values);
            configured(&all)
        };
        assert_eq!(
            update(&[("MUTATOR_TIMEOUT", "4294967")])
                .unwrap()
                .unwrap()
                .timeout,
            Duration::from_secs(4294967)
        );
        for (key, value) in [
            ("MUTATOR_TIMEOUT", "0"),
            ("MUTATOR_TIMEOUT", "4294968"),
            ("MUTATOR_MAX_BYTES", "0"),
            ("MUTATOR_MAX_ENTRIES", "-1"),
            ("MUTATOR_INSECURE_REGISTRY", "yes"),
        ] {
            assert!(update(&[(key, value)]).is_err(), "{key}={value}");
        }
    }
}

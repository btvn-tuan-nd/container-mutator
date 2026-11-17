//! Crane owns registry transport, authentication, digest verification and retries.
//! This adapter only bounds its output and validates the snapshot contract.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::config::{Config, Limits};
use crate::error::invalid;

const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;
const FORMAT_LABEL: &str = "io.container-mutator.format";
const RUNTIME_LABEL: &str = "io.container-mutator.runtime";
const SNAPSHOT_FORMAT: &str = "1";

pub struct Candidate {
    pub digest: String,
    pinned: String,
    repository: String,
}

pub struct Layer {
    pub digest: String,
    pub media_type: String,
    pub size: u64,
    reference: String,
}

pub struct Registry<'a> {
    config: &'a Config,
    platform: &'static str,
    architecture: &'static str,
}

impl<'a> Registry<'a> {
    pub fn new(config: &'a Config) -> Result<Self> {
        let (platform, architecture) = match std::env::consts::ARCH {
            "x86_64" => ("linux/amd64", "amd64"),
            "aarch64" => ("linux/arm64", "arm64"),
            other => bail!("unsupported container architecture {other}"),
        };
        Ok(Self {
            config,
            platform,
            architecture,
        })
    }

    pub fn resolve(&self) -> Result<Candidate> {
        let pinned = String::from_utf8(self.capture(&[
            "digest",
            "--full-ref",
            "--platform",
            self.platform,
            "--",
            &self.config.image,
        ])?)?
        .trim_end()
        .to_owned();
        let (repository, digest) = pinned
            .rsplit_once('@')
            .context("crane returned no digest")?;
        Ok(Candidate {
            digest: digest.to_owned(),
            repository: repository.to_owned(),
            pinned,
        })
    }

    pub fn validate(&self, candidate: &Candidate, runtime: &str, limits: &Limits) -> Result<Layer> {
        let snapshot = select_snapshot(
            &self.capture(&["manifest", "--", &candidate.pinned])?,
            limits.max_bytes,
        )?;
        let config_ref = format!("{}@{}", candidate.repository, snapshot.config.digest);
        let config = self.capture(&["blob", "--", &config_ref])?;
        ensure!(
            config.len() as u64 == snapshot.config.size,
            invalid("image config size does not match its descriptor")
        );
        validate_image_config(&config, runtime, self.architecture)?;
        let [layer] = snapshot.layers;
        Ok(Layer {
            reference: format!("{}@{}", candidate.repository, layer.digest),
            digest: layer.digest,
            media_type: layer.media_type,
            size: layer.size,
        })
    }

    pub fn download_layer(&self, layer: &Layer, destination: &Path) -> Result<()> {
        crate::releases::ensure_space(
            destination.parent().context("layer has no parent")?,
            layer.size,
        )?;
        // The caller owns the private staging directory and discards it on failure.
        let mut file = File::create(destination)?;
        let size = self
            .fetch_unchecked(&["blob", "--", &layer.reference], &mut file, layer.size)
            .with_context(|| format!("cannot download {}", layer.digest))?;
        ensure!(
            size == layer.size,
            invalid("snapshot size does not match its descriptor")
        );
        Ok(())
    }

    fn capture(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.fetch(args, &mut bytes, MAX_CONFIG_BYTES, true)?;
        Ok(bytes)
    }

    /// Blob download: the output is unverified until crane succeeds and the
    /// digest is checked, so any transport fault, including oversize output,
    /// is transient and retried on the next poll, never quarantined.
    fn fetch_unchecked(&self, args: &[&str], output: &mut impl Write, limit: u64) -> Result<u64> {
        self.fetch(args, output, limit, false)
    }

    fn fetch(
        &self,
        args: &[&str],
        output: &mut impl Write,
        limit: u64,
        strict: bool,
    ) -> Result<u64> {
        let mut command = Command::new("/opt/mutator/bin/crane");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if self.config.insecure {
            command.arg("--insecure");
        }
        let mut child = command.args(args).spawn().context("cannot start crane")?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let result = (|| -> Result<u64> {
            let count = io::copy(&mut stdout.by_ref().take(limit), output)?;
            // Probe for overflow without writing an extra byte into the output.
            // Metadata must validate against the contract; a layer blob has
            // not been digest-verified yet, so oversize there is a transport
            // fault to retry, not a deterministic rejection.
            if stdout.read(&mut [0])? != 0 {
                if strict {
                    return Err(invalid(format!("crane output exceeds {limit} bytes")));
                }
                bail!("crane output exceeds {limit} bytes");
            }
            Ok(count)
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().context("cannot wait for crane")?;
        let count = result?;
        ensure!(status.success(), "crane {} failed: {status}", args[0]);
        Ok(count)
    }
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    config: Descriptor,
    layers: [Descriptor; 1],
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
}

fn select_snapshot(bytes: &[u8], max_bytes: u64) -> Result<Snapshot> {
    let snapshot: Snapshot = serde_json::from_slice(bytes).map_err(|error| {
        invalid(format!(
            "expected a single-layer snapshot manifest: {error}"
        ))
    })?;
    ensure!(
        matches!(
            snapshot.config.media_type.as_str(),
            "application/vnd.oci.image.config.v1+json"
                | "application/vnd.docker.container.image.v1+json"
        ),
        invalid("unsupported image config media type")
    );
    ensure!(
        snapshot.config.size <= MAX_CONFIG_BYTES,
        invalid("image config exceeds metadata limit")
    );
    crate::extract::compression(&snapshot.layers[0].media_type)?;
    ensure!(
        snapshot.layers[0].size <= max_bytes,
        invalid("snapshot exceeds compressed size limit")
    );
    Ok(snapshot)
}

#[derive(Deserialize)]
struct ImageConfig {
    architecture: String,
    os: String,
    config: ContainerConfig,
}

#[derive(Deserialize)]
struct ContainerConfig {
    #[serde(default, rename = "Labels")]
    labels: HashMap<String, String>,
}

fn validate_image_config(bytes: &[u8], runtime: &str, target_arch: &str) -> Result<()> {
    let config: ImageConfig = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("invalid image config: {error}")))?;
    ensure!(
        config.os == "linux" && config.architecture == target_arch,
        invalid(format!(
            "image requires {}/{}; expected linux/{target_arch}",
            config.os, config.architecture
        ))
    );
    for (label, expected) in [(FORMAT_LABEL, SNAPSHOT_FORMAT), (RUNTIME_LABEL, runtime)] {
        ensure!(
            config.config.labels.get(label).map(String::as_str) == Some(expected),
            invalid(format!("image requires {label}={expected:?}"))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::InvalidRelease;

    const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
    const LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

    fn assert_invalid(error: anyhow::Error) {
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<InvalidRelease>().is_some()),
            "{error:#}"
        );
    }

    fn descriptor(media_type: &str, size: i64) -> String {
        format!(r#"{{"mediaType":"{media_type}","digest":"{DIGEST}","size":{size}}}"#)
    }

    fn manifest(config: &str, layers: &[String]) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"config":{config},"layers":[{}]}}"#,
            layers.join(",")
        )
        .into_bytes()
    }

    fn supported_manifest(layers: &[String]) -> Vec<u8> {
        manifest(&descriptor(CONFIG_MEDIA_TYPE, 2), layers)
    }

    #[test]
    fn snapshot_must_be_one_supported_layer_within_limits() {
        let layer = descriptor(LAYER_GZIP, 10);
        let snapshot =
            select_snapshot(&supported_manifest(std::slice::from_ref(&layer)), 10).unwrap();
        assert_eq!(snapshot.layers[0].size, 10);

        for (bytes, limit) in [
            (supported_manifest(&[]), 10),
            (supported_manifest(&[layer.clone(), layer.clone()]), 10),
            (
                supported_manifest(&[descriptor("application/x-unknown", 10)]),
                10,
            ),
            (supported_manifest(std::slice::from_ref(&layer)), 9),
            (supported_manifest(&[descriptor(LAYER_GZIP, -1)]), 10),
            (
                manifest(
                    &descriptor(CONFIG_MEDIA_TYPE, MAX_CONFIG_BYTES as i64 + 1),
                    std::slice::from_ref(&layer),
                ),
                10,
            ),
        ] {
            assert_invalid(select_snapshot(&bytes, limit).unwrap_err());
        }
    }

    #[test]
    fn config_must_match_format_runtime_and_native_platform() {
        let config = |os: &str, arch: &str, format: &str, runtime: &str| {
            format!(
                r#"{{"architecture":"{arch}","os":"{os}","config":{{"Labels":{{"{FORMAT_LABEL}":"{format}","{RUNTIME_LABEL}":"{runtime}"}}}}}}"#
            )
        };
        assert!(
            validate_image_config(
                config("linux", "amd64", SNAPSHOT_FORMAT, "base-1").as_bytes(),
                "base-1",
                "amd64"
            )
            .is_ok()
        );

        for invalid in [
            config("linux", "amd64", "2", "base-1"),
            config("linux", "amd64", SNAPSHOT_FORMAT, "base-2"),
            config("windows", "amd64", SNAPSHOT_FORMAT, "base-1"),
            config("linux", "arm64", SNAPSHOT_FORMAT, "base-1"),
            r#"{"architecture":"amd64","os":"linux","config":{}}"#.to_owned(),
            r#"{"architecture":"amd64","os":"linux"}"#.to_owned(),
            "not json".to_owned(),
        ] {
            assert_invalid(
                validate_image_config(invalid.as_bytes(), "base-1", "amd64").unwrap_err(),
            );
        }
    }
}

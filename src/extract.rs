//! Extract one complete `/app` snapshot archive into a fresh payload directory.
//!
//! A snapshot has no layered-rootfs semantics: every archive member must be
//! below `app/` (a root directory header is the only other allowed entry).
//! Whiteouts, duplicates, special files, unsafe paths and payload outside
//! `app/` are rejected as deterministic
//! [`InvalidRelease`](crate::error::InvalidRelease) violations. Symlinks are
//! stored verbatim and never followed; hardlinks may only resolve to regular
//! files extracted from this same snapshot.
//!
//! Extraction streams once. `Limits::max_bytes` bounds the compressed layer,
//! every decompressed byte including headers and padding, and file output;
//! `Limits::max_entries` bounds archive members. Paths, link targets and
//! per-entry allocations are also capped. Gzip archives are decoded with
//! `MultiGzDecoder`, and the decoder is drained through EOF so concatenated
//! members cannot smuggle a second archive past the tar end marker.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink as make_symlink};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::MultiGzDecoder;
use tar::{Archive, EntryType};

use crate::config::Limits;
use crate::error::invalid;

const APP_ROOT: &str = "app";
const MAX_PATH_BYTES: usize = 4095;
const MAX_LINK_BYTES: usize = 4095;
const MAX_ENTRY_METADATA_BYTES: u64 = 64 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const TRAILER_BUFFER_BYTES: usize = 8 * 1024;

/// Streams a complete `/app` snapshot into an empty, existing payload directory.
pub fn extract_snapshot(
    layer_path: &Path,
    media_type: &str,
    payload: &Path,
    limits: &Limits,
) -> Result<()> {
    let metadata = fs::symlink_metadata(payload)
        .with_context(|| format!("cannot inspect payload directory {payload:?}"))?;
    ensure!(
        metadata.file_type().is_dir(),
        "payload path {payload:?} is not a real directory"
    );
    ensure!(
        fs::read_dir(payload)
            .with_context(|| format!("cannot read payload directory {payload:?}"))?
            .next()
            .is_none(),
        "payload directory {payload:?} is not empty"
    );

    let layer_size = fs::metadata(layer_path)
        .with_context(|| format!("cannot inspect layer archive {layer_path:?}"))?
        .len();
    if layer_size > limits.max_bytes {
        return Err(invalid(format!(
            "compressed layer archive is {layer_size} bytes, exceeding the {} byte limit",
            limits.max_bytes
        )));
    }

    crate::releases::ensure_space(payload, 0)?;

    let compression = compression(media_type)?;
    let guards = Rc::new(Guards::default());
    let file = File::open(layer_path)
        .with_context(|| format!("cannot open layer archive {layer_path:?}"))?;
    let source = LocalReader {
        inner: BufReader::new(file),
        guards: Rc::clone(&guards),
    };
    let decoded: Box<dyn Read> = match compression {
        Compression::None => Box::new(source),
        Compression::Gzip => Box::new(MultiGzDecoder::new(source)),
        Compression::Zstd => Box::new(
            zstd::stream::read::Decoder::new(source)
                .map_err(|error| archive_error(error, &guards, limits.max_bytes))?,
        ),
    };
    let reader = LimitedReader {
        inner: decoded,
        remaining: limits.max_bytes,
        guards: Rc::clone(&guards),
    };
    let stage = Stage {
        payload,
        limits,
        guards,
        nodes: HashMap::new(),
        entries: 0,
    };
    stage.run(reader)
}

#[derive(Clone, Copy)]
pub(crate) enum Compression {
    None,
    Gzip,
    Zstd,
}

pub(crate) fn compression(media_type: &str) -> Result<Compression> {
    Ok(match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar" => Compression::None,
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => Compression::Gzip,
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd"
        | "application/vnd.docker.image.rootfs.diff.tar.zstd"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.zstd" => Compression::Zstd,
        other => return Err(invalid(format!("unsupported layer media type {other:?}"))),
    })
}

#[derive(Default)]
struct Guards {
    local_io: Cell<bool>,
    exceeded: Cell<bool>,
    metadata_remaining: Cell<Option<u64>>,
}

fn archive_error(error: io::Error, guards: &Guards, max_bytes: u64) -> anyhow::Error {
    if guards.local_io.get() {
        anyhow::Error::new(error).context("cannot read layer archive")
    } else if guards.exceeded.get() {
        invalid(format!(
            "decompressed layer archive exceeds the {max_bytes} byte limit"
        ))
    } else {
        invalid(format!("invalid layer archive: {error}"))
    }
}

/// Marks an error from the local layer file so it stays transient instead of
/// being mistaken for a malformed candidate.
struct LocalReader<R> {
    inner: R,
    guards: Rc<Guards>,
}

impl<R: Read> Read for LocalReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(buffer);
        if result.is_err() {
            self.guards.local_io.set(true);
        }
        result
    }
}

/// Bounds every decompressed byte read from the archive.
struct LimitedReader<R> {
    inner: R,
    remaining: u64,
    guards: Rc<Guards>,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.guards.exceeded.set(true);
                    Err(io::Error::other("decompressed layer size limit exceeded"))
                }
            };
        }
        let metadata = self.guards.metadata_remaining.get();
        if metadata == Some(0) {
            return Err(io::Error::other(
                "archive metadata exceeds 64 KiB per entry",
            ));
        }
        let limit = self
            .remaining
            .min(metadata.unwrap_or(u64::MAX))
            .min(buffer.len() as u64) as usize;
        let count = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= count as u64;
        if let Some(remaining) = metadata {
            self.guards
                .metadata_remaining
                .set(Some(remaining - count as u64));
        }
        Ok(count)
    }
}

enum Node {
    File,
    Directory(Option<u32>),
    Symlink,
    Hardlink(PathBuf),
}

struct Stage<'a> {
    payload: &'a Path,
    limits: &'a Limits,
    guards: Rc<Guards>,
    nodes: HashMap<PathBuf, Node>,
    entries: u64,
}

impl Stage<'_> {
    fn run<R: Read>(mut self, reader: R) -> Result<()> {
        let mut archive = Archive::new(reader);
        {
            let entries = match archive.entries() {
                Ok(entries) => entries,
                Err(error) => return Err(self.fail(error)),
            };
            self.guards
                .metadata_remaining
                .set(Some(MAX_ENTRY_METADATA_BYTES));
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return Err(self.fail(error)),
                };
                self.guards.metadata_remaining.set(None);
                self.add(entry)?;
                self.guards
                    .metadata_remaining
                    .set(Some(MAX_ENTRY_METADATA_BYTES));
            }
        }
        self.guards.metadata_remaining.set(None);
        // Tar stops at its end marker; force the decoder through gzip/zstd
        // trailers and reject any non-zero content after the snapshot.
        let mut reader = archive.into_inner();
        let mut buffer = [0u8; TRAILER_BUFFER_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) if buffer[..count].iter().all(|byte| *byte == 0) => {}
                Ok(_) => {
                    return Err(invalid(
                        "layer archive has non-zero data after its end-of-archive marker",
                    ));
                }
                Err(error) => return Err(self.fail(error)),
            }
        }
        self.finish()
    }

    fn fail(&self, error: io::Error) -> anyhow::Error {
        archive_error(error, &self.guards, self.limits.max_bytes)
    }

    fn add<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<()> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(invalid(format!(
                "layer archive has more than {} entries",
                self.limits.max_entries
            )));
        }
        if let Some(fields) = entry.pax_extensions().map_err(|error| self.fail(error))? {
            for field in fields {
                if field
                    .map_err(|error| self.fail(error))?
                    .key_bytes()
                    .starts_with(b"GNU.sparse.")
                {
                    return Err(invalid("sparse files are not supported"));
                }
            }
        }

        let parsed = {
            let path = entry.path_bytes();
            parse_path(&path)?
        };
        let relative = match parsed {
            ArchivePath::Root => {
                if entry.header().entry_type() != EntryType::Directory {
                    return Err(invalid("layer archive root header is not a directory"));
                }
                return Ok(());
            }
            ArchivePath::App(relative) => relative,
        };

        if relative.as_os_str().is_empty() {
            if entry.header().entry_type() != EntryType::Directory {
                return Err(invalid("layer archive attempts to replace the app root"));
            }
            let mode = mode(&entry, &relative)?;
            return self.directory(relative, mode);
        }

        match entry.header().entry_type() {
            EntryType::Directory => {
                let mode = mode(&entry, &relative)?;
                self.directory(relative, mode)
            }
            EntryType::Regular | EntryType::Continuous => {
                let mode = mode(&entry, &relative)?;
                self.file(relative, mode, &mut entry)
            }
            EntryType::Symlink => self.symlink(relative, &entry),
            EntryType::Link => self.hardlink(relative, &entry),
            other => Err(invalid(format!(
                "unsupported {other:?} entry {relative:?}; snapshots allow only files, directories, symlinks and hardlinks"
            ))),
        }
    }

    fn directory(&mut self, relative: PathBuf, mode: u32) -> Result<()> {
        if self
            .nodes
            .get(&relative)
            .is_some_and(|node| !matches!(node, Node::Directory(None)))
        {
            return Err(duplicate(&relative));
        }
        self.parents(&relative)?;
        let path = self.payload.join(&relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(duplicate(&relative)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => create_dir(&path)?,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect directory {path:?}"));
            }
        }
        self.nodes.insert(relative, Node::Directory(Some(mode)));
        Ok(())
    }

    fn file<R: Read>(
        &mut self,
        relative: PathBuf,
        mode: u32,
        entry: &mut tar::Entry<'_, R>,
    ) -> Result<()> {
        if self.nodes.contains_key(&relative) {
            return Err(duplicate(&relative));
        }
        let size = entry.size();
        if size > self.limits.max_bytes {
            return Err(invalid(format!(
                "layer file {relative:?} exceeds the {} byte output limit",
                self.limits.max_bytes
            )));
        }
        self.parents(&relative)?;
        crate::releases::ensure_space(self.payload, size)?;
        let path = self.payload.join(&relative);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(duplicate(&relative));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create file {path:?}"));
            }
        };
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        let mut copied = 0u64;
        loop {
            let count = match entry.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) => return Err(self.fail(error)),
            };
            copied += count as u64;
            file.write_all(&buffer[..count])
                .with_context(|| format!("cannot write file {path:?}"))?;
        }
        if copied != size {
            return Err(invalid(format!(
                "layer file {relative:?} is truncated: expected {size} bytes, read {copied}"
            )));
        }
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot set mode on file {path:?}"))?;
        self.nodes.insert(relative, Node::File);
        Ok(())
    }

    fn symlink<R: Read>(&mut self, relative: PathBuf, entry: &tar::Entry<'_, R>) -> Result<()> {
        if self.nodes.contains_key(&relative) {
            return Err(duplicate(&relative));
        }
        let target = entry
            .link_name_bytes()
            .ok_or_else(|| invalid(format!("symlink {relative:?} has no target")))?;
        if target.is_empty() || target.len() > MAX_LINK_BYTES || target.contains(&0) {
            return Err(invalid(format!("invalid symlink target for {relative:?}")));
        }
        let target = Path::new(OsStr::from_bytes(&target)).to_path_buf();
        self.parents(&relative)?;
        let path = self.payload.join(&relative);
        match make_symlink(&target, &path) {
            Ok(()) => {
                self.nodes.insert(relative, Node::Symlink);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(duplicate(&relative)),
            Err(error) => Err(error).with_context(|| format!("cannot create symlink {path:?}")),
        }
    }

    fn hardlink<R: Read>(&mut self, relative: PathBuf, entry: &tar::Entry<'_, R>) -> Result<()> {
        if self.nodes.contains_key(&relative) {
            return Err(duplicate(&relative));
        }
        let target = {
            let Some(bytes) = entry.link_name_bytes() else {
                return Err(invalid(format!("hardlink {relative:?} has no target")));
            };
            match parse_path(&bytes)? {
                ArchivePath::App(target) if !target.as_os_str().is_empty() => target,
                _ => {
                    return Err(invalid(format!(
                        "hardlink {relative:?} targets a path outside app/"
                    )));
                }
            }
        };
        if target == relative {
            return Err(invalid(format!("hardlink {relative:?} targets itself")));
        }
        self.nodes.insert(relative, Node::Hardlink(target));
        Ok(())
    }

    /// Creates missing parent directories, refusing to traverse symlinks or
    /// files created earlier by the snapshot.
    fn parents(&mut self, relative: &Path) -> Result<()> {
        let Some(parent) = relative.parent() else {
            return Ok(());
        };
        let mut prefix = PathBuf::new();
        let mut path = self.payload.to_path_buf();
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err(invalid(format!("unsafe layer path {relative:?}")));
            };
            prefix.push(name);
            path.push(name);
            if self
                .nodes
                .get(&prefix)
                .is_some_and(|node| !matches!(node, Node::Directory(_)))
            {
                return Err(invalid(format!(
                    "layer path {relative:?} is nested below non-directory {prefix:?}"
                )));
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(invalid(format!(
                        "layer path {relative:?} traverses non-directory {prefix:?}"
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    create_dir(&path)?;
                    self.nodes.insert(prefix.clone(), Node::Directory(None));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot inspect {path:?}"));
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.create_hardlinks()?;
        let mut directories: Vec<(PathBuf, u32)> = self
            .nodes
            .into_iter()
            .filter_map(|(path, node)| match node {
                Node::Directory(mode) => Some((path, mode.unwrap_or(0o755))),
                _ => None,
            })
            .collect();
        // Children first: restrictive parents must not block the final chmods.
        directories.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        for (relative, mode) in directories {
            set_dir_mode(&self.payload.join(relative), mode)?;
        }
        Ok(())
    }

    /// Resolves every hardlink chain to a regular file of this snapshot before
    /// creating links. State marking plus path sharing keeps this linear.
    fn create_hardlinks(&mut self) -> Result<()> {
        let links: Vec<(PathBuf, PathBuf)> = self
            .nodes
            .iter()
            .filter_map(|(path, node)| match node {
                Node::Hardlink(target) => Some((path.clone(), target.clone())),
                _ => None,
            })
            .collect();
        if links.is_empty() {
            return Ok(());
        }
        let index: HashMap<&Path, usize> = links
            .iter()
            .enumerate()
            .map(|(index, link)| (link.0.as_path(), index))
            .collect();
        let mut state = vec![0u8; links.len()];
        let mut resolved: Vec<Option<usize>> = vec![None; links.len()];
        for start in 0..links.len() {
            if state[start] == 2 {
                continue;
            }
            let mut chain = Vec::new();
            let mut current = start;
            let terminal = loop {
                match state[current] {
                    2 => break resolved[current].expect("hardlink already resolved"),
                    1 => return Err(invalid(format!("hardlink cycle at {:?}", links[current].0))),
                    _ => {}
                }
                state[current] = 1;
                chain.push(current);
                let target = &links[current].1;
                if let Some(&next) = index.get(target.as_path()) {
                    current = next;
                } else if matches!(self.nodes.get(target.as_path()), Some(Node::File)) {
                    break current;
                } else {
                    return Err(invalid(format!(
                        "hardlink {:?} target {target:?} is not a regular snapshot file",
                        links[current].0
                    )));
                }
            };
            for node in chain {
                resolved[node] = Some(terminal);
                state[node] = 2;
            }
        }
        for (index, link) in links.iter().enumerate() {
            let source_relative = &links[resolved[index].expect("hardlink was resolved")].1;
            let source = self.payload.join(source_relative);
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| format!("cannot inspect hardlink target {source:?}"))?;
            if !metadata.file_type().is_file() {
                return Err(invalid(format!(
                    "hardlink target {source_relative:?} is not a regular file"
                )));
            }
            self.parents(&link.0)?;
            let destination = self.payload.join(&link.0);
            match fs::hard_link(&source, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(duplicate(&link.0));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot create hardlink {destination:?}"));
                }
            }
        }
        Ok(())
    }
}

enum ArchivePath {
    Root,
    App(PathBuf),
}

fn parse_path(bytes: &[u8]) -> Result<ArchivePath> {
    if bytes.len() > MAX_PATH_BYTES {
        return Err(invalid(format!(
            "layer archive path exceeds {MAX_PATH_BYTES} bytes"
        )));
    }
    if bytes.contains(&0) {
        return Err(invalid("layer archive path contains a NUL byte"));
    }
    let archive_path = Path::new(OsStr::from_bytes(bytes));
    let mut relative = PathBuf::new();
    let mut first = false;
    for component in archive_path.components() {
        match component {
            Component::Normal(name) => {
                if name.as_bytes().starts_with(b".wh.") {
                    return Err(invalid(format!(
                        "layer archive contains whiteout entry {archive_path:?}"
                    )));
                }
                if !first {
                    first = true;
                    if name != OsStr::new(APP_ROOT) {
                        return Err(invalid(format!(
                            "layer archive entry {archive_path:?} is outside app/"
                        )));
                    }
                } else {
                    relative.push(name);
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid(format!(
                    "layer archive path {archive_path:?} contains a parent-directory component"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(format!(
                    "layer archive path {archive_path:?} is absolute"
                )));
            }
        }
    }
    Ok(if first {
        ArchivePath::App(relative)
    } else {
        ArchivePath::Root
    })
}

fn mode<R: Read>(entry: &tar::Entry<'_, R>, path: &Path) -> Result<u32> {
    entry
        .header()
        .mode()
        .map(|mode| mode & 0o777)
        .map_err(|error| invalid(format!("invalid mode for layer entry {path:?}: {error}")))
}

fn duplicate(path: &Path) -> anyhow::Error {
    invalid(format!("duplicate or conflicting layer path {path:?}"))
}

fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir(path).with_context(|| format!("cannot create directory {path:?}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot set mode on directory {path:?}"))
}

fn set_dir_mode(path: &Path, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .with_context(|| format!("cannot set mode on directory {path:?}"))
        }
        Ok(_) => bail!("staged path {path:?} is not a directory"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot inspect directory {path:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::{TempDir, tempdir};

    const TAR: &str = "application/vnd.oci.image.layer.v1.tar";
    const TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    const TAR_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

    enum Entry<'a> {
        File(&'a str, &'a [u8], u32),
        Dir(&'a str, u32),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        Special(&'a str, EntryType),
    }

    fn archive(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for item in entries {
            let mut header = tar::Header::new_gnu();
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_mode(0o644);
            header.set_size(0);
            match item {
                Entry::File(path, data, mode) => {
                    header.set_entry_type(EntryType::Regular);
                    header.set_mode(*mode);
                    header.set_size(data.len() as u64);
                    builder.append_data(&mut header, *path, *data).unwrap();
                }
                Entry::Dir(path, mode) => {
                    header.set_entry_type(EntryType::Directory);
                    header.set_mode(*mode);
                    builder
                        .append_data(&mut header, *path, io::empty())
                        .unwrap();
                }
                Entry::Symlink(path, target) => {
                    header.set_entry_type(EntryType::Symlink);
                    builder.append_link(&mut header, *path, *target).unwrap();
                }
                Entry::Hardlink(path, target) => {
                    header.set_entry_type(EntryType::Link);
                    builder.append_link(&mut header, *path, *target).unwrap();
                }
                Entry::Special(path, entry_type) => {
                    header.set_entry_type(*entry_type);
                    builder
                        .append_data(&mut header, *path, io::empty())
                        .unwrap();
                }
            }
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap()
    }

    fn raw_archive(path: &[u8], entry_type: EntryType) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o644);
        header.set_size(0);
        header.set_entry_type(entry_type);
        let bytes = header.as_mut_bytes();
        bytes[..100].fill(0);
        bytes[..path.len()].copy_from_slice(path);
        header.set_cksum();
        builder.append(&header, io::empty()).unwrap();
        builder.finish().unwrap();
        builder.into_inner().unwrap()
    }

    fn single(path: &str, entry_type: EntryType, target: Option<&str>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o644);
        header.set_size(0);
        header.set_entry_type(entry_type);
        match target {
            Some(target) => builder.append_link(&mut header, path, target).unwrap(),
            None => builder.append_data(&mut header, path, io::empty()).unwrap(),
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn limits() -> Limits {
        Limits {
            max_bytes: 1024 * 1024,
            max_entries: 64,
        }
    }

    fn payload() -> (TempDir, PathBuf) {
        let temp = tempdir().unwrap();
        let app = temp.path().join("app");
        fs::create_dir(&app).unwrap();
        (temp, app)
    }

    fn extract(root: &Path, name: &str, bytes: &[u8], media: &str, limits: &Limits) -> Result<()> {
        let layer = root.join(name);
        fs::write(&layer, bytes).unwrap();
        extract_snapshot(&layer, media, &root.join("app"), limits)
    }

    fn assert_invalid(result: Result<()>) {
        let error = result.expect_err("expected an invalid release");
        assert!(
            error.is::<crate::error::InvalidRelease>(),
            "not an InvalidRelease: {error:#}"
        );
    }

    fn rejected(entries: &[Entry<'_>]) {
        let (temp, _) = payload();
        assert_invalid(extract(
            temp.path(),
            "snapshot.tar",
            &archive(entries),
            TAR,
            &limits(),
        ));
    }

    fn assert_rejected(bytes: &[u8], media: &str, limits: &Limits) {
        let (temp, _) = payload();
        assert_invalid(extract(temp.path(), "snapshot", bytes, media, limits));
    }

    fn contents(payload: &Path, relative: &str) -> String {
        fs::read_to_string(payload.join(relative)).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn rejects_oversized_metadata_and_pax_sparse_files() {
        for (key, value) in [
            ("comment", vec![b'x'; MAX_ENTRY_METADATA_BYTES as usize]),
            ("GNU.sparse.realsize", b"1".to_vec()),
        ] {
            let mut builder = tar::Builder::new(Vec::new());
            builder
                .append_pax_extensions([(key, value.as_slice())])
                .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(1);
            builder
                .append_data(&mut header, "app/run", &b"x"[..])
                .unwrap();
            assert_rejected(&builder.into_inner().unwrap(), TAR, &limits());
        }
    }

    #[test]
    fn extracts_complete_snapshot_with_modes_and_links() {
        let (temp, app) = payload();
        let bytes = archive(&[
            Entry::Dir(".", 0o755),
            Entry::Dir("app", 0o750),
            Entry::Dir("app/bin", 0o750),
            Entry::File("app/bin/run", b"#!/bin/sh\n", 0o755),
            Entry::Dir("app/private", 0o555),
            Entry::File("app/private/data", b"secret", 0o640),
            Entry::File("app/implicit/child", b"child", 0o644),
            Entry::Symlink("app/alias", "/app/bin/run"),
            Entry::Symlink("app/relative", "bin/run"),
            Entry::Hardlink("app/chain", "app/hard"),
            Entry::Hardlink("app/hard", "app/bin/run"),
        ]);
        extract(temp.path(), "snapshot.tar", &bytes, TAR, &limits()).unwrap();

        assert_eq!(contents(&app, "bin/run"), "#!/bin/sh\n");
        assert_eq!(contents(&app, "private/data"), "secret");
        assert_eq!(contents(&app, "implicit/child"), "child");
        assert_eq!(mode_of(&app), 0o750);
        assert_eq!(mode_of(&app.join("bin")), 0o750);
        assert_eq!(mode_of(&app.join("bin/run")), 0o755);
        assert_eq!(mode_of(&app.join("private")), 0o555);
        assert_eq!(mode_of(&app.join("implicit")), 0o755);
        assert_eq!(
            fs::read_link(app.join("alias")).unwrap(),
            PathBuf::from("/app/bin/run")
        );
        assert_eq!(
            fs::read_link(app.join("relative")).unwrap(),
            PathBuf::from("bin/run")
        );
        let inode = fs::metadata(app.join("bin/run")).unwrap().ino();
        assert_eq!(fs::metadata(app.join("hard")).unwrap().ino(), inode);
        assert_eq!(fs::metadata(app.join("chain")).unwrap().ino(), inode);
        fs::set_permissions(app.join("private"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn rejects_wrong_snapshot_shapes_as_invalid_release() {
        rejected(&[Entry::File("etc/passwd", b"x", 0o644)]);
        rejected(&[Entry::File("app/.wh.secret", b"", 0o644)]);
        rejected(&[
            Entry::File("app/file", b"x", 0o644),
            Entry::File("app/file", b"y", 0o644),
        ]);
        rejected(&[Entry::Dir("app", 0o755), Entry::Dir("app", 0o755)]);
        rejected(&[Entry::Symlink("app", "/tmp")]);
        rejected(&[Entry::Special("app/fifo", EntryType::Fifo)]);
        rejected(&[Entry::Special("app/sparse", EntryType::GNUSparse)]);
        assert_rejected(&raw_archive(b".", EntryType::Regular), TAR, &limits());
        assert_rejected(
            &raw_archive(b"app/../escape", EntryType::Regular),
            TAR,
            &limits(),
        );
        assert_rejected(
            &raw_archive(b"/app/escape", EntryType::Regular),
            TAR,
            &limits(),
        );
        assert_rejected(b"irrelevant", "application/octet-stream", &limits());
    }

    #[test]
    fn enforces_byte_entry_path_and_link_limits() {
        let two = archive(&[
            Entry::File("app/a", b"a", 0o644),
            Entry::File("app/b", b"b", 0o644),
        ]);
        assert_rejected(
            &two,
            TAR,
            &Limits {
                max_bytes: 1024 * 1024,
                max_entries: 1,
            },
        );
        // The compressed layer itself exceeds the byte limit.
        assert_rejected(
            &two,
            TAR,
            &Limits {
                max_bytes: 1024,
                max_entries: 64,
            },
        );
        // Gzip is small, but the decompressed tar (headers included) exceeds 2048.
        let compressed = gzip(&two);
        assert!(compressed.len() < 2048);
        assert_rejected(
            &compressed,
            TAR_GZIP,
            &Limits {
                max_bytes: 2048,
                max_entries: 64,
            },
        );

        let long_path = format!("app/{}", "a".repeat(MAX_PATH_BYTES));
        assert_rejected(
            &single(&long_path, EntryType::Regular, None),
            TAR,
            &limits(),
        );
        let long_target = "b".repeat(MAX_LINK_BYTES + 1);
        assert_rejected(
            &single("app/link", EntryType::Symlink, Some(long_target.as_str())),
            TAR,
            &limits(),
        );
    }

    #[test]
    fn multigzip_is_one_stream_and_second_archive_is_rejected() {
        let bytes = archive(&[Entry::File("app/data", b"multi member data", 0o644)]);
        let split = bytes.len() / 2;
        let mut split_gzip = gzip(&bytes[..split]);
        split_gzip.extend(gzip(&bytes[split..]));
        let (temp, app) = payload();
        extract(
            temp.path(),
            "split.tar.gz",
            &split_gzip,
            TAR_GZIP,
            &limits(),
        )
        .unwrap();
        assert_eq!(contents(&app, "data"), "multi member data");

        let first = archive(&[Entry::File("app/first", b"first", 0o644)]);
        let second = archive(&[Entry::File("app/second", b"second", 0o644)]);
        let mut two_archives = gzip(&first);
        two_archives.extend(gzip(&second));
        let (temp, app) = payload();
        assert_invalid(extract(
            temp.path(),
            "two.tar.gz",
            &two_archives,
            TAR_GZIP,
            &limits(),
        ));
        assert!(app.join("first").is_file());
        assert!(!app.join("second").exists());

        let zstd_layer = zstd::stream::encode_all(first.as_slice(), 3).unwrap();
        let (temp, app) = payload();
        extract(
            temp.path(),
            "snapshot.tar.zst",
            &zstd_layer,
            TAR_ZSTD,
            &limits(),
        )
        .unwrap();
        assert_eq!(contents(&app, "first"), "first");
    }

    #[test]
    fn links_never_escape_or_cycle() {
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("victim"), b"safe").unwrap();
        let target = outside.path().to_str().unwrap().to_string();
        let (temp, app) = payload();
        let bytes = archive(&[
            Entry::Symlink("app/link", target.as_str()),
            Entry::File("app/link/victim", b"overwrite", 0o644),
        ]);
        assert_invalid(extract(temp.path(), "escape.tar", &bytes, TAR, &limits()));
        assert_eq!(
            fs::read(outside.path().join("victim")).unwrap(),
            b"safe".as_slice()
        );
        assert_eq!(
            fs::read_link(app.join("link")).unwrap(),
            PathBuf::from(target)
        );

        rejected(&[Entry::Hardlink("app/bad", "../../etc/passwd")]);
        rejected(&[Entry::Hardlink("app/bad", "/etc/passwd")]);
        rejected(&[Entry::Hardlink("app/bad", "etc/passwd")]);
        rejected(&[Entry::Hardlink("app/bad", "app/missing")]);
        rejected(&[
            Entry::Symlink("app/target", "elsewhere"),
            Entry::Hardlink("app/bad", "app/target"),
        ]);
        rejected(&[
            Entry::Dir("app/dir", 0o755),
            Entry::Hardlink("app/bad", "app/dir"),
        ]);
        rejected(&[
            Entry::Hardlink("app/a", "app/b"),
            Entry::Hardlink("app/b", "app/a"),
        ]);
    }
}

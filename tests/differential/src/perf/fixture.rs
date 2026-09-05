use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const LARGE_LINE_COUNT: usize = 50_000;
pub const LARGE_MIDPOINT: i64 = 25_000;
pub const PLUGIN_SINK_PER_PLUGIN: i64 = 20_100;
const PLUGIN_BODY: &[u8] =
    b"local s = 0 for i = 1, 200 do s = s + i end _G.__perf_sink = (_G.__perf_sink or 0) + s\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug)]
pub enum FixtureError {
    Io { path: PathBuf, source: io::Error },
    CountOverflow { count: usize },
    SinkOverflow { count: usize },
    Format(fmt::Error),
}

impl fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "fixture I/O at {}: {source}", path.display())
            }
            Self::CountOverflow { count } => write!(
                formatter,
                "plugin count {count} exceeds the fixture naming range"
            ),
            Self::SinkOverflow { count } => {
                write!(
                    formatter,
                    "plugin sink contribution overflows for count {count}"
                )
            }
            Self::Format(source) => write!(formatter, "could not format fixture content: {source}"),
        }
    }
}

impl std::error::Error for FixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Format(source) => Some(source),
            Self::CountOverflow { .. } | Self::SinkOverflow { .. } => None,
        }
    }
}

impl From<fmt::Error> for FixtureError {
    fn from(source: fmt::Error) -> Self {
        Self::Format(source)
    }
}

/// Return the exact `_G.__perf_sink` contribution from `count` generated plugins.
///
/// # Errors
///
/// Returns [`FixtureError::SinkOverflow`] if `count * 20_100` does not fit in
/// the RPC integer representation.
pub fn expected_plugin_sink(count: usize) -> Result<i64, FixtureError> {
    let rpc_count = i64::try_from(count).map_err(|_| FixtureError::SinkOverflow { count })?;
    rpc_count
        .checked_mul(PLUGIN_SINK_PER_PLUGIN)
        .ok_or(FixtureError::SinkOverflow { count })
}

/// Create a large text fixture at `dir/large.txt`.
///
/// The fixture contains `LARGE_LINE_COUNT` lines of the form
/// `{index:07} the quick brown fox jumps over the lazy dog`.
///
/// # Errors
///
/// Returns `FixtureError::Io` if creating `dir` or writing the file fails.
/// Returns `FixtureError::Format` if formatting the in-memory content fails.
pub fn large_buffer(dir: &Path) -> Result<Fixture, FixtureError> {
    fs::create_dir_all(dir).map_err(|source| io_error(dir, source))?;
    let path = dir.join("large.txt");
    let mut content = String::with_capacity(LARGE_LINE_COUNT.saturating_mul(52));
    for index in 0..LARGE_LINE_COUNT {
        writeln!(
            content,
            "{index:07} the quick brown fox jumps over the lazy dog"
        )?;
    }
    let sha256 = hash_bytes(content.as_bytes());
    write_if_changed(&path, content.as_bytes(), &sha256)?;
    Ok(Fixture { path, sha256 })
}

/// Create a runtime-path tree of `count` plugin fixture files under `dir/rtp-{count:03}`.
///
/// # Errors
///
/// Returns `FixtureError::CountOverflow` if `count` is greater than `999`.
/// Returns `FixtureError::Io` if directory or file operations fail.
pub fn plugin_tree(dir: &Path, count: usize) -> Result<Fixture, FixtureError> {
    if count > 999 {
        return Err(FixtureError::CountOverflow { count });
    }
    fs::create_dir_all(dir).map_err(|source| io_error(dir, source))?;
    let root = dir.join(format!("rtp-{count:03}"));
    let expected_hash = plugin_hash(count);
    if root.is_dir() && hash_plugin_tree(&root, count)?.as_deref() == Some(expected_hash.as_str()) {
        return Ok(Fixture {
            path: root,
            sha256: expected_hash,
        });
    }

    let temporary = dir.join(format!(".rtp-{count:03}-{}.tmp", std::process::id()));
    remove_generated_path(&temporary)?;
    let plugin_dir = temporary.join("plugin");
    fs::create_dir_all(&plugin_dir).map_err(|source| io_error(&plugin_dir, source))?;
    for index in 0..count {
        let path = plugin_dir.join(format!("{index:03}.lua"));
        fs::write(&path, PLUGIN_BODY).map_err(|source| io_error(&path, source))?;
    }
    remove_generated_path(&root)?;
    fs::rename(temporary, &root).map_err(|source| io_error(&root, source))?;
    Ok(Fixture {
        path: root,
        sha256: expected_hash,
    })
}

/// Compute the SHA-256 hex digest of the file at `path`.
///
/// # Errors
///
/// Returns `FixtureError::Io` if the file cannot be opened or read.
pub fn hash_file(path: &Path) -> Result<String, FixtureError> {
    let file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn write_if_changed(path: &Path, content: &[u8], expected_hash: &str) -> Result<(), FixtureError> {
    if path.is_file() && hash_file(path)? == expected_hash {
        return Ok(());
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    remove_generated_path(&temporary)?;
    let file = File::create(&temporary).map_err(|source| io_error(&temporary, source))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(content)
        .map_err(|source| io_error(&temporary, source))?;
    writer
        .flush()
        .map_err(|source| io_error(&temporary, source))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|source| io_error(&temporary, source))?;
    fs::rename(temporary, path).map_err(|source| io_error(path, source))
}

#[must_use]
pub fn plugin_hash(count: usize) -> String {
    let mut hasher = Sha256::new();
    for index in 0..count {
        hasher.update(format!("plugin/{index:03}.lua\0"));
        hasher.update(PLUGIN_BODY);
    }
    hex_digest(hasher.finalize().as_slice())
}

fn hash_plugin_tree(root: &Path, count: usize) -> Result<Option<String>, FixtureError> {
    let plugin_dir = root.join("plugin");
    let mut entries = fs::read_dir(&plugin_dir).map_err(|source| io_error(&plugin_dir, source))?;
    let actual_count = entries.try_fold(0_usize, |acc, entry| {
        entry.map_err(|source| io_error(&plugin_dir, source))?;
        acc.checked_add(1)
            .ok_or(FixtureError::CountOverflow { count: acc })
    })?;
    if actual_count != count {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    for index in 0..count {
        let path = plugin_dir.join(format!("{index:03}.lua"));
        let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
        hasher.update(format!("plugin/{index:03}.lua\0"));
        hasher.update(bytes);
    }
    Ok(Some(hex_digest(hasher.finalize().as_slice())))
}

fn remove_generated_path(path: &Path) -> Result<(), FixtureError> {
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

const HEX: &[u8; 16] = b"0123456789abcdef";
fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn io_error(path: &Path, source: io::Error) -> FixtureError {
    FixtureError::Io {
        path: path.to_path_buf(),
        source,
    }
}

//! Neovim persistent-undo container I/O.
//!
//! The numeric encoding is big-endian (`undo_write_bytes` in `undo.c`). A
//! parsed file retains its complete byte stream, including header branches and
//! extension records, so read/write preserves fields this crate does not edit.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::Buffer;

const MAGIC: &[u8; 9] = b"Vim\x9fUnDo\xe5";
const VERSION: u16 = 3;
const HEADER_END_MAGIC: u16 = 0xe7aa;
const HASH_SIZE: usize = 32;
const FIXED_PREFIX: usize = 9 + 2 + HASH_SIZE + 4;

/// Persistent-undo decoding or encoding error.
#[derive(Debug, Error)]
pub enum UndoFileError {
    /// Underlying stream failure.
    #[error("undo file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// File signature was absent.
    #[error("not a Neovim undo file")]
    Magic,
    /// File version is not supported.
    #[error("unsupported undo file version {0}")]
    Version(u16),
    /// A record was cut short or structurally invalid.
    #[error("truncated or malformed undo file")]
    Malformed,
    /// The file belongs to different buffer contents.
    #[error("undo file content hash or line count does not match")]
    ContentMismatch,
}

/// A validated persistent-undo file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoFile {
    content_hash: [u8; HASH_SIZE],
    line_count: u32,
    bytes: Vec<u8>,
}

impl UndoFile {
    /// Reads an upstream-format file and validates its top-level header.
    pub fn read(mut reader: impl Read) -> Result<Self, UndoFileError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        if bytes.get(..MAGIC.len()) != Some(MAGIC) {
            return Err(UndoFileError::Magic);
        }
        let version = read_u16(&bytes, 9)?;
        if version != VERSION {
            return Err(UndoFileError::Version(version));
        }
        let hash_slice = bytes
            .get(11..11 + HASH_SIZE)
            .ok_or(UndoFileError::Malformed)?;
        let mut content_hash = [0; HASH_SIZE];
        content_hash.copy_from_slice(hash_slice);
        let line_count = read_u32(&bytes, 11 + HASH_SIZE)?;
        validate_global_header(&bytes)?;
        Ok(Self {
            content_hash,
            line_count,
            bytes,
        })
    }

    /// Creates the smallest valid undo file for a buffer (an empty history).
    #[must_use]
    pub fn empty_for_buffer(buffer: &Buffer) -> Self {
        let content_hash = content_hash(buffer);
        let line_count = u32::try_from(buffer.line_count()).unwrap_or(u32::MAX);
        let mut bytes = Vec::with_capacity(96);
        bytes.extend_from_slice(MAGIC);
        put_u16(&mut bytes, VERSION);
        bytes.extend_from_slice(&content_hash);
        put_u32(&mut bytes, line_count);
        put_u32(&mut bytes, 0); // saved line length
        put_u32(&mut bytes, 0); // saved line lnum
        put_u32(&mut bytes, 0); // saved line column
        put_u32(&mut bytes, 0); // old head
        put_u32(&mut bytes, 0); // new head
        put_u32(&mut bytes, 0); // current head
        put_u32(&mut bytes, 0); // number of headers
        put_u32(&mut bytes, 0); // last sequence
        put_u32(&mut bytes, 0); // current sequence
        bytes.extend_from_slice(&0_i64.to_be_bytes());
        bytes.push(4); // optional payload size
        bytes.push(1); // UF_LAST_SAVE_NR
        put_u32(&mut bytes, 0);
        bytes.push(0); // optional-field terminator
        put_u16(&mut bytes, HEADER_END_MAGIC);
        Self {
            content_hash,
            line_count,
            bytes,
        }
    }

    /// Writes the complete validated byte stream.
    pub fn write(&self, mut writer: impl Write) -> Result<(), UndoFileError> {
        writer.write_all(&self.bytes)?;
        Ok(())
    }

    /// Returns the SHA-256 stored by Neovim.
    #[must_use]
    pub const fn hash(&self) -> &[u8; HASH_SIZE] {
        &self.content_hash
    }

    /// Returns the stored logical line count.
    #[must_use]
    pub const fn line_count(&self) -> u32 {
        self.line_count
    }

    /// Confirms that the undo stream belongs to `buffer`.
    pub fn verify_buffer(&self, buffer: &Buffer) -> Result<(), UndoFileError> {
        if self.content_hash != content_hash(buffer)
            || usize::try_from(self.line_count).ok() != Some(buffer.line_count())
        {
            return Err(UndoFileError::ContentMismatch);
        }
        Ok(())
    }

    /// Returns the exact encoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Computes Neovim's undo hash: each logical line followed by NUL.
#[must_use]
pub fn content_hash(buffer: &Buffer) -> [u8; HASH_SIZE] {
    let mut digest = Sha256::new();
    for lnum in 1..=buffer.line_count() {
        if let Ok(line) = buffer.line(lnum) {
            digest.update(line);
            digest.update([0]);
        }
    }
    digest.finalize().into()
}

fn validate_global_header(bytes: &[u8]) -> Result<(), UndoFileError> {
    let mut offset = FIXED_PREFIX;
    let saved_line_len = usize::try_from(read_u32(bytes, offset)?).map_err(|_| UndoFileError::Malformed)?;
    offset = offset.checked_add(4).ok_or(UndoFileError::Malformed)?;
    offset = offset
        .checked_add(saved_line_len)
        .ok_or(UndoFileError::Malformed)?;
    // saved line position, three head pointers, head count, two sequences, time
    offset = offset.checked_add(4 * 8 + 8).ok_or(UndoFileError::Malformed)?;
    loop {
        let len = *bytes.get(offset).ok_or(UndoFileError::Malformed)?;
        offset += 1;
        if len == 0 {
            break;
        }
        let payload = usize::from(len);
        offset = offset
            .checked_add(1 + payload)
            .ok_or(UndoFileError::Malformed)?;
        if offset > bytes.len() {
            return Err(UndoFileError::Malformed);
        }
    }
    if bytes.len().saturating_sub(offset) < 2 {
        return Err(UndoFileError::Malformed);
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, UndoFileError> {
    let raw: [u8; 2] = bytes
        .get(offset..offset + 2)
        .ok_or(UndoFileError::Malformed)?
        .try_into()
        .map_err(|_| UndoFileError::Malformed)?;
    Ok(u16::from_be_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, UndoFileError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or(UndoFileError::Malformed)?
        .try_into()
        .map_err(|_| UndoFileError::Malformed)?;
    Ok(u32::from_be_bytes(raw))
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

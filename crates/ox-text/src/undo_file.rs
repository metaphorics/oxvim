//! Neovim persistent-undo container I/O.
//!
//! The numeric encoding is big-endian (`undo_write_bytes` in `undo.c`). A
//! parsed file retains its complete byte stream, including header branches and
//! extension records, so read/write preserves fields this crate does not edit.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{Buffer, UndoTree};

const MAGIC: &[u8; 9] = b"Vim\x9fUnDo\xe5";
const VERSION: u16 = 3;
const HEADER_MAGIC: u16 = 0x5fd0;
const HEADER_END_MAGIC: u16 = 0xe7aa;
const ENTRY_MAGIC: u16 = 0xf518;
const ENTRY_END_MAGIC: u16 = 0x3581;
const SAVE_NR: u8 = 1; // optional save-count field kind
const NAMED_MARKS: usize = 26;
const HASH_SIZE: usize = 32;
const FIXED_PREFIX: usize = 9 + 2 + HASH_SIZE + 4;
// Both serialized extmark object variants (splice and move) carry a fixed
// 48-byte payload: six 32-bit fields then three 64-bit byte counts.
const EXTMARK_PAYLOAD: usize = 48;

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
    /// Reads an upstream-format file and fully validates its structure: the
    /// global header, the declared header record count and links, per-header
    /// entry framing and length-prefixed payloads, both end markers, and the
    /// end of input. Truncated, mis-framed, or trailing-garbage tails are
    /// rejected.
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
        validate_structure(&bytes)?;
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

    /// Serializes an undo tree in upstream persistent-undo format.
    ///
    /// Produces a valid file that Neovim can load with `:rundo`: the global
    /// header, one header per tree node (with branch links, cursor and
    /// timestamp), and the entry lines Neovim needs to undo each edit.
    #[must_use]
    pub fn from_tree(buffer: &Buffer, tree: &UndoTree) -> Self {
        let content_hash = content_hash(buffer);
        let line_count = u32::try_from(buffer.line_count()).unwrap_or(u32::MAX);
        let records = tree.header_records();
        let summary = tree.summary();
        let num_head = u32::try_from(records.len()).unwrap_or(u32::MAX);

        let mut bytes = Vec::with_capacity(FIXED_PREFIX + 4 * 8 + 8 + 12 + records.len() * 96);
        put_global_header(&mut bytes, &content_hash, line_count, &summary, num_head);
        for record in &records {
            put_header(&mut bytes, record);
        }
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

/// Walks the whole file: the global header, `num_head` undo headers (each
/// with its entry list and empty extmark section), the header-end marker, and
/// then requires end of input. Every length prefix is bounds-checked against
/// the remaining bytes; trailing bytes after the header-end marker reject the
/// file. Framing mirrors `u_read_undo` in `undo.c`.
fn validate_structure(bytes: &[u8]) -> Result<(), UndoFileError> {
    // Saved line for the "U" command: a length prefix then its payload.
    let saved_len = read_u32(bytes, FIXED_PREFIX)?;
    let mut offset = add(
        FIXED_PREFIX + 4,
        usize::try_from(saved_len).map_err(|_| UndoFileError::Malformed)?,
    )?;
    // Saved line lnum/col, old/new/current head, then the declared head count.
    let num_head = read_u32(bytes, add(offset, 20)?)?;
    // Consume the remaining fixed fields (5 heads + seq_last/seq_cur + time).
    offset = add(offset, 8 * 4 + 8)?;
    offset = skip_optional(bytes, offset)?;
    if num_head == 0 {
        expect_u16(bytes, offset, HEADER_END_MAGIC)?;
        offset = add(offset, 2)?;
        return end_of_input(bytes, offset);
    }
    for _ in 0..num_head {
        expect_u16(bytes, offset, HEADER_MAGIC)?;
        offset = add(offset, 2)?;
        offset = validate_header(bytes, offset)?;
    }
    expect_u16(bytes, offset, HEADER_END_MAGIC)?;
    offset = add(offset, 2)?;
    end_of_input(bytes, offset)
}

/// Validates one undo header after its `HEADER_MAGIC`, returning the offset
/// just past its entries and extmark terminator.
fn validate_header(bytes: &[u8], mut offset: usize) -> Result<usize, UndoFileError> {
    // Four branch links and the sequence number.
    offset = add(offset, 5 * 4)?;
    // Saved cursor (lnum, col, coladd), vcol, flags.
    offset = add(offset, 3 * 4 + 4 + 2)?;
    // Named marks (each a position triple) and the visual info block.
    offset = add(offset, NAMED_MARKS * 12)?;
    offset = add(offset, 2 * 12 + 4 + 4)?;
    // Header timestamp.
    offset = add(offset, 8)?;
    offset = skip_optional(bytes, offset)?;
    // Undo entries: ENTRY_MAGIC, lengths, terminated by ENTRY_END_MAGIC.
    while peek_is(bytes, offset, ENTRY_MAGIC) {
        offset = add(offset, 2)?;
        offset = validate_entry(bytes, offset)?;
    }
    expect_u16(bytes, offset, ENTRY_END_MAGIC)?;
    offset = add(offset, 2)?;
    // Extmark undo section: optional extmark objects then ENTRY_END_MAGIC.
    while peek_is(bytes, offset, ENTRY_MAGIC) {
        offset = add(offset, 2)?;
        let kind = read_u32(bytes, offset)?;
        offset = add(offset, 4)?;
        if kind > 1 {
            // Only splice and move objects are ever serialized.
            return Err(UndoFileError::Malformed);
        }
        offset = add(offset, EXTMARK_PAYLOAD)?;
        if offset > bytes.len() {
            return Err(UndoFileError::Malformed);
        }
    }
    expect_u16(bytes, offset, ENTRY_END_MAGIC)?;
    add(offset, 2)
}

/// Validates one length-prefixed series of saved lines after its `ENTRY_MAGIC`.
fn validate_entry(bytes: &[u8], mut offset: usize) -> Result<usize, UndoFileError> {
    // ue_top, ue_bot, ue_lcount, then ue_size.
    offset = add(offset, 3 * 4)?;
    let size = read_u32(bytes, offset)?;
    offset = add(offset, 4)?;
    for _ in 0..size {
        let line_len = read_u32(bytes, offset)?;
        offset = add(offset, 4)?;
        offset = add(offset, usize::try_from(line_len).map_err(|_| UndoFileError::Malformed)?)?;
        if offset > bytes.len() {
            return Err(UndoFileError::Malformed);
        }
    }
    Ok(offset)
}

/// Skips a sequence of optional fields: `len == 0` terminates, otherwise each
/// field is a `len` byte, a kind byte, and `len` payload bytes (all bounded).
fn skip_optional(bytes: &[u8], mut offset: usize) -> Result<usize, UndoFileError> {
    loop {
        let len = *bytes.get(offset).ok_or(UndoFileError::Malformed)?;
        offset = add(offset, 1)?;
        if len == 0 {
            return Ok(offset);
        }
        offset = add(offset, usize::from(len) + 1)?;
        if offset > bytes.len() {
            return Err(UndoFileError::Malformed);
        }
    }
}

fn peek_is(bytes: &[u8], offset: usize, expected: u16) -> bool {
    matches!(read_u16(bytes, offset), Ok(value) if value == expected)
}

fn expect_u16(bytes: &[u8], offset: usize, expected: u16) -> Result<(), UndoFileError> {
    match read_u16(bytes, offset)? {
        value if value == expected => Ok(()),
        _ => Err(UndoFileError::Malformed),
    }
}

fn add(offset: usize, n: usize) -> Result<usize, UndoFileError> {
    offset.checked_add(n).ok_or(UndoFileError::Malformed)
}

fn end_of_input(bytes: &[u8], offset: usize) -> Result<(), UndoFileError> {
    if offset == bytes.len() {
        Ok(())
    } else {
        Err(UndoFileError::Malformed)
    }
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

fn put_u64(bytes: &mut Vec<u8>, value: i64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

/// Writes the shared top-level header from `empty_for_buffer`/`from_tree`.
fn put_global_header(
    bytes: &mut Vec<u8>,
    content_hash: &[u8; HASH_SIZE],
    line_count: u32,
    summary: &crate::UndoSummary,
    num_head: u32,
) {
    bytes.extend_from_slice(MAGIC);
    put_u16(bytes, VERSION);
    bytes.extend_from_slice(content_hash);
    put_u32(bytes, line_count);
    // Saved line for the "U" command: none (zero length, so no bytes follow).
    put_u32(bytes, 0); // saved line length
    put_u32(bytes, 0); // saved line lnum
    put_u32(bytes, 0); // saved line column
    put_u32(bytes, u32::try_from(summary.oldhead).unwrap_or(u32::MAX)); // old head
    put_u32(bytes, u32::try_from(summary.newhead).unwrap_or(u32::MAX)); // new head
    put_u32(bytes, u32::try_from(summary.curhead).unwrap_or(u32::MAX)); // current head
    put_u32(bytes, num_head); // number of headers
    put_u32(bytes, u32::try_from(summary.seq_last).unwrap_or(u32::MAX)); // last sequence
    put_u32(bytes, u32::try_from(summary.seq_cur).unwrap_or(u32::MAX)); // current sequence
    put_u64(bytes, summary.time_cur); // current time
    bytes.push(4); // optional payload size
    bytes.push(SAVE_NR); // UF_LAST_SAVE_NR
    put_u32(bytes, 0); // save count last
    bytes.push(0); // optional-field terminator
}

/// Writes one undo header and its entry lines.
fn put_header(bytes: &mut Vec<u8>, record: &crate::HeaderRecord) {
    put_u16(bytes, HEADER_MAGIC);
    for link in [record.next, record.prev, record.alt_next, record.alt_prev] {
        put_u32(bytes, u32::try_from(link).unwrap_or(u32::MAX));
    }
    put_u32(bytes, u32::try_from(record.seq).unwrap_or(u32::MAX));
    // Saved cursor (before the edit: the state undoing restores), then vcol.
    put_pos(bytes, record.edit.cursor_before.lnum, record.edit.cursor_before.col);
    put_u32(bytes, 0); // cursor vcol
    put_u16(bytes, 0); // flags
    for _ in 0..NAMED_MARKS {
        put_pos(bytes, 0, 0);
    }
    // Visualinfo: two positions, mode, curswant — all zero.
    put_pos(bytes, 0, 0);
    put_pos(bytes, 0, 0);
    put_u32(bytes, 0);
    put_u32(bytes, 0);
    put_u64(bytes, record.timestamp);
    bytes.push(4); // optional payload size
    bytes.push(SAVE_NR); // UHP_SAVE_NR
    put_u32(bytes, 0); // save count
    bytes.push(0); // optional-field terminator

    // One entry undoing the "after" range back to the "before" lines.
    // `u_undoredo` deletes lines (top+1 .. bot-1) and inserts `ue_size`
    // lines after `top`; anchoring at `start-1` with `bot = start+after.len`
    // therefore deletes the `after` block and reinserts `before` at `start`.
    put_u16(bytes, ENTRY_MAGIC);
    let start = u32::try_from(record.edit.start).unwrap_or(u32::MAX);
    let after = u32::try_from(record.edit.after.len()).unwrap_or(u32::MAX);
    let before = u32::try_from(record.edit.before.len()).unwrap_or(u32::MAX);
    put_u32(bytes, start.saturating_sub(1)); // ue_top
    put_u32(bytes, start.saturating_add(after)); // ue_bot
    put_u32(bytes, 0); // ue_lcount
    put_u32(bytes, before); // ue_size
    for line in &record.edit.before {
        let len = u32::try_from(line.len()).unwrap_or(u32::MAX);
        put_u32(bytes, len);
        bytes.extend_from_slice(line);
    }
    put_u16(bytes, ENTRY_END_MAGIC);
    put_u16(bytes, ENTRY_END_MAGIC); // extmark section is empty
}

fn put_pos(bytes: &mut Vec<u8>, lnum: usize, col: usize) {
    put_u32(bytes, u32::try_from(lnum).unwrap_or(u32::MAX));
    put_u32(bytes, u32::try_from(col).unwrap_or(u32::MAX));
    put_u32(bytes, 0); // coladd
}

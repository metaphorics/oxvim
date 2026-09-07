//! Neovim swapfile block-format snapshots.
//!
//! Upstream pages dirty memfile blocks incrementally. Oxvim deliberately emits
//! a complete block-0/root-pointer/data-block snapshot; recovery observes the
//! same tree and line bytes.
//!
//! Block zero follows `memline.c:188-202` (`ZeroBlock`) byte for byte: id,
//! version string, page size, original-file mtime/inode, creator pid, user and
//! host names, the edited file's name, and the magic words that identify the
//! writer's byte order (`memline.c:170-177`).

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::Path;

use thiserror::Error;

use crate::{Buffer, BufferError};

const PAGE_SIZE: usize = 4096;
const ZERO_BLOCK_SIZE: usize = 1024;
const PTR_ID: u16 = (b'p' as u16) << 8 | b't' as u16;
const DATA_ID: u16 = (b'd' as u16) << 8 | b'a' as u16;
const PTR_HEADER: usize = 8;
const PTR_ENTRY: usize = 24;
const DATA_HEADER: usize = 24;
/// `b0_uname`/`b0_hname` start after `b0_pid` (`memline.c:189-197`).
const B0_UNAME: usize = 28;
const B0_HNAME: usize = B0_UNAME + B0_UNAME_SIZE;
/// `b0_fname` starts at 108: the sum of `b0_id[2]`, `b0_version[10]`,
/// `b0_page_size[4]`, `b0_mtime[4]`, `b0_ino[4]`, `b0_pid[4]`,
/// `b0_uname[40]`, and `b0_hname[40]` (`memline.c:189-197`).
const B0_FNAME: usize = 108;
/// `B0_FNAME_SIZE_ORG` (`memline.c:163`).
const B0_FNAME_LEN: usize = 900;
/// `B0_FNAME_SIZE_CRYPT` (`memline.c:165`): names are written at most this
/// long, which is also the span `ml_check_b0_strings` requires a NUL within
/// (`memline.c:632`).
const B0_FNAME_SIZE_CRYPT: usize = 890;
/// `B0_FNAME_SIZE_NOCRYPT` (`memline.c:164`): where an optional
/// 'fileencoding' is appended (`memline.c:721-735`).
const B0_FNAME_SIZE_NOCRYPT: usize = 898;
/// `B0_UNAME_SIZE`/`B0_HNAME_SIZE` (`memline.c:166-167`) and their offsets
const B0_UNAME_SIZE: usize = 40;
const B0_HNAME_SIZE: usize = 40;
/// `b0_flags`/`b0_dirty` live at the end of `b0_fname` (`memline.c:209,212`).
const B0_FLAGS: usize = B0_FNAME + B0_FNAME_LEN - 2;
const B0_DIRTY: usize = B0_FNAME + B0_FNAME_LEN - 1;
/// `memline.c:170-176`: byte-order check values.
const B0_MAGIC_LONG: i64 = 0x3031_3233;
const B0_MAGIC_INT: u32 = 0x2021_2223;
const B0_MAGIC_SHORT: u16 = 0x1213;
const B0_MAGIC_CHAR: u8 = 0x55;
/// `B0_SAME_DIR`/`B0_HAS_FENC`/`B0_FF_MASK` (`memline.c:217-224`).
const B0_SAME_DIR: u8 = 1 << 2;
const B0_HAS_FENC: u8 = 1 << 3;
const B0_FF_MASK: u8 = 3;

/// Block-zero identity fields (`memline.c:337-346`): what a real recovery
/// reads to say which file a swapfile belongs to, who wrote it, and whether
/// it holds changes (`swapfile_info`, `memline.c:1553-1590`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapMeta {
    /// `b0_mtime`: the original file's modification time in seconds, 0 when
    /// unknown (`memline.c:687,693`).
    pub mtime: u32,
    /// `b0_ino`: the original file's inode, 0 when unknown
    /// (`memline.c:688,694`).
    pub inode: u32,
    /// `b0_pid`: process id of the writer (`memline.c:345`).
    pub pid: u32,
    /// `b0_uname`: creator's login name (`memline.c:341-342`).
    pub user: String,
    /// `b0_hname`: creator's host name (`memline.c:343-344`).
    pub host: String,
    /// `b0_dirty`: `B0_DIRTY` when the buffer has changes
    /// (`memline.c:338,208`).
    pub dirty: bool,
    /// `B0_SAME_DIR`: swapfile and edited file share a directory
    /// (`memline.c:712-719`).
    pub same_dir: bool,
    /// 'fileencoding' appended after `b0_fname` when it fits
    /// (`memline.c:721-735`); empty omits it.
    pub file_encoding: String,
    /// `get_fileformat(buf) + 1` in the low two bits of `b0_flags`
    /// (`memline.c:339,214-217`): 1 unix, 2 dos, 3 mac.
    pub fileformat: u8,
}

/// Swap snapshot error.
#[derive(Debug, Error)]
pub enum SwapError {
    /// Underlying stream failure.
    #[error("swapfile I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Text in a data block was not representable by the rope buffer.
    #[error(transparent)]
    Buffer(#[from] BufferError),
    /// Block zero or the block tree was malformed.
    #[error("malformed or unsupported swapfile: {0}")]
    Malformed(&'static str),
    /// Snapshot is too large for one data extent.
    #[error("snapshot exceeds swapfile numeric limits")]
    TooLarge,
}

/// A complete swapfile snapshot.
#[derive(Clone, Debug)]
pub struct SwapFile {
    /// Original edited file name from block zero (`b0_fname`, the full path
    /// `b_ffname` upstream, `memline.c:660-706`).
    pub file_name: String,
    /// Block-zero identity fields.
    pub meta: SwapMeta,
    /// Recovered text.
    pub buffer: Buffer,
}

impl SwapFile {
    /// Creates a snapshot for `buffer` with default block-zero metadata.
    #[must_use]
    pub fn new(file_name: impl Into<String>, buffer: Buffer) -> Self {
        Self {
            file_name: file_name.into(),
            meta: SwapMeta::default(),
            buffer,
        }
    }

    /// Sets the block-zero identity fields.
    #[must_use]
    pub fn with_meta(mut self, meta: SwapMeta) -> Self {
        self.meta = meta;
        self
    }

    /// Serializes block zero, one root pointer block, and one data extent.
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::TooLarge`] when the text, its index, or the
    /// file name exceeds the block-format limits, the buffer's own line
    /// error if a line is malformed, and the writer's I/O error if a block
    /// cannot be written.
    pub fn write(&self, mut writer: impl Write) -> Result<(), SwapError> {
        let lines: Vec<Vec<u8>> = (1..=self.buffer.line_count())
            .map(|lnum| self.buffer.line(lnum))
            .collect::<Result<_, _>>()?;
        let text_bytes = lines
            .iter()
            .try_fold(0_usize, |sum, line| sum.checked_add(line.len() + 1))
            .ok_or(SwapError::TooLarge)?;
        let needed = DATA_HEADER
            .checked_add(lines.len().checked_mul(4).ok_or(SwapError::TooLarge)?)
            .and_then(|size| size.checked_add(text_bytes))
            .ok_or(SwapError::TooLarge)?;
        let pages = needed.div_ceil(PAGE_SIZE).max(1);
        let data_len = pages.checked_mul(PAGE_SIZE).ok_or(SwapError::TooLarge)?;
        let pages_u32 = u32::try_from(pages).map_err(|_| SwapError::TooLarge)?;
        let line_count_i64 = i64::try_from(lines.len()).map_err(|_| SwapError::TooLarge)?;
        let line_count_i32 = i32::try_from(lines.len()).map_err(|_| SwapError::TooLarge)?;

        let block0 = self.block_zero()?;
        writer.write_all(&block0)?;
        writer.write_all(&vec![0; PAGE_SIZE - ZERO_BLOCK_SIZE])?;

        // Pointer block (`memline.c:120-128`, filled at 358-371): one entry
        // pointing at the data block, `pb_pointer[]` from offset 8.
        let mut pointer = vec![0; PAGE_SIZE];
        pointer[0..2].copy_from_slice(&PTR_ID.to_le_bytes());
        pointer[2..4].copy_from_slice(&1_u16.to_le_bytes());
        let max =
            u16::try_from((PAGE_SIZE - PTR_HEADER) / PTR_ENTRY).map_err(|_| SwapError::TooLarge)?;
        pointer[4..6].copy_from_slice(&max.to_le_bytes());
        pointer[8..16].copy_from_slice(&2_i64.to_le_bytes());
        pointer[16..20].copy_from_slice(&line_count_i32.to_le_bytes());
        pointer[20..24].copy_from_slice(&1_i32.to_le_bytes());
        pointer[24..28].copy_from_slice(&pages_u32.to_le_bytes());
        writer.write_all(&pointer)?;

        // Data block (`memline.c:137-160`): header, one index word per line,
        // text packed downward from the end with a NUL after every line
        // (`ml_new_data` at 374-384 stores the same shape).
        let mut data = vec![0; data_len];
        data[0..2].copy_from_slice(&DATA_ID.to_le_bytes());
        let mut text_start = data_len;
        for (index, line) in lines.iter().enumerate() {
            text_start = text_start
                .checked_sub(line.len() + 1)
                .ok_or(SwapError::TooLarge)?;
            data[text_start..text_start + line.len()].copy_from_slice(line);
            let index_u32 = u32::try_from(text_start).map_err(|_| SwapError::TooLarge)?;
            let index_offset = DATA_HEADER + index * 4;
            data[index_offset..index_offset + 4].copy_from_slice(&index_u32.to_le_bytes());
        }
        let index_end = DATA_HEADER + lines.len() * 4;
        let free = text_start
            .checked_sub(index_end)
            .ok_or(SwapError::TooLarge)?;
        data[4..8].copy_from_slice(
            &u32::try_from(free)
                .map_err(|_| SwapError::TooLarge)?
                .to_le_bytes(),
        );
        data[8..12].copy_from_slice(
            &u32::try_from(text_start)
                .map_err(|_| SwapError::TooLarge)?
                .to_le_bytes(),
        );
        data[12..16].copy_from_slice(
            &u32::try_from(data_len)
                .map_err(|_| SwapError::TooLarge)?
                .to_le_bytes(),
        );
        data[16..24].copy_from_slice(&line_count_i64.to_le_bytes());
        writer.write_all(&data)?;
        Ok(())
    }

    /// Writes the snapshot to `path`: parents are created when missing
    /// (`os_mkdir_recurse`, `memline.c:3655-3664`), the file is truncated to
    /// exactly the snapshot length, and the bytes reach the disk before this
    /// returns (`mf_sync` `MFS_FLUSH` — the `do_fsync` half of `ml_preserve`,
    /// `memline.c:1763`).
    ///
    /// New files are reserved atomically (`O_EXCL`, `memfile.c:157-160`)
    /// without following symlinks (`O_NOFOLLOW`, `:765-776`) and with
    /// owner-only permissions (`fileio.c:435-444`): a pre-existing link
    /// or file the writer did not create fails instead of redirecting
    /// the snapshot. Rewrites of a file this writer already reserved
    /// re-open it, still refusing to follow links.
    ///
    /// # Errors
    ///
    /// Returns the parent-directory creation, reservation, serialization,
    /// or sync failure.
    pub fn write_to(&self, path: &Path) -> Result<(), SwapError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = Self::reserve_swapfile(path)?;
        self.write(&mut file)?;
        file.sync_all()?;
        Ok(())
    }

/// Atomically reserves a new swapfile or re-opens one already reserved:
/// the create half refuses symlinks and pre-existing files, the re-open
/// half still refuses symlinks, and both enforce owner-only permissions.
fn reserve_swapfile(path: &Path) -> Result<std::fs::File, SwapError> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    #[cfg(unix)]
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(Self::libc_nofollow())
        .open(path);
    #[cfg(not(unix))]
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    match created {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A file this writer reserved on an earlier preserve: re-open
            // for truncation without following links, and re-assert the
            // owner-only mode in case it predates this reservation.
            #[cfg(unix)]
            let file = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(Self::libc_nofollow())
                .open(path)?;
            #[cfg(not(unix))]
            let file = std::fs::OpenOptions::new().write(true).open(path)?;
            #[cfg(unix)]
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.set_len(0)?;
            Ok(file)
        }
        Err(error) => Err(SwapError::Io(error)),
    }
}

/// `O_NOFOLLOW` without taking a `libc` dependency: the flag value is a
/// stable kernel ABI constant on every Unix target this port supports.
#[cfg(unix)]
fn libc_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    return 0o400_000;
    #[cfg(target_os = "macos")]
    return 0x100;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return 0;
}

    /// Reads a native 64-bit little-endian Neovim swap block tree.
    ///
    /// # Errors
    ///
    /// Returns the reader's I/O error and [`SwapError::Malformed`] for a
    /// bad block-zero id, a byte-order mismatch (`b0_magic_wrong`,
    /// `memline.c:3670-3676`), an out-of-range page size, truncated blocks,
    /// or any other structural violation.
    pub fn read(mut reader: impl Read) -> Result<Self, SwapError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        if bytes.get(0..2) != Some(b"b0") {
            return Err(SwapError::Malformed("block-zero id"));
        }
        // `b0_magic_wrong` (memline.c:3670-3676): the four magic words
        // identify the writer's byte order; a mismatch means the file is
        // foreign or corrupt.
        if le_i64(&bytes, 1008).ok() != Some(B0_MAGIC_LONG)
            || le_u32(&bytes, 1016).ok() != Some(B0_MAGIC_INT)
            || le_u16(&bytes, 1020).ok() != Some(B0_MAGIC_SHORT)
            || bytes.get(1022) != Some(&B0_MAGIC_CHAR)
        {
            return Err(SwapError::Malformed("byte order magic"));
        }
        let page_size =
            usize::try_from(le_u32(&bytes, 12)?).map_err(|_| SwapError::Malformed("page size"))?;
        if !(ZERO_BLOCK_SIZE..=1 << 20).contains(&page_size) {
            return Err(SwapError::Malformed("page size"));
        }
        let fname_bytes = bytes
            .get(B0_FNAME..B0_FNAME + B0_FNAME_SIZE_CRYPT - 1)
            .ok_or(SwapError::Malformed("block zero"))?;
        let fname_len = fname_bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(fname_bytes.len());
        let file_name = String::from_utf8_lossy(&fname_bytes[..fname_len]).into_owned();
        let mut lines = Vec::new();
        let mut visited = BTreeSet::new();
        read_block(&bytes, page_size, 1, 1, &mut visited, &mut lines)?;
        let buffer = Buffer::from_lines(&lines, true)?;
        Ok(Self {
            file_name,
            meta: SwapMeta::default(),
            buffer,
        })
    }

    /// Builds block zero exactly as `ml_open` fills it (`memline.c:326-346`).
    fn block_zero(&self) -> Result<Vec<u8>, SwapError> {
        let mut block = vec![0; ZERO_BLOCK_SIZE];
        block[0..2].copy_from_slice(b"b0");
        // `xstrlcpy(xstpcpy(b0p->b0_version, "VIM "), Versions[0], 6)`
        // (`memline.c:334`): Neovim stamps the oldest readable Vim version.
        block[2..10].copy_from_slice(b"VIM 8.1\0");
        block[12..16].copy_from_slice(
            &u32::try_from(PAGE_SIZE)
                .map_err(|_| SwapError::TooLarge)?
                .to_le_bytes(),
        );
        block[16..20].copy_from_slice(&self.meta.mtime.to_le_bytes());
        block[20..24].copy_from_slice(&self.meta.inode.to_le_bytes());
        block[24..28].copy_from_slice(&self.meta.pid.to_le_bytes());
        block[B0_UNAME..B0_UNAME + B0_UNAME_SIZE]
            .copy_from_slice(&capped_nul(self.meta.user.as_bytes(), B0_UNAME_SIZE));
        block[B0_HNAME..B0_HNAME + B0_HNAME_SIZE]
            .copy_from_slice(&capped_nul(self.meta.host.as_bytes(), B0_HNAME_SIZE));
        let name = self.file_name.as_bytes();
        let copy_len = name.len().min(B0_FNAME_SIZE_CRYPT - 1);
        block[B0_FNAME..B0_FNAME + copy_len].copy_from_slice(&name[..copy_len]);

        let mut flags = self.meta.fileformat & B0_FF_MASK;
        let encoding = self.meta.file_encoding.as_bytes();
        let fits = copy_len + encoding.len() < B0_FNAME_SIZE_NOCRYPT;
        if !encoding.is_empty() && fits {
            // `add_b0_fenc` (`memline.c:721-735`): a NUL in front of the
            // encoding, both at the end of `b0_fname`.
            let start = B0_FNAME + B0_FNAME_SIZE_NOCRYPT - encoding.len();
            block[start - 1] = 0;
            block[start..start + encoding.len()].copy_from_slice(encoding);
            flags |= B0_HAS_FENC;
        }
        if self.meta.same_dir {
            flags |= B0_SAME_DIR;
        }
        block[B0_FLAGS] = flags;
        block[B0_DIRTY] = if self.meta.dirty { 0x55 } else { 0 };

        block[1008..1016].copy_from_slice(&B0_MAGIC_LONG.to_le_bytes());
        block[1016..1020].copy_from_slice(&B0_MAGIC_INT.to_le_bytes());
        block[1020..1022].copy_from_slice(&B0_MAGIC_SHORT.to_le_bytes());
        block[1022] = B0_MAGIC_CHAR;
        Ok(block)
    }
}

/// A byte string truncated to `cap - 1` and NUL-terminated, the shape every
/// `ZeroBlock` string takes (`memline.c:341-344`).
fn capped_nul(bytes: &[u8], cap: usize) -> Vec<u8> {
    let mut output = vec![0; cap];
    let copy_len = bytes.len().min(cap - 1);
    output[..copy_len].copy_from_slice(&bytes[..copy_len]);
    output
}

fn read_block(
    bytes: &[u8],
    page_size: usize,
    block_number: usize,
    page_count: usize,
    visited: &mut BTreeSet<usize>,
    lines: &mut Vec<Vec<u8>>,
) -> Result<(), SwapError> {
    if !visited.insert(block_number) {
        return Err(SwapError::Malformed("block cycle"));
    }
    let offset = block_number
        .checked_mul(page_size)
        .ok_or(SwapError::Malformed("block offset"))?;
    let extent = page_count
        .checked_mul(page_size)
        .ok_or(SwapError::Malformed("block extent"))?;
    let block = bytes
        .get(offset..offset + extent)
        .ok_or(SwapError::Malformed("truncated block"))?;
    let id = le_u16(block, 0)?;
    if id == DATA_ID {
        read_data(block, lines)
    } else if id == PTR_ID {
        let count = usize::from(le_u16(block, 2)?);
        let max = usize::from(le_u16(block, 4)?);
        if count > max || PTR_HEADER + count * PTR_ENTRY > block.len() {
            return Err(SwapError::Malformed("pointer count"));
        }
        for index in 0..count {
            let base = PTR_HEADER + index * PTR_ENTRY;
            let child = le_i64(block, base)?;
            if child <= 0 {
                return Err(SwapError::Malformed("negative original-file block"));
            }
            let child_pages = usize::try_from(le_u32(block, base + 16)?)
                .map_err(|_| SwapError::Malformed("child page count"))?;
            read_block(
                bytes,
                page_size,
                usize::try_from(child).map_err(|_| SwapError::Malformed("block number"))?,
                child_pages,
                visited,
                lines,
            )?;
        }
        Ok(())
    } else {
        Err(SwapError::Malformed("block id"))
    }
}

fn read_data(block: &[u8], lines: &mut Vec<Vec<u8>>) -> Result<(), SwapError> {
    let count =
        usize::try_from(le_i64(block, 16)?).map_err(|_| SwapError::Malformed("data line count"))?;
    let index_end = count
        .checked_mul(4)
        .and_then(|index| DATA_HEADER.checked_add(index))
        .ok_or(SwapError::Malformed("data index"))?;
    if index_end > block.len() {
        return Err(SwapError::Malformed("data index"));
    }
    for index in 0..count {
        let start = usize::try_from(le_u32(block, DATA_HEADER + index * 4)?)
            .map_err(|_| SwapError::Malformed("line index"))?;
        let tail = block
            .get(start..)
            .ok_or(SwapError::Malformed("line index"))?;
        let length = tail
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(SwapError::Malformed("unterminated line"))?;
        lines.push(tail[..length].to_vec());
    }
    Ok(())
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, SwapError> {
    Ok(u16::from_le_bytes(array(bytes, offset)?))
}
fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, SwapError> {
    Ok(u32::from_le_bytes(array(bytes, offset)?))
}
fn le_i64(bytes: &[u8], offset: usize) -> Result<i64, SwapError> {
    Ok(i64::from_le_bytes(array(bytes, offset)?))
}
fn array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], SwapError> {
    bytes
        .get(offset..offset + N)
        .and_then(|word| word.try_into().ok())
        .ok_or(SwapError::Malformed("truncated block"))
}

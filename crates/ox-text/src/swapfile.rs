//! Neovim swapfile block-format snapshots.
//!
//! Upstream pages dirty memfile blocks incrementally. Oxvim deliberately emits
//! a complete block-0/root-pointer/data-block snapshot; recovery observes the
//! same tree and line bytes.

use std::collections::BTreeSet;
use std::io::{Read, Write};

use thiserror::Error;

use crate::{Buffer, BufferError};

const PAGE_SIZE: usize = 4096;
const ZERO_BLOCK_SIZE: usize = 1024;
const PTR_ID: u16 = (b'p' as u16) << 8 | b't' as u16;
const DATA_ID: u16 = (b'd' as u16) << 8 | b'a' as u16;
const PTR_HEADER: usize = 8;
const PTR_ENTRY: usize = 24;
const DATA_HEADER: usize = 24;
const B0_FNAME: usize = 108;
const B0_FNAME_LEN: usize = 900;

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
    /// Original edited file name from block zero.
    pub file_name: String,
    /// Recovered text.
    pub buffer: Buffer,
}

impl SwapFile {
    /// Creates a snapshot for `buffer`.
    #[must_use]
    pub fn new(file_name: impl Into<String>, buffer: Buffer) -> Self {
        Self {
            file_name: file_name.into(),
            buffer,
        }
    }

    /// Serializes block zero, one root pointer block, and one data extent.
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

        let mut pointer = vec![0; PAGE_SIZE];
        pointer[0..2].copy_from_slice(&PTR_ID.to_le_bytes());
        pointer[2..4].copy_from_slice(&1_u16.to_le_bytes());
        let max = u16::try_from((PAGE_SIZE - PTR_HEADER) / PTR_ENTRY)
            .map_err(|_| SwapError::TooLarge)?;
        pointer[4..6].copy_from_slice(&max.to_le_bytes());
        pointer[8..16].copy_from_slice(&2_i64.to_le_bytes());
        pointer[16..20].copy_from_slice(&line_count_i32.to_le_bytes());
        pointer[20..24].copy_from_slice(&1_i32.to_le_bytes());
        pointer[24..28].copy_from_slice(&pages_u32.to_le_bytes());
        writer.write_all(&pointer)?;

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
        let free = text_start.checked_sub(index_end).ok_or(SwapError::TooLarge)?;
        data[4..8].copy_from_slice(
            &u32::try_from(free).map_err(|_| SwapError::TooLarge)?.to_le_bytes(),
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

    /// Reads a native 64-bit little-endian Neovim swap block tree.
    pub fn read(mut reader: impl Read) -> Result<Self, SwapError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        if bytes.get(0..2) != Some(b"b0") {
            return Err(SwapError::Malformed("block-zero id"));
        }
        let page_size = usize::try_from(le_u32(&bytes, 12)?)
            .map_err(|_| SwapError::Malformed("page size"))?;
        if page_size < ZERO_BLOCK_SIZE || page_size > 1 << 20 {
            return Err(SwapError::Malformed("page size"));
        }
        let fname_bytes = bytes
            .get(B0_FNAME..B0_FNAME + B0_FNAME_LEN - 2)
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
        Ok(Self { file_name, buffer })
    }

    fn block_zero(&self) -> Result<Vec<u8>, SwapError> {
        let mut block = vec![0; ZERO_BLOCK_SIZE];
        block[0..2].copy_from_slice(b"b0");
        block[2..10].copy_from_slice(b"VIM 9.1\0");
        block[12..16].copy_from_slice(
            &u32::try_from(PAGE_SIZE)
                .map_err(|_| SwapError::TooLarge)?
                .to_le_bytes(),
        );
        let name = self.file_name.as_bytes();
        let copy_len = name.len().min(B0_FNAME_LEN - 3);
        block[B0_FNAME..B0_FNAME + copy_len].copy_from_slice(&name[..copy_len]);
        block[B0_FNAME + B0_FNAME_LEN - 2] = 1; // Unix fileformat + 1
        block[B0_FNAME + B0_FNAME_LEN - 1] = 0x55; // B0_DIRTY
        block[1008..1016].copy_from_slice(&0x3031_3233_i64.to_le_bytes());
        block[1016..1020].copy_from_slice(&0x2021_2223_i32.to_le_bytes());
        block[1020..1022].copy_from_slice(&0x1213_i16.to_le_bytes());
        block[1022] = 0x55;
        Ok(block)
    }
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
    let count = usize::try_from(le_i64(block, 16)?)
        .map_err(|_| SwapError::Malformed("data line count"))?;
    if DATA_HEADER + count * 4 > block.len() {
        return Err(SwapError::Malformed("data index"));
    }
    for index in 0..count {
        let start = usize::try_from(le_u32(block, DATA_HEADER + index * 4)?)
            .map_err(|_| SwapError::Malformed("line index"))?;
        let tail = block.get(start..).ok_or(SwapError::Malformed("line index"))?;
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
        .ok_or(SwapError::Malformed("truncated integer"))?
        .try_into()
        .map_err(|_| SwapError::Malformed("integer width"))
}

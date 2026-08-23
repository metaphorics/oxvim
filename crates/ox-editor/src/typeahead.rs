//! Encoded key sequences and the editor typeahead stack.
//!
//! The three-byte representation follows `src/nvim/keycodes.h:15-20,32-45,70-89`.
//! Offset insertion and per-byte remap metadata follow `src/nvim/input.c:922-1027`.

use ox_types::BufHandle;
use thiserror::Error;

use crate::mapping::MapModes;

/// Marker introducing an internal three-byte key code.
pub const K_SPECIAL: u8 = 0x80;
/// Second byte used to quote a literal zero byte.
pub const KS_ZERO: u8 = 0xff;
/// Second byte used to quote a literal [`K_SPECIAL`].
pub const KS_SPECIAL: u8 = 0xfe;
/// Second byte used by named special keys without termcap names.
pub const KS_EXTRA: u8 = 0xfd;
/// Third byte identifying the event-loop wakeup key used by low-level input.
pub const KE_EVENT: u8 = 102;
/// Third-byte filler used with quoted literal bytes.
pub const KE_FILLER: u8 = b'X';

/// Compact internal key-string representation.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Keys(Vec<u8>);

impl Keys {
    /// Encodes raw bytes, quoting zero and `K_SPECIAL` as upstream requires.
    #[must_use]
    pub fn encode(bytes: &[u8]) -> Self {
        let extra = bytes
            .iter()
            .filter(|byte| **byte == 0 || **byte == K_SPECIAL)
            .count()
            .saturating_mul(2);
        let mut encoded = Vec::with_capacity(bytes.len().saturating_add(extra));
        for byte in bytes {
            match *byte {
                0 => encoded.extend_from_slice(&[K_SPECIAL, KS_ZERO, KE_FILLER]),
                K_SPECIAL => encoded.extend_from_slice(&[K_SPECIAL, KS_SPECIAL, KE_FILLER]),
                value => encoded.push(value),
            }
        }
        Self(encoded)
    }

    /// Creates a key string from bytes already in internal form.
    pub fn from_encoded(bytes: Vec<u8>) -> Result<Self, KeyDecodeError> {
        validate_encoded(&bytes)?;
        Ok(Self(bytes))
    }

    /// Creates one named special key from its termcap bytes.
    pub fn special(second: u8, third: u8) -> Result<Self, KeyDecodeError> {
        if !(0x02..=0x7f).contains(&third) {
            return Err(KeyDecodeError::InvalidThirdByte(third));
        }
        Ok(Self(vec![K_SPECIAL, second, third]))
    }

    /// Encoded bytes consumed directly by mapping lookup.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Number of encoded bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the sequence contains no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Decodes all logical keys without losing named special-key identity.
    pub fn decode(&self) -> Result<Vec<Key>, KeyDecodeError> {
        let mut result = Vec::with_capacity(self.0.len());
        let mut offset = 0;
        while offset < self.0.len() {
            let (key, width) = decode_one(&self.0[offset..])?;
            result.push(key);
            offset += width;
        }
        Ok(result)
    }
}

impl From<&str> for Keys {
    fn from(value: &str) -> Self {
        Self::encode(value.as_bytes())
    }
}

impl From<Vec<u8>> for Keys {
    fn from(value: Vec<u8>) -> Self {
        Self::encode(&value)
    }
}

/// One decoded logical key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    /// Literal input byte, including quoted zero or `0x80`.
    Byte(u8),
    /// Named special key represented by its second and third internal bytes.
    Special(u8, u8),
}

/// Remapping policy carried alongside inserted typeahead bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Remap {
    /// Allow mappings to consume the inserted bytes.
    #[default]
    Yes,
    /// Do not remap any inserted byte.
    No,
    /// Do not remap the first byte, while allowing abbreviations.
    SkipFirst,
    /// Only script-local mappings may consume the bytes.
    Script,
}

/// Metadata copied to every encoded byte inserted into typeahead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TypeaheadFlags {
    /// Remapping policy.
    pub remap: Remap,
    /// Mapping modes in which the inserted keys participate.
    pub modes: MapModes,
    /// Buffer-local scope, when the producer is tied to one buffer.
    pub buffer: Option<BufHandle>,
    /// Whether input came from a mapping rather than direct typing.
    pub mapped: bool,
    /// Whether command output should remain silent while consuming it.
    pub silent: bool,
}

impl Default for TypeaheadFlags {
    fn default() -> Self {
        Self {
            remap: Remap::Yes,
            modes: MapModes::ALL,
            buffer: None,
            mapped: false,
            silent: false,
        }
    }
}

/// Invalid key encoding or stack offset.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TypeaheadError {
    /// Insertion offset exceeded the encoded byte length.
    #[error("typeahead insertion offset {offset} exceeds length {len}")]
    OffsetOutOfRange {
        /// Requested insertion offset.
        offset: usize,
        /// Current encoded byte length.
        len: usize,
    },
    /// Key bytes were malformed.
    #[error(transparent)]
    Decode(#[from] KeyDecodeError),
}

/// Malformed internal key sequence.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum KeyDecodeError {
    /// `K_SPECIAL` did not have both following bytes.
    #[error("truncated three-byte special key at byte {0}")]
    Truncated(usize),
    /// A special key's third byte was outside the reserved range.
    #[error("invalid special-key third byte {0:#x}")]
    InvalidThirdByte(u8),
    /// Quoted zero or `K_SPECIAL` used a non-filler third byte.
    #[error("quoted literal used invalid filler {0:#x}")]
    InvalidFiller(u8),
}

/// Stack-like typeahead buffer with insertion at an encoded-byte offset.
#[derive(Clone, Debug, Default)]
pub struct Typeahead {
    bytes: Vec<u8>,
    flags: Vec<TypeaheadFlags>,
}

impl Typeahead {
    /// Creates an empty typeahead buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            flags: Vec::new(),
        }
    }

    /// Inserts keys at `offset`; zero pushes onto the front like `ins_typebuf`.
    pub fn push(
        &mut self,
        keys: &Keys,
        offset: usize,
        flags: TypeaheadFlags,
    ) -> Result<(), TypeaheadError> {
        if offset > self.bytes.len() {
            return Err(TypeaheadError::OffsetOutOfRange {
                offset,
                len: self.bytes.len(),
            });
        }
        self.bytes
            .splice(offset..offset, keys.as_bytes().iter().copied());
        self.flags.splice(
            offset..offset,
            std::iter::repeat_n(flags, keys.len()),
        );
        Ok(())
    }

    /// Appends direct typed input after all queued bytes.
    pub fn append(&mut self, keys: &Keys, flags: TypeaheadFlags) {
        self.bytes.extend_from_slice(keys.as_bytes());
        self.flags
            .extend(std::iter::repeat_n(flags, keys.len()));
    }

    /// Queues input with `feedkeys()` mode semantics and reports whether the
    /// caller must execute the queue immediately (`x`). All input remains in
    /// this one buffer; `L` precedes raw input with `K_EVENT` so the normal
    /// state loop takes its event-processing path before consuming it.
    pub fn feedkeys(&mut self, keys: &Keys, mode: &str) -> Result<bool, TypeaheadError> {
        let flags = TypeaheadFlags {
            remap: if mode.contains('n') { Remap::No } else { Remap::Yes },
            ..TypeaheadFlags::default()
        };
        if mode.contains('L') {
            let event = Keys::special(KS_EXTRA, KE_EVENT)?;
            let mut low_level = event.as_bytes().to_vec();
            low_level.extend_from_slice(keys.as_bytes());
            let low_level = Keys::from_encoded(low_level)?;
            if mode.contains('i') { self.push(&low_level, 0, flags)?; } else { self.append(&low_level, flags); }
        } else if mode.contains('i') {
            self.push(keys, 0, flags)?;
        } else {
            self.append(keys, flags);
        }
        Ok(mode.contains('x'))
    }

    /// Encoded bytes used for prefix mapping lookup.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrows at most `keylen` encoded bytes for mapping lookup.
    #[must_use]
    pub fn keylen(&self, keylen: usize) -> &[u8] {
        &self.bytes[..keylen.min(self.bytes.len())]
    }

    /// Metadata for the first queued encoded byte.
    #[must_use]
    pub fn front_flags(&self) -> Option<TypeaheadFlags> {
        self.flags.first().copied()
    }

    /// Decodes the next logical key without consuming it.
    pub fn peek(&self) -> Result<Option<Key>, KeyDecodeError> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        decode_one(&self.bytes).map(|(key, _)| Some(key))
    }

    /// Removes and decodes the next logical key.
    pub fn pop(&mut self) -> Result<Option<Key>, KeyDecodeError> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        let (key, width) = decode_one(&self.bytes)?;
        self.bytes.drain(..width);
        self.flags.drain(..width);
        Ok(Some(key))
    }

    /// Removes `count` encoded bytes from the front.
    pub fn consume(&mut self, count: usize) -> usize {
        let count = count.min(self.bytes.len());
        self.bytes.drain(..count);
        self.flags.drain(..count);
        count
    }

    /// Discards every queued key and its metadata.
    pub fn flush(&mut self) {
        self.bytes.clear();
        self.flags.clear();
    }

    /// Number of encoded bytes, matching upstream `tb_len` semantics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether no encoded bytes are queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

fn validate_encoded(bytes: &[u8]) -> Result<(), KeyDecodeError> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (_, width) = decode_one(&bytes[offset..]).map_err(|error| match error {
            KeyDecodeError::Truncated(relative) => KeyDecodeError::Truncated(offset + relative),
            other => other,
        })?;
        offset += width;
    }
    Ok(())
}

fn decode_one(bytes: &[u8]) -> Result<(Key, usize), KeyDecodeError> {
    let Some(first) = bytes.first().copied() else {
        return Err(KeyDecodeError::Truncated(0));
    };
    if first != K_SPECIAL {
        return Ok((Key::Byte(first), 1));
    }
    if bytes.len() < 3 {
        return Err(KeyDecodeError::Truncated(0));
    }
    let second = bytes[1];
    let third = bytes[2];
    if !(0x02..=0x7f).contains(&third) {
        return Err(KeyDecodeError::InvalidThirdByte(third));
    }
    match second {
        KS_ZERO | KS_SPECIAL if third != KE_FILLER => Err(KeyDecodeError::InvalidFiller(third)),
        KS_ZERO => Ok((Key::Byte(0), 3)),
        KS_SPECIAL => Ok((Key::Byte(K_SPECIAL), 3)),
        _ => Ok((Key::Special(second, third), 3)),
    }
}

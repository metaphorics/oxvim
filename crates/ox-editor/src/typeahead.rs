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

    /// Quotes a trailing literal [`K_SPECIAL`] while preserving complete
    /// three-byte internal key codes.
    #[must_use]
    pub fn escape_ks(bytes: &[u8]) -> Self {
        let mut escaped = Vec::with_capacity(bytes.len().saturating_add(2));
        let mut offset = 0;
        while offset < bytes.len() {
            if bytes[offset] != K_SPECIAL {
                escaped.push(bytes[offset]);
                offset += 1;
                continue;
            }
            if bytes.len() - offset >= 3 {
                escaped.extend_from_slice(&bytes[offset..offset + 3]);
                offset += 3;
                continue;
            }
            escaped.extend_from_slice(&[K_SPECIAL, KS_SPECIAL, KE_FILLER]);
            offset += 1;
        }
        Self(escaped)
    }

    /// Creates a key string from bytes already in internal form.
    ///
    /// # Errors
    ///
    /// Returns [`KeyDecodeError`] if the bytes contain a truncated
    /// three-byte special key, an invalid special-key third byte, or a
    /// quoted literal with the wrong filler.
    pub fn from_encoded(bytes: Vec<u8>) -> Result<Self, KeyDecodeError> {
        validate_encoded(&bytes)?;
        Ok(Self(bytes))
    }

    /// Creates one named special key from its termcap bytes.
    ///
    /// # Errors
    ///
    /// Returns [`KeyDecodeError::InvalidThirdByte`] when `third` is outside
    /// the reserved `0x02..=0x7f` range.
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
    ///
    /// # Errors
    ///
    /// Returns [`KeyDecodeError`] if any three-byte special key in the
    /// sequence is truncated, has an invalid third byte, or uses the wrong
    /// filler for a quoted literal.
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

    /// Decodes `<...>` key notation into the internal bytes used by mappings
    /// and typeahead (`replace_termcodes`, `keycodes.c`).
    ///
    /// Literal bytes must be quoted, but complete modifier and special-key
    /// triples must not be quoted again. Building the result in its final form
    /// keeps those two byte domains separate.
    #[must_use]
    pub fn parse_notation(text: &str, leader: &str, local_leader: &str) -> Self {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'<' {
                append_raw(&mut out, &bytes[index..=index]);
                index += 1;
                continue;
            }
            let Some(close) = bytes[index + 1..].iter().position(|byte| *byte == b'>') else {
                append_raw(&mut out, &bytes[index..=index]);
                index += 1;
                continue;
            };
            let name = &text[index + 1..index + 1 + close];
            let Some(decoded) = named_key_bytes(name, leader, local_leader) else {
                append_raw(&mut out, &bytes[index..=index]);
                index += 1;
                continue;
            };
            out.extend_from_slice(&decoded);
            index += close + 2;
        }
        Self(out)
    }
}

/// Prefix for a modifier byte applied to a following key (keycodes.h:44).
pub const KS_MODIFIER: u8 = 0xfc;
/// Shift modifier mask (keycodes.h:467).
pub const MOD_MASK_SHIFT: u8 = 0x02;
/// Ctrl modifier mask (keycodes.h:468).
pub const MOD_MASK_CTRL: u8 = 0x04;
/// Alt/Meta modifier mask (keycodes.h:469).
pub const MOD_MASK_ALT: u8 = 0x08;
/// META when distinct from ALT (keycodes.h:470; the notation parser folds
/// `m` into ALT the way terminals deliver it).
pub const MOD_MASK_META: u8 = 0x10;
/// Double-click mask (keycodes.h:471).
pub const MOD_MASK_2CLICK: u8 = 0x20;
/// Triple-click mask (keycodes.h:472).
pub const MOD_MASK_3CLICK: u8 = 0x40;
/// Quadruple-click mask (keycodes.h:473).
pub const MOD_MASK_4CLICK: u8 = 0x60;
/// Command ("super") key mask (keycodes.h:474).
pub const MOD_MASK_CMD: u8 = 0x80;

fn append_raw(output: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        match *byte {
            0 => output.extend_from_slice(&[K_SPECIAL, KS_ZERO, KE_FILLER]),
            K_SPECIAL => output.extend_from_slice(&[K_SPECIAL, KS_SPECIAL, KE_FILLER]),
            byte => output.push(byte),
        }
    }
}

/// One `<...>` key name in final internal form.
fn named_key_bytes(name: &str, leader: &str, local_leader: &str) -> Option<Vec<u8>> {
    if name.eq_ignore_ascii_case("leader") {
        let mut output = Vec::with_capacity(leader.len());
        append_raw(&mut output, leader.as_bytes());
        return Some(output);
    }
    if name.eq_ignore_ascii_case("localleader") {
        let mut output = Vec::with_capacity(local_leader.len());
        append_raw(&mut output, local_leader.as_bytes());
        return Some(output);
    }

    let (mut rest, simplify) = name
        .strip_prefix('*')
        .map_or((name, true), |rest| (rest, false));
    let mut modifiers = 0;
    while let Some((prefix, tail)) = rest.split_once('-') {
        let modifier = if prefix.eq_ignore_ascii_case("s") {
            MOD_MASK_SHIFT
        } else if prefix.eq_ignore_ascii_case("c") || prefix.eq_ignore_ascii_case("ctrl") {
            MOD_MASK_CTRL
        } else if prefix.eq_ignore_ascii_case("m") || prefix.eq_ignore_ascii_case("a") {
            MOD_MASK_ALT
        } else {
            break;
        };
        modifiers |= modifier;
        rest = tail;
    }
    if rest.is_empty() {
        return None;
    }

    let mut raw = None;
    if rest.len() == 1 {
        let byte = rest.as_bytes()[0];
        if simplify && modifiers & MOD_MASK_SHIFT != 0 && byte.is_ascii_alphabetic() {
            modifiers &= !MOD_MASK_SHIFT;
            raw = Some(byte.to_ascii_uppercase());
        } else if simplify && modifiers & MOD_MASK_CTRL != 0 {
            let upper = byte.to_ascii_uppercase();
            raw = match upper {
                b'?' => Some(0x7f),
                b'@'..=b'_' => Some(upper & 0x1f),
                _ => None,
            };
            if raw.is_some() {
                modifiers &= !MOD_MASK_CTRL;
            }
        }
    }

    let lower = rest.to_ascii_lowercase();
    if raw.is_none() {
        raw = match lower.as_str() {
            "lt" => Some(b'<'),
            "bslash" => Some(b'\\'),
            "bar" => Some(b'|'),
            "space" => Some(b' '),
            "tab" => Some(0x09),
            "cr" | "return" | "enter" => Some(0x0d),
            "nl" | "newline" | "linefeed" | "lf" => Some(0x0a),
            "esc" | "escape" => Some(0x1b),
            "bs" | "backspace" => Some(0x08),
            "del" | "delete" => Some(0x7f),
            "nul" => Some(0x00),
            _ => None,
        };
    }

    let mut output = Vec::new();
    if modifiers != 0 {
        output.extend_from_slice(&[K_SPECIAL, KS_MODIFIER, modifiers]);
    }
    if let Some(raw) = raw {
        append_raw(&mut output, &[raw]);
        return Some(output);
    }
    if let [b'f', digit @ b'1'..=b'9'] = lower.as_bytes() {
        output.extend_from_slice(&[K_SPECIAL, b'k', *digit]);
        return Some(output);
    }
    if modifiers != 0 && rest.chars().count() == 1 {
        append_raw(&mut output, rest.as_bytes());
        return Some(output);
    }
    None
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

/// `str2specialbuf` (`message.c:2173-2187`), which drives `str2special`
/// (`message.c:2084-2166`) over a whole encoded key string: control bytes and
/// named special keys become `<>` notation and everything else passes through.
///
/// `replace_spaces` and `replace_lt` are upstream's two flags. `maparg()`'s
/// `lhs` key uses `(true, false)` and its `rhs` key and string form use
/// `(false, false)`; `:map`'s listing uses `(true, false)` for the lhs and
/// `(false, false)` for the rhs (`mapping.c:2096,2116,2206`, `showmap`
/// `mapping.c:236,262`).
///
/// Named gap: this port's [`Keys::parse_notation`] decodes `<BS>`, `<Del>`,
/// `<NL>` and `<Nul>` to plain bytes where upstream builds the special keys
/// `K_BS`, `K_DEL`, `K_NL` and `K_ZERO`, so those four render here as
/// `<C-H>`, a raw `0x7f`, `<NL>` and `<Nul>`. `<CR>`, `<Tab>`, `<Esc>` and
/// every `<C-x>` agree with upstream because they *are* plain bytes there too.
#[must_use]
pub fn special_notation(bytes: &[u8], replace_spaces: bool, replace_lt: bool) -> String {
    // Bytes, not chars: a multi-byte character passes through one byte at a
    // time, so pushing `char::from(byte)` would re-encode each of its bytes as
    // a separate code point.
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        // `mb_unescape` runs first in `str2special`, so a quoted literal byte
        // is rendered as the byte it stands for, not as a special key.
        let (byte, width) = match bytes[offset..] {
            [K_SPECIAL, KS_ZERO, KE_FILLER, ..] => (0u8, 3),
            [K_SPECIAL, KS_SPECIAL, KE_FILLER, ..] => (K_SPECIAL, 3),
            [K_SPECIAL, second, third, ..] => {
                // A named special key this port cannot name; upstream prints
                // its termcap pair as `<t_xx>` (`keycodes.c:324-327`).
                out.extend_from_slice(b"<t_");
                out.extend_from_slice(&[second, third, b'>']);
                offset += 3;
                continue;
            }
            [byte, ..] => (byte, 1),
            [] => break,
        };
        offset += width;
        match byte {
            b' ' if replace_spaces => out.extend_from_slice(b"<Space>"),
            b'<' if replace_lt => out.extend_from_slice(b"<lt>"),
            0x00 => out.extend_from_slice(b"<Nul>"),
            0x09 => out.extend_from_slice(b"<Tab>"),
            0x0a => out.extend_from_slice(b"<NL>"),
            0x0d => out.extend_from_slice(b"<CR>"),
            0x1b => out.extend_from_slice(b"<Esc>"),
            // `get_special_key` (`keycodes.c:292-297`): a control byte with no
            // table entry becomes `<C-` plus the byte offset by `@`.
            0x01..=0x1f => {
                out.extend_from_slice(b"<C-");
                out.extend_from_slice(&[byte | 0x40, b'>']);
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
    ///
    /// # Errors
    ///
    /// Returns [`TypeaheadError::OffsetOutOfRange`] when `offset` exceeds the
    /// current encoded byte length.
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
        self.flags
            .splice(offset..offset, std::iter::repeat_n(flags, keys.len()));
        Ok(())
    }

    /// Appends direct typed input after all queued bytes.
    pub fn append(&mut self, keys: &Keys, flags: TypeaheadFlags) {
        self.bytes.extend_from_slice(keys.as_bytes());
        self.flags.extend(std::iter::repeat_n(flags, keys.len()));
    }

    /// Queues input with `feedkeys()` mode semantics and reports whether the
    /// caller must execute the queue immediately (`x`). All input remains in
    /// this one buffer; `L` precedes raw input with `K_EVENT` so the normal
    /// state loop takes its event-processing path before consuming it.
    ///
    /// # Errors
    ///
    /// Returns [`TypeaheadError`] if the `L` mode's synthetic event key is
    /// malformed or the resulting low-level sequence fails encoding
    /// validation, or if the underlying [`push`][Typeahead::push] fails.
    pub fn feedkeys(&mut self, keys: &Keys, mode: &str) -> Result<bool, TypeaheadError> {
        let flags = TypeaheadFlags {
            remap: if mode.contains('n') {
                Remap::No
            } else {
                Remap::Yes
            },
            ..TypeaheadFlags::default()
        };
        if mode.contains('L') {
            let event = Keys::special(KS_EXTRA, KE_EVENT)?;
            let mut low_level = event.as_bytes().to_vec();
            low_level.extend_from_slice(keys.as_bytes());
            let low_level = Keys::from_encoded(low_level)?;
            if mode.contains('i') {
                self.push(&low_level, 0, flags)?;
            } else {
                self.append(&low_level, flags);
            }
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

    /// Marks the front byte un-remappable, so mapping lookup skips it.
    ///
    /// `vgetorpeek` clears one byte of `typebuf.tb_noremap` when an
    /// incomplete mapping times out with no complete match behind it, which is
    /// what releases the queued keys literally.
    pub fn deny_front_remap(&mut self) {
        if let Some(flags) = self.flags.first_mut() {
            flags.remap = Remap::No;
        }
    }

    /// Decodes the next logical key without consuming it.
    ///
    /// # Errors
    ///
    /// Returns [`KeyDecodeError`] if the front of the buffer is a malformed
    /// three-byte special key (`None` is returned only when the buffer is
    /// empty, not on decode failure).
    pub fn peek(&self) -> Result<Option<Key>, KeyDecodeError> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        decode_one(&self.bytes).map(|(key, _)| Some(key))
    }

    /// Removes and decodes the next logical key.
    ///
    /// # Errors
    ///
    /// Returns [`KeyDecodeError`] if the front of the buffer is a malformed
    /// three-byte special key (`None` is returned only when the buffer is
    /// empty, not on decode failure).
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

    /// `flush_buffers(FLUSH_MINIMAL)` (`input.c:473-499`): discards the
    /// *mapped* run at the front of the queue and leaves typed input alone.
    ///
    /// This is how an error in Normal mode abandons the rest of what produced
    /// it. `:normal` stuffs its argument with `ins_typebuf(..., nottyped =
    /// true)`, which counts the whole argument into `tb_maplen`
    /// (`input.c:964-966`), so the remainder of a `:normal` goes here too.
    pub fn flush_mapped(&mut self) {
        let mapped = self.flags.iter().take_while(|flags| flags.mapped).count();
        self.bytes.drain(..mapped);
        self.flags.drain(..mapped);
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

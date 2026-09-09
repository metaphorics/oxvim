//! Object <-> msgpack conversions and the incremental frame decoder.
//!
//! # Upstream mapping
//!
//! `Object` mirrors `src/nvim/msgpack_rpc/packer.c`'s `mpack_object_inner()`:
//!
//!   - `String` is always packed as a msgpack **str**, regardless of UTF-8
//!     validity, matching upstream `packer.c mpack_str()`. Decoding accepts
//!     both **str** and **bin** (`unpacker.c`); both become `Object::String`.
//!   - `LuaRef` is packed as a human-readable string `<Lua <n>>`, exactly as
//!     `packer.c` (`nlua_funcref_str(ref, NULL, true)`, `executor.c:2481`).
//!   - Handles are packed as msgpack EXT with type
//!     `ObjectType - EXT_OBJECT_TYPE_SHIFT` (`packer.c mpack_handle()`):
//!     Buffer=0, Window=1, Tabpage=2. The payload is the handle as a
//!     non-negative integer. For `handle <= 0x7f` upstream uses fixext1
//!     (`0xd4`); larger handles get a uint-encoded payload. We build the same
//!     uint payload bytes and let `rmpv` pick the most compact EXT header.
//!     That means a 2-byte payload becomes fixext2 (`0xd5`) while upstream
//!     forces ext8 (`0xc7`) for any payload — the decoded handle is identical,
//!     so we deliberately keep rmpv's compact form.
//!   - Decoding an EXT with an out-of-range type or an unparseable payload
//!     yields `Nil`, mirroring `unpacker.c` (`*res = NIL;`).
//!
//! `IncrementalDecoder` mirrors `unpacker.c`'s "3-state FSM" (see the big
//! comment in `unpacker.c`): it refuses to emit a message until a whole frame
//! is present, handles a frame split anywhere, and yields every complete
//! message from each `feed`. Instead of a hand-rolled FSM it classifies
//! `rmpv::decode::Error` by `io::ErrorKind`: end-of-input means "need more
//! bytes" (wait for the next read); anything else is barfed input and fails
//! with a typed [`DecodeError`].

use std::io::{Cursor, ErrorKind, Write};

use ox_types::{
    BufHandle, Dict, EXT_TYPE_BUFFER, EXT_TYPE_TABPAGE, EXT_TYPE_WINDOW, HandleError, Object,
    OxStr, TabHandle, WinHandle,
};
use rmp::encode::{write_array_len, write_map_len, write_str_len};
use rmpv::{Integer, Value};

use crate::message::Message;

/// An error produced while decoding msgpack into [`Object`] / [`Message`].
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The byte stream is not valid `MessagePack` (reserved/invalid marker,
    /// depth-limit overrun, ...).
    #[error("malformed msgpack: {0}")]
    Malformed(String),
    /// A msgpack unsigned integer exceeds the signed 64-bit `Object::Integer`
    /// domain.
    #[error("msgpack unsigned integer {0} is out of the i64 Object range")]
    IntegerOutOfRange(u64),
    /// An editor-handle EXT carries a payload that is not a valid handle.
    #[error("bad editor handle in EXT payload")]
    Handle(#[from] HandleError),
    /// The stream ended mid-value. [`IncrementalDecoder`] treats this as "wait
    /// for more bytes", never as a hard error.
    #[error("incomplete msgpack frame: more bytes required")]
    Incomplete,
    /// Too many bytes were buffered without a complete frame resolving.
    #[error("incoming frame exceeds the decoder limit of {limit} bytes")]
    Oversized {
        /// The configured [`IncrementalDecoder`] byte limit.
        limit: usize,
    },
    /// A decoded value has the wrong shape for a request/response/notification.
    #[error("bad message shape: {0}")]
    Message(String),
}

/// Default upper bound for the [`IncrementalDecoder`] staging buffer. Bounding
/// the buffer keeps a hostile peer from making us hold unbounded memory while
/// we wait for a frame that never completes.
pub const DEFAULT_DECODE_LIMIT: usize = 64 * 1024 * 1024;

/// Write a raw byte sequence as a msgpack **str** header and payload.
///
/// This intentionally ignores UTF-8 validity, matching upstream `packer.c`
/// `mpack_str()` which treats Object strings as opaque byte sequences.
fn write_str_bytes<W: Write>(out: &mut W, bytes: &[u8]) -> Result<(), rmpv::encode::Error> {
    let len = u32::try_from(bytes.len()).map_err(|_| {
        rmpv::encode::Error::InvalidDataWrite(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "string length exceeds u32",
        ))
    })?;
    write_str_len(out, len)?;
    out.write_all(bytes)
        .map_err(rmpv::encode::Error::InvalidDataWrite)
}

/// Encode an [`Object`] directly into a msgpack byte stream.
///
/// `Object::String` is always encoded as a msgpack **str**, even when the
/// bytes are not valid UTF-8, so the wire format matches upstream Neovim.
pub(crate) fn write_object<W: Write>(out: &mut W, obj: &Object) -> Result<(), rmpv::encode::Error> {
    match obj {
        Object::Nil => rmpv::encode::write_value(out, &Value::Nil),
        Object::Boolean(b) => rmpv::encode::write_value(out, &Value::Boolean(*b)),
        Object::Integer(n) => rmpv::encode::write_value(out, &Value::Integer(Integer::from(*n))),
        Object::Float(f) => rmpv::encode::write_value(out, &Value::F64(*f)),
        Object::String(s) => write_str_bytes(out, s.as_bytes()),
        Object::Array(items) => {
            let len = u32::try_from(items.len()).map_err(|_| {
                rmpv::encode::Error::InvalidDataWrite(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "array length exceeds u32",
                ))
            })?;
            write_array_len(out, len)?;
            for item in items {
                write_object(out, item)?;
            }
            Ok(())
        }
        Object::Dict(d) => {
            let len = u32::try_from(d.0.len()).map_err(|_| {
                rmpv::encode::Error::InvalidDataWrite(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "dict length exceeds u32",
                ))
            })?;
            write_map_len(out, len)?;
            for (k, v) in &d.0 {
                write_str_bytes(out, k.as_bytes())?;
                write_object(out, v)?;
            }
            Ok(())
        }
        Object::LuaRef(r) => {
            let s = format!("<Lua {r}>");
            write_str_bytes(out, s.as_bytes())
        }
        Object::Buffer(h) => rmpv::encode::write_value(
            out,
            &Value::Ext(EXT_TYPE_BUFFER, handle_payload(i64::from(*h))),
        ),
        Object::Window(h) => rmpv::encode::write_value(
            out,
            &Value::Ext(EXT_TYPE_WINDOW, handle_payload(i64::from(*h))),
        ),
        Object::Tabpage(h) => rmpv::encode::write_value(
            out,
            &Value::Ext(EXT_TYPE_TABPAGE, handle_payload(i64::from(*h))),
        ),
    }
}

/// Encode a handle as the uint payload bytes upstream `packer.c mpack_handle()`
/// writes: a single byte for `0..=0x7f` (fixext1), otherwise `mpack_uint`.
fn handle_payload(handle: i64) -> Vec<u8> {
    // `handle` is always non-negative by construction: it comes from an
    // `i64::from(&handle)` of a handle type that rejects negatives. Index the
    // big-endian representation so every byte is extracted without a lossy
    // narrowing cast; the four width branches mirror upstream mpack_uint.
    let bytes = handle.to_be_bytes();
    if handle <= 0x7f {
        vec![bytes[7]]
    } else if handle <= 0xff {
        vec![0xcc, bytes[7]]
    } else if handle <= 0xffff {
        vec![0xcd, bytes[6], bytes[7]]
    } else {
        vec![0xce, bytes[4], bytes[5], bytes[6], bytes[7]]
    }
}

/// Convert a decoded `rmpv::Value` into an [`Object`].
pub fn object_from_value(value: Value) -> Result<Object, DecodeError> {
    match value {
        Value::Nil => Ok(Object::Nil),
        Value::Boolean(b) => Ok(Object::Boolean(b)),
        Value::Integer(i) => match i.as_i64() {
            Some(n) => Ok(Object::Integer(n)),
            None => Err(DecodeError::IntegerOutOfRange(
                i.as_u64().unwrap_or(u64::MAX),
            )),
        },
        Value::F32(f) => Ok(Object::Float(f64::from(f))),
        Value::F64(f) => Ok(Object::Float(f)),
        Value::String(s) => Ok(Object::String(OxStr(s.into_bytes()))),
        Value::Binary(b) => Ok(Object::String(OxStr(b))),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(object_from_value(item)?);
            }
            Ok(Object::Array(out))
        }
        Value::Map(pairs) => {
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                let key = match k {
                    Value::String(s) => OxStr(s.into_bytes()),
                    Value::Binary(b) => OxStr(b),
                    other => {
                        return Err(DecodeError::Message(format!(
                            "dict key is not a string: {other:?}"
                        )));
                    }
                };
                out.push((key, object_from_value(v)?));
            }
            Ok(Object::Dict(Dict(out)))
        }
        Value::Ext(ext_type, payload) => {
            let Some(handle) = ext_payload_uint(&payload) else {
                // Unknown/unparseable EXT: upstream sets NIL (unpacker.c).
                return Ok(Object::Nil);
            };
            match ext_type {
                EXT_TYPE_BUFFER => BufHandle::try_from(handle)
                    .map(Object::Buffer)
                    .map_err(DecodeError::Handle),
                EXT_TYPE_WINDOW => WinHandle::try_from(handle)
                    .map(Object::Window)
                    .map_err(DecodeError::Handle),
                EXT_TYPE_TABPAGE => TabHandle::try_from(handle)
                    .map(Object::Tabpage)
                    .map_err(DecodeError::Handle),
                _ => Ok(Object::Nil),
            }
        }
    }
}

/// Parse an EXT payload as a non-negative integer, mirroring the uint token
/// `unpacker.c` requires (`MPACK_TOKEN_UINT`, then `mpack_unpack_uint`).
fn ext_payload_uint(payload: &[u8]) -> Option<i64> {
    match *payload {
        [b] => Some(i64::from(b)),
        [0xcc, low] => Some(i64::from(low)),
        [0xcd, hi, lo] => Some(i64::from(u16::from_be_bytes([hi, lo]))),
        [0xce, b0, b1, b2, b3] => Some(i64::from(u32::from_be_bytes([b0, b1, b2, b3]))),
        [0xcf, b0, b1, b2, b3, b4, b5, b6, b7] => {
            i64::try_from(u64::from_be_bytes([b0, b1, b2, b3, b4, b5, b6, b7])).ok()
        }
        _ => None,
    }
}

/// An error produced while encoding an [`Object`] or [`Message`] to msgpack.
///
/// Encoding is infallible for well-formed objects; the only failure is an
/// `Object` that violates the wire limits — a string, array, or dictionary
/// whose length exceeds the `u32` msgpack length field — or an underlying
/// sink write failure.
#[derive(Debug, thiserror::Error)]
#[error("could not encode msgpack: {0}")]
pub struct EncodeError(#[from] rmpv::encode::Error);


/// Encode a single [`Object`] into `out` as its msgpack byte representation.
///
/// # Errors
///
/// Returns [`EncodeError`] when `obj` violates the msgpack wire limits — a
/// string, array, or dictionary whose length exceeds the `u32` length field —
/// or when `out` itself fails. On error `out` may hold a partial frame; it
/// must not be transmitted.
pub fn encode<W: Write>(out: &mut W, obj: &Object) -> Result<(), EncodeError> {
    write_object(out, obj).map_err(EncodeError)
}

/// Decode exactly one [`Object`] from the front of `bytes`.
///
/// Trailing bytes are left undecoded (callers feeding a stream should use
/// [`IncrementalDecoder`]); this is a convenience for known-single-object
/// payloads and tests.
///
/// # Errors
///
/// Returns [`DecodeError::Incomplete`] if `bytes` ends mid-value, or
/// [`DecodeError::Malformed`] for any other msgpack decode failure. Errors from
/// [`object_from_value`] (integer out of range, bad handle, bad dict key) are
/// propagated unchanged.
pub fn decode(bytes: &[u8]) -> Result<Object, DecodeError> {
    object_from_value(decode_one(bytes)?)
}

/// Upper bound on msgpack nesting depth for decoding. Nesting beyond this
/// (adversarially crafted) input returns [`DecodeError::Malformed`] instead of
/// risking a recursive stack overflow inside `rmpv`. Legitimate RPC frames are
/// far shallower than this (grid redraw cells top out around depth 5).
pub const MAX_MESSAGE_DEPTH: usize = 64;

/// Decode one msgpack value, classifying end-of-input as
/// [`DecodeError::Incomplete`] and anything else as [`DecodeError::Malformed`].
fn decode_one(bytes: &[u8]) -> Result<Value, DecodeError> {
    let mut cursor = Cursor::new(bytes);
    match rmpv::decode::read_value_with_max_depth(&mut cursor, MAX_MESSAGE_DEPTH) {
        Ok(value) => Ok(value),
        Err(e) => match e.kind() {
            // Ran off the end mid-value: this is a prefix of a larger frame.
            ErrorKind::UnexpectedEof => Err(DecodeError::Incomplete),
            _ => Err(DecodeError::Malformed(e.to_string())),
        },
    }
}

/// A terminal [`IncrementalDecoder::feed`] failure.
///
/// One feed can decode a valid prefix of messages and still hit a frame that
/// cannot be decoded. The prefix rides along in [`Self::messages`] so the
/// caller can drain it before reporting [`Self::error`]; the error is never
/// dropped in favor of the prefix.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct FeedError {
    /// Complete messages decoded before the failing frame, in wire order.
    /// Drain them before reporting `error`.
    pub messages: Vec<Message>,
    /// The decode failure that ended the feed.
    #[source]
    pub error: DecodeError,
}

/// Incremental msgpack-RPC frame decoder.
///
/// Feed arbitrary byte slices; the decoder yields every complete [`Message`]
/// carried so far. It never panics: garbage outside a valid frame becomes a
/// typed [`DecodeError`], and input that never resolves into a frame is capped
/// at [`DEFAULT_DECODE_LIMIT`] (configurable via [`Self::with_limit`]).
///
/// Conceptually this is `unpacker.c`'s header-then-payload FSM: bytes are held
/// back until the full frame is present, partial frames may be split anywhere,
/// and one `feed` may produce many messages.
pub struct IncrementalDecoder {
    buf: Vec<u8>,
    /// Byte offset into `buf` where the next message begins; everything before
    /// this cursor has already been decoded/consumed but is retained so later
    /// `feed`s do not O(n) shift the staging buffer.
    cursor: usize,
    limit: usize,
}

impl IncrementalDecoder {
    /// A decoder with the default [`DEFAULT_DECODE_LIMIT`] staging cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            limit: DEFAULT_DECODE_LIMIT,
        }
    }

    /// A decoder with a custom staging cap.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            limit,
        }
    }

    /// Feed a byte slice, decoding any complete messages it completes.
    ///
    /// If a frame decodes as valid msgpack but is not a valid message shape,
    /// or the input is malformed or pushes the staging buffer over the limit,
    /// the decoder stops at that frame and fails. Messages already decoded in
    /// the same feed ride along in [`FeedError::messages`] so the caller can
    /// drain the valid prefix before surfacing the terminal error; the
    /// undecoded tail is discarded so the decoder can be reused.
    ///
    /// # Errors
    ///
    /// Returns [`FeedError`] wrapping [`DecodeError::Oversized`] when buffered
    /// input exceeds the configured limit without resolving a frame,
    /// [`DecodeError::Malformed`] for unparseable input, and any
    /// [`DecodeError::Message`] shape error from [`Message::from_value`].
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Message>, FeedError> {
        if !bytes.is_empty() {
            self.buf.extend_from_slice(bytes);
        }
        // Amount of data from the front of the buffer that has already been
        // decoded in previous feeds. Updated after each successfully decoded
        // message in this feed.
        let mut consumed = self.cursor;
        let mut out = Vec::new();
        let mut error: Option<DecodeError> = None;

        while consumed < self.buf.len() && error.is_none() {
            let mut cursor = Cursor::new(&self.buf[consumed..]);
            match rmpv::decode::read_value_with_max_depth(&mut cursor, MAX_MESSAGE_DEPTH) {
                Ok(value) => {
                    let Ok(bytes_used) = usize::try_from(cursor.position()) else {
                        error = Some(DecodeError::Malformed(
                            "cursor position overflow".into(),
                        ));
                        break;
                    };
                    // Validate message shape before consuming the frame.
                    match Message::from_value(value) {
                        Ok(message) => {
                            consumed += bytes_used;
                            out.push(message);
                        }
                        Err(e) => {
                            error = Some(e);
                        }
                    }
                }
                Err(e) => match e.kind() {
                    // End of input while reading: valid prefix, wait for more.
                    ErrorKind::UnexpectedEof => {
                        if self.buf.len() - consumed > self.limit {
                            error = Some(DecodeError::Oversized { limit: self.limit });
                        }
                        break;
                    }
                    _ => {
                        error = Some(DecodeError::Malformed(e.to_string()));
                    }
                },
            }
        }
        self.cursor = consumed;
        if let Some(error) = error {
            // The undecodable tail (including the bad frame) is discarded so
            // the decoder stays reusable; the valid prefix rides along in the
            // error so the caller can drain it before reporting the failure.
            self.buf.clear();
            self.cursor = 0;
            return Err(FeedError {
                messages: out,
                error,
            });
        }

        if self.cursor >= self.buf.len() {
            // Entire buffer consumed: compact by resetting.
            self.buf.clear();
            self.cursor = 0;
        } else if self.cursor >= 1024 * 1024 {
            // Lazy compaction: once at least 1 MiB of prefix has been consumed,
            // discard it so the staging buffer does not grow indefinitely for
            // streaming protocols. Shifting down is O(n) but is amortized over
            // the bytes that would otherwise keep accumulating.
            let remaining = self.buf.split_off(self.cursor);
            self.buf = remaining;
            self.cursor = 0;
        }
        Ok(out)
    }

    /// Whether no undecoded bytes are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cursor >= self.buf.len()
    }

    /// Number of undecoded bytes currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.cursor
    }
}
impl Default for IncrementalDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{Message, MsgidCounter};
    use ox_types::TabHandle;

    // Encode through the public `encode` into a fresh buffer.
    fn encoded(obj: &Object) -> Vec<u8> {
        let mut out = Vec::new();
        encode(&mut out, obj).unwrap();
        out
    }


    #[test]
    fn object_round_trip_all_kinds() {
        // Keys are already sorted so the decoded (order-preserving) dict matches.
        let obj = Object::Dict(Dict(vec![
            (OxStr::from("a"), Object::Integer(-7)),
            (OxStr::from("b"), Object::Boolean(true)),
            (OxStr::from("c"), Object::Nil),
            (OxStr::from("d"), Object::Float(3.5)),
            (
                OxStr::from("e"),
                Object::Array(vec![
                    Object::String(OxStr::from("x")),
                    Object::Buffer(BufHandle::try_from(9).unwrap()),
                    Object::Window(ox_types::WinHandle::try_from(128).unwrap()),
                    Object::Tabpage(TabHandle::try_from(1).unwrap()),
                ]),
            ),
        ]));
        assert_eq!(decode(&encoded(&obj)).unwrap(), obj);
    }

    #[test]
    fn luaref_and_negative_int_encode() {
        // "<Lua 42>" is 8 chars -> fixstr 0xa8 (executor.c nlua_funcref_str).
        assert_eq!(encoded(&Object::LuaRef(42)), b"\xa8<Lua 42>");
        assert_eq!(encoded(&Object::Integer(-1)), &[0xff]);
        assert_eq!(encoded(&Object::Integer(-32)), &[0xe0]);
        assert_eq!(encoded(&Object::Integer(-33)), &[0xd0, 0xdf]);
        // Integer 128 uses uint8 0xcc, mirroring upstream mpack_uint.
        assert_eq!(encoded(&Object::Integer(128)), &[0xcc, 0x80]);
    }

    #[test]
    fn handle_ext_round_trip() {
        for h in [0, 1, 0x7f, 0x80, 0xffff, 0x10000, i32::MAX - 1] {
            let handle = BufHandle::try_from(i64::from(h)).unwrap();
            let decoded = decode(&encoded(&Object::Buffer(handle))).unwrap();
            assert_eq!(decoded, Object::Buffer(handle), "handle {h}");
        }
    }

    #[test]
    fn small_handle_is_fixext1() {
        assert_eq!(
            encoded(&Object::Buffer(BufHandle::try_from(7).unwrap())),
            &[0xd4, 0, 7]
        );
    }

    #[test]
    fn non_utf8_string_encodes_as_str() {
        // Non-UTF-8 bytes must ride as msgpack str, not bin, to match upstream.
        let raw = OxStr::from(&[0xff, b'x', 0x00][..]);
        let encoded = encoded(&Object::String(raw.clone()));
        assert_eq!(encoded, [0xa3, 0xff, b'x', 0x00]);
        assert_eq!(decode(&encoded).unwrap(), Object::String(raw));
    }

    #[test]
    fn str_and_bin_both_decode_to_oxstr() {
        let raw = OxStr::from(&[0xff, b'x'][..]);
        // Manually-built str and bin frames with identical payload bytes.
        let as_str = [0xa2, 0xff, b'x'];
        let as_bin = [0xc4, 0x02, 0xff, b'x'];
        assert_eq!(decode(&as_str).unwrap(), Object::String(raw.clone()));
        assert_eq!(decode(&as_bin).unwrap(), Object::String(raw));
    }

    #[test]
    fn incremental_split_at_every_offset() {
        let m1 = Message::Notification {
            method: OxStr::from("nvim_echo"),
            params: vec![Object::Array(vec![Object::String(OxStr::from("hi"))])],
        };
        let m2 = Message::Request {
            msgid: 5,
            method: OxStr::from("nvim_get_mode"),
            params: vec![],
        };
        let frames: Vec<u8> = [m1.encode_bytes().unwrap(), m2.encode_bytes().unwrap()].concat();
        for split in 0..=frames.len() {
            let mut dec = IncrementalDecoder::new();
            let mut got = dec.feed(&frames[..split]).unwrap();
            got.extend(dec.feed(&frames[split..]).unwrap());
            assert_eq!(got, vec![m1.clone(), m2.clone()], "split at {split}");
            assert!(dec.is_empty());
        }
    }

    #[test]
    fn multi_message_single_read() {
        let mut dec = IncrementalDecoder::new();
        let mut counter = MsgidCounter::new();
        let a = Message::Request {
            msgid: counter.next_id(),
            method: OxStr::from("a"),
            params: vec![],
        };
        let b = Message::Request {
            msgid: counter.next_id(),
            method: OxStr::from("b"),
            params: vec![],
        };
        let c = Message::Request {
            msgid: counter.next_id(),
            method: OxStr::from("c"),
            params: vec![],
        };
        let blob = [
            a.encode_bytes().unwrap(),
            b.encode_bytes().unwrap(),
            c.encode_bytes().unwrap(),
        ]
        .concat();
        let got = dec.feed(&blob).unwrap();
        assert_eq!(got, vec![a, b, c]);
        assert!(dec.is_empty());
    }

    #[test]
    fn garbage_yields_typed_error_not_panic() {
        let mut dec = IncrementalDecoder::new();
        // 0xc1 (reserved marker) is decoded by rmpv as Nil, which is not a valid
        // message frame -> a typed DecodeError::Message, never a panic.
        assert!(dec.feed(&[0xc1, 0x01, 0x02]).is_err());
        assert!(dec.is_empty(), "buffer cleared after error");

        // Genuinely malformed input: >1024 nested array headers exceed the depth
        // limit and surface as DecodeError::Malformed.
        let mut nested = vec![0x91u8; 2000];
        let mut dec2 = IncrementalDecoder::new();
        let failure = dec2.feed(&nested).unwrap_err();
        assert!(
            matches!(failure.error, DecodeError::Malformed(_)),
            "{:?}",
            failure.error
        );
        let _ = nested.pop();

        // Decoder is still usable afterwards.
        let ok = Message::Notification {
            method: OxStr::from("t"),
            params: vec![],
        };
        assert_eq!(dec.feed(&ok.encode_bytes().unwrap()).unwrap(), vec![ok]);
    }

    #[test]
    fn garbage_prefix_is_incomplete_then_never_resolves() {
        // A str32 header declaring many bytes followed by just a few payload
        // bytes is a valid prefix -> incomplete, capped by the limit -> typed
        // Oversized error.
        let mut dec = IncrementalDecoder::with_limit(10);
        let mut input = vec![0xdb, 0x00, 0x00, 0x01, 0x2c]; // str32 len 300
        input.extend(std::iter::repeat_n(b'a', 11));
        let failure = dec.feed(&input).unwrap_err();
        assert!(
            matches!(failure.error, DecodeError::Oversized { limit: 10 }),
            "{:?}",
            failure.error
        );
        assert!(dec.is_empty());
    }

    #[test]
    fn one_meg_frame() {
        // A Notification whose params array holds ~1.2 MiB of integers.
        let big = Message::Notification {
            method: OxStr::from("big"),
            params: vec![Object::Array(vec![Object::Integer(0); 1_200_000])],
        };
        let encoded = big.encode_bytes().unwrap();
        assert!(encoded.len() > 1024 * 1024);
        let mut dec = IncrementalDecoder::new();
        let msgs = dec.feed(&encoded).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::Notification { .. }));
        assert!(dec.is_empty());
    }

    #[test]
    fn feed_error_carries_the_decoded_prefix() {
        let good = Message::Notification {
            method: OxStr::from("ok"),
            params: vec![],
        };
        // Valid frame followed by a malformed tail in one feed: the valid
        // message rides along in the error so the caller can drain the prefix
        // before surfacing the terminal failure; the bad tail is discarded
        // and the decoder is left empty and reusable.
        let mut dec = IncrementalDecoder::new();
        let blob = [
            good.encode_bytes().unwrap(),
            vec![0x91u8; 100], // nested array headers past the depth limit
        ]
        .concat();
        let failure = dec.feed(&blob).unwrap_err();
        assert_eq!(failure.messages, vec![good.clone()]);
        assert!(
            matches!(failure.error, DecodeError::Malformed(_)),
            "{:?}",
            failure.error
        );
        assert!(dec.is_empty(), "buffer cleared after error");
        // Reusable: a fresh, different message decodes normally.
        let fresh = Message::Notification {
            method: OxStr::from("fresh"),
            params: vec![],
        };
        assert_eq!(dec.feed(&fresh.encode_bytes().unwrap()).unwrap(), vec![fresh]);

        // Same contract for an oversized tail: under a small staging limit an
        // incomplete tail larger than the limit still reports the earlier
        // frame inside the error and leaves the decoder reusable.
        let mut dec = IncrementalDecoder::with_limit(8);
        let mut tail = vec![0xdb, 0x00, 0x00, 0x01, 0x2c]; // str32 len 300
        tail.extend(std::iter::repeat_n(b'a', 4)); // 9 buffered tail bytes > 8
        let blob = [good.encode_bytes().unwrap(), tail].concat();
        let failure = dec.feed(&blob).unwrap_err();
        assert_eq!(failure.messages, vec![good]);
        assert!(
            matches!(failure.error, DecodeError::Oversized { limit: 8 }),
            "{:?}",
            failure.error
        );
        assert!(dec.is_empty(), "buffer cleared after oversized tail");
    }

    #[test]
    fn encode_failure_is_an_error_not_a_frame() {
        // A sink that rejects every write stands in for the u32 wire-limit
        // rejection, which needs a >4 GiB object that cannot be allocated in
        // a test: the failure must surface as an error, never as a byte frame
        // a caller could mistake for a successful send.
        struct Reject;
        impl Write for Reject {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "collection length exceeds u32",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(encode(&mut Reject, &Object::Integer(0)).is_err());
    }
}

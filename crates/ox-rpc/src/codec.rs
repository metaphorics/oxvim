//! Object <-> msgpack conversions and the incremental frame decoder.
//!
//! # Upstream mapping
//!
//! `Object` mirrors `src/nvim/msgpack_rpc/packer.c`'s `mpack_object_inner()`:
//!
//!   - `String` is packed as a msgpack **str** when valid UTF-8, else as a
//!     **bin** (`Value::Binary`). Upstream always emits `str`
//!     (`packer.c mpack_str`), even for arbitrary byte strings; `rmpv` can only
//!     hold lossless arbitrary bytes in its `Binary` form, so non-UTF-8 strings
//!     ride as bin and decode back to the same `OxStr`. Both str and bin decode
//!     to `Object::String`.
//!   - `LuaRef` is packed as a human-readable string `<Lua <n>>`, exactly as
//!     `packer.c` (`nlua_funcref_str(ref, NULL, true)`, `executor.c:2481`).
//!   - Handles are packed as msgpack EXT with type
//!     `ObjectType - EXT_OBJECT_TYPE_SHIFT` (`packer.c mpack_handle()`):
//!     Buffer=0, Window=1, Tabpage=2. The payload is the handle as a
//!     non-negative integer. For `handle <= 0x7f` upstream uses fixext1
//!     (`0xd4`); larger handles get a uint-encoded payload inside ext8. We
//!     build the same uint payload bytes and let `rmpv` pick the header; for a
//!     2-byte payload `rmpv` emits fixext2 (`0xd5`) where upstream uses ext8
//!     (`0xc7`) — every reader decodes both identically.
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

use std::io::{Cursor, ErrorKind};

use ox_types::{
    BufHandle, Dict, HandleError, Object, OxStr, TabHandle, WinHandle, EXT_TYPE_BUFFER,
    EXT_TYPE_TABPAGE, EXT_TYPE_WINDOW,
};
use rmpv::{Integer, Utf8String, Value};

use crate::message::Message;

/// An error produced while decoding msgpack into [`Object`] / [`Message`].
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The byte stream is not valid MessagePack (reserved/invalid marker,
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

/// Convert an [`Object`] into an `rmpv::Value` for wire building.
pub fn value_from_object(obj: &Object) -> Value {
    match obj {
        Object::Nil => Value::Nil,
        Object::Boolean(b) => Value::Boolean(*b),
        Object::Integer(n) => Value::Integer(Integer::from(*n)),
        Object::Float(f) => Value::F64(*f),
        Object::String(s) => {
            // Valid UTF-8 rides as msgpack str; arbitrary bytes as bin. Both
            // decode back to `Object::String`. See module docs.
            match std::str::from_utf8(s.as_bytes()) {
                Ok(valid) => Value::String(Utf8String::from(valid)),
                Err(_) => Value::Binary(s.as_bytes().to_vec()),
            }
        }
        Object::Array(items) => Value::Array(items.iter().map(value_from_object).collect()),
        Object::Dict(d) => Value::Map(
            d.iter()
                .map(|(k, v)| (value_from_object(&Object::String(k.clone())), value_from_object(v)))
                .collect(),
        ),
        Object::LuaRef(r) => Value::String(Utf8String::from(format!("<Lua {r}>"))),
        Object::Buffer(h) => Value::Ext(EXT_TYPE_BUFFER, handle_payload(i64::from(*h))),
        Object::Window(h) => Value::Ext(EXT_TYPE_WINDOW, handle_payload(i64::from(*h))),
        Object::Tabpage(h) => Value::Ext(EXT_TYPE_TABPAGE, handle_payload(i64::from(*h))),
    }
}

/// Encode a handle as the uint payload bytes upstream `packer.c mpack_handle()`
/// writes: a single byte for `0..=0x7f` (fixext1), otherwise `mpack_uint`.
fn handle_payload(handle: i64) -> Vec<u8> {
    // `handle` is always non-negative by construction: it comes from an
    // `i64::from(&handle)` of a handle type that rejects negatives.
    if handle <= 0x7f {
        vec![handle as u8]
    } else {
        let h = handle as u32;
        if h <= 0xff {
            vec![0xcc, h as u8]
        } else if h <= 0xffff {
            vec![0xcd, (h >> 8) as u8, h as u8]
        } else {
            vec![0xce, (h >> 24) as u8, (h >> 16) as u8, (h >> 8) as u8, h as u8]
        }
    }
}

/// Convert a decoded `rmpv::Value` into an [`Object`].
pub fn object_from_value(value: Value) -> Result<Object, DecodeError> {
    match value {
        Value::Nil => Ok(Object::Nil),
        Value::Boolean(b) => Ok(Object::Boolean(b)),
        Value::Integer(i) => match i.as_i64() {
            Some(n) => Ok(Object::Integer(n)),
            None => Err(DecodeError::IntegerOutOfRange(i.as_u64().unwrap_or(u64::MAX))),
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
                        )))
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
                EXT_TYPE_BUFFER => BufHandle::try_from(handle).map(Object::Buffer).map_err(DecodeError::Handle),
                EXT_TYPE_WINDOW => WinHandle::try_from(handle).map(Object::Window).map_err(DecodeError::Handle),
                EXT_TYPE_TABPAGE => TabHandle::try_from(handle).map(Object::Tabpage).map_err(DecodeError::Handle),
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
        [0xce, a, b, c, d] => Some(i64::from(u32::from_be_bytes([a, b, c, d]))),
        [0xcf, a, b, c, d, e, f, g, h] => {
            i64::try_from(u64::from_be_bytes([a, b, c, d, e, f, g, h])).ok()
        }
        _ => None,
    }
}

/// Encode a single [`Object`] to its msgpack byte representation.
///
/// `rmpv::encode::write_value` into an in-memory `Vec<u8>` cannot fail: `Vec`'s
/// `io::Write` is infallible and the depth limit applies to decoding only, so
/// the `Result` is discarded and the buffer is returned regardless.
pub fn encode(obj: &Object) -> Vec<u8> {
    let value = value_from_object(obj);
    let mut out = Vec::new();
    let _ = rmpv::encode::write_value(&mut out, &value);
    out
}

/// Decode exactly one [`Object`] from the front of `bytes`.
///
/// Trailing bytes are left undecoded (callers feeding a stream should use
/// [`IncrementalDecoder`]); this is a convenience for known-single-object
/// payloads and tests.
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
    limit: usize,
}

impl IncrementalDecoder {
    /// A decoder with the default [`DEFAULT_DECODE_LIMIT`] staging cap.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new(), limit: DEFAULT_DECODE_LIMIT }
    }

    /// A decoder with a custom staging cap.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self { buf: Vec::new(), limit }
    }

    /// Feed a byte slice, decoding any complete messages it completes.
    ///
    /// On error the internal buffer is cleared so the decoder can be reused.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Message>, DecodeError> {
        if !bytes.is_empty() {
            self.buf.extend_from_slice(bytes);
        }
        let mut out = Vec::new();
        let result = (|| {
            loop {
                if self.buf.is_empty() {
                    break;
                }
                let mut cursor = Cursor::new(&self.buf[..]);
                match rmpv::decode::read_value_with_max_depth(&mut cursor, MAX_MESSAGE_DEPTH) {
                    Ok(value) => {
                        let consumed = cursor.position() as usize;
                        self.buf.drain(..consumed);
                        out.push(Message::from_value(value)?);
                    }
                    Err(e) => match e.kind() {
                        // End of input while reading: valid prefix, wait for more.
                        ErrorKind::UnexpectedEof => {
                            if self.buf.len() > self.limit {
                                return Err(DecodeError::Oversized { limit: self.limit });
                            }
                            break;
                        }
                        _ => return Err(DecodeError::Malformed(e.to_string())),
                    },
                }
            }
            let mut done = Vec::new();
            std::mem::swap(&mut done, &mut out);
            Ok(done)
        })();
        if result.is_err() {
            self.buf.clear();
        }
        result
    }

    /// Whether no undecoded bytes are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Number of undecoded bytes currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
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
        assert_eq!(decode(&encode(&obj)).unwrap(), obj);
    }

    #[test]
    fn luaref_and_negative_int_encode() {
        // "<Lua 42>" is 8 chars -> fixstr 0xa8 (executor.c nlua_funcref_str).
        assert_eq!(encode(&Object::LuaRef(42)), b"\xa8<Lua 42>");
        assert_eq!(encode(&Object::Integer(-1)), &[0xff]);
        assert_eq!(encode(&Object::Integer(-32)), &[0xe0]);
        assert_eq!(encode(&Object::Integer(-33)), &[0xd0, 0xdf]);
        // Integer 128 uses uint8 0xcc, mirroring upstream mpack_uint.
        assert_eq!(encode(&Object::Integer(128)), &[0xcc, 0x80]);
    }

    #[test]
    fn handle_ext_round_trip() {
        for h in [0, 1, 0x7f, 0x80, 0xffff, 0x10000, i32::MAX - 1] {
            let handle = BufHandle::try_from(i64::from(h)).unwrap();
            let decoded = decode(&encode(&Object::Buffer(handle))).unwrap();
            assert_eq!(decoded, Object::Buffer(handle), "handle {h}");
        }
    }

    #[test]
    fn small_handle_is_fixext1() {
        assert_eq!(
            encode(&Object::Buffer(BufHandle::try_from(7).unwrap())),
            &[0xd4, 0, 7]
        );
    }

    #[test]
    fn non_utf8_string_round_trips_via_bin() {
        let raw = OxStr::from(&[0xff, b'x', 0x00][..]);
        let encoded = encode(&Object::String(raw.clone()));
        assert_eq!(decode(&encoded).unwrap(), Object::String(raw));
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
        let frames: Vec<u8> =
            [m1.encode_bytes(), m2.encode_bytes()].concat();
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
            msgid: counter.next(),
            method: OxStr::from("a"),
            params: vec![],
        };
        let b = Message::Request {
            msgid: counter.next(),
            method: OxStr::from("b"),
            params: vec![],
        };
        let c = Message::Request {
            msgid: counter.next(),
            method: OxStr::from("c"),
            params: vec![],
        };
        let blob = [a.encode_bytes(), b.encode_bytes(), c.encode_bytes()].concat();
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
        let err = dec2.feed(&nested).unwrap_err();
        assert!(matches!(err, DecodeError::Malformed(_)), "{err:?}");
        let _ = nested.pop();

        // Decoder is still usable afterwards.
        let ok = Message::Notification { method: OxStr::from("t"), params: vec![] };
        assert_eq!(dec.feed(&ok.encode_bytes()).unwrap(), vec![ok]);
    }

    #[test]
    fn garbage_prefix_is_incomplete_then_never_resolves() {
        // A str32 header declaring many bytes followed by just a few payload
        // bytes is a valid prefix -> incomplete, capped by the limit -> typed
        // Oversized error.
        let mut dec = IncrementalDecoder::with_limit(10);
        let mut input = vec![0xdb, 0x00, 0x00, 0x01, 0x2c]; // str32 len 300
        input.extend(std::iter::repeat(b'a').take(11));
        let err = dec.feed(&input).unwrap_err();
        assert!(matches!(err, DecodeError::Oversized { limit: 10 }), "{err:?}");
        assert!(dec.is_empty());
    }

    #[test]
    fn one_meg_frame() {
        // A Notification whose params array holds ~1.2 MiB of integers.
        let big = Message::Notification {
            method: OxStr::from("big"),
            params: vec![Object::Array(vec![Object::Integer(0); 1_200_000])],
        };
        let encoded = big.encode_bytes();
        assert!(encoded.len() > 1024 * 1024);
        let mut dec = IncrementalDecoder::new();
        let msgs = dec.feed(&encoded).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], Message::Notification { .. }));
        assert!(dec.is_empty());
    }
}
//! The three msgpack-RPC message kinds and request-id allocation.
//!
//! Upstream wire shapes (`src/nvim/msgpack_rpc/channel.c`):
//!
//!   - `serialize_request` → `[0, msgid, method, args]` (request) or
//!     `[2, method, args]` (notification), with `msgid` packed via `mpack_uint`.
//!   - `serialize_response` → `[1, msgid, error, result]` where `error` is
//!     `NIL` on success or `[type, message]` on failure — the `[type, message]`
//!     pair is exactly the 2-element array packed with `mpack_integer(err->type)`
//!     and `mpack_str(err->msg)` (type `0` = exception, `1` = validation,
//!     per `api/private/defs.h` `kErrorType*`).

use ox_types::{ApiError, Object, OxStr};
use rmpv::Value;

use crate::codec::{object_from_value, DecodeError};

/// A single msgpack-RPC message (`kMessageType*` in `api/private/defs.h`).
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// `[0, msgid, method, args]` — a call expecting a response.
    Request {
        /// Remote request id (`next_request_id` in `channel.c`).
        msgid: u32,
        /// Method (=`handler.name`) to invoke.
        method: OxStr,
        /// Call arguments; must be an array on the wire.
        params: Vec<Object>,
    },
    /// `[1, msgid, error|[type,msg], result]` — the reply to a request.
    Response {
        /// The `msgid` being answered.
        msgid: u32,
        /// `Ok(result)` on success (packed error=NIL); `Err(api_error)` packs
        /// the `[type, message]` array.
        result: Result<Object, ApiError>,
    },
    /// `[2, method, args]` — a one-way notice.
    Notification {
        /// Method (=`handler.name`) to invoke.
        method: OxStr,
        /// Notice arguments; must be an array on the wire.
        params: Vec<Object>,
    },
}

impl Message {
    /// Encode the message to its exact wire bytes.
    pub fn encode_bytes(&self) -> Vec<u8> {
        let frame = match self {
            Message::Request { msgid, method, params } => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(i64::from(*msgid)),
                Object::String(method.clone()),
                Object::Array(params.clone()),
            ]),
            Message::Notification { method, params } => Object::Array(vec![
                Object::Integer(2),
                Object::String(method.clone()),
                Object::Array(params.clone()),
            ]),
            Message::Response { msgid, result } => match result {
                Ok(res) => Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(i64::from(*msgid)),
                    Object::Nil,
                    res.clone(),
                ]),
                Err(err) => Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(i64::from(*msgid)),
                    Object::Array(vec![
                        Object::Integer(err.error_type()),
                        Object::String(OxStr::from(err.message())),
                    ]),
                    Object::Nil,
                ]),
            },
        };
        crate::codec::encode(&frame)
    }

    /// Decode a complete message frame from an `rmpv::Value`.
    ///
    /// Upstream does the header/shape validation in `unpacker.c`'s FSM; here a
    /// value that is not `[kind, ...]` with the right arity is a [`DecodeError::Message`].
    pub fn from_value(value: Value) -> Result<Message, DecodeError> {
        let Value::Array(mut items) = value else {
            return Err(DecodeError::Message("message must be an array".into()));
        };
        if items.is_empty() {
            return Err(DecodeError::Message("empty message array".into()));
        }
        let Some(kind) = int_val(items.remove(0)) else {
            return Err(DecodeError::Message("message kind must be an integer".into()));
        };
        match kind {
            0 => {
                if items.len() != 3 {
                    return Err(DecodeError::Message(format!(
                        "request must have 4 elements, got {}",
                        items.len() + 1
                    )));
                }
                let msgid = msgid(items.remove(0))?;
                let method = ox_str(items.remove(0))?;
                let params = params(items.remove(0))?;
                Ok(Message::Request { msgid, method, params })
            }
            1 => {
                if items.len() != 3 {
                    return Err(DecodeError::Message(format!(
                        "response must have 4 elements, got {}",
                        items.len() + 1
                    )));
                }
                let msgid = msgid(items.remove(0))?;
                let err = items.remove(0);
                let result = items.remove(0);
                match err {
                    Value::Nil => Ok(Message::Response { msgid, result: Ok(object_from_value(result)?) }),
                    Value::Array(mut pair) => {
                        if pair.len() != 2 {
                            return Err(DecodeError::Message(
                                "response error must be [type, message]".into(),
                            ));
                        }
                        let err_type = match int_val(pair.remove(0)) {
                            Some(t) => t,
                            None => {
                                return Err(DecodeError::Message(
                                    "response error type must be an integer".into(),
                                ))
                            }
                        };
                        let msg = match ox_str(pair.remove(0)) {
                            Ok(m) => m,
                            Err(_) => {
                                return Err(DecodeError::Message(
                                    "response error message must be a string".into(),
                                ))
                            }
                        };
                        let api_error = match err_type {
                            0 => ApiError::Exception(msg.to_string_lossy().into_owned()),
                            1 => ApiError::Validation(msg.to_string_lossy().into_owned()),
                            other => {
                                return Err(DecodeError::Message(format!(
                                    "unknown error type code {other}"
                                )))
                            }
                        };
                        Ok(Message::Response { msgid, result: Err(api_error) })
                    }
                    _ => Err(DecodeError::Message("response error must be nil or [type, message]".into())),
                }
            }
            2 => {
                if items.len() != 2 {
                    return Err(DecodeError::Message(format!(
                        "notification must have 3 elements, got {}",
                        items.len() + 1
                    )));
                }
                let method = ox_str(items.remove(0))?;
                let params = params(items.remove(0))?;
                Ok(Message::Notification { method, params })
            }
            other => Err(DecodeError::Message(format!(
                "unknown message kind {other} (expected 0, 1 or 2)"
            ))),
        }
    }
}

/// A monotonically increasing request-id source.
///
/// Upstream seeds `rpc->next_request_id = 1` in `rpc_start()` (`channel.c`) and
/// post-increments per call, so ids start at 1. After the `u32` counter wraps it
/// skips 0 (the brief's requirement; `msgid 0` is reserved for broadcast/`rpc_send_event`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MsgidCounter {
    next: u32,
}

impl MsgidCounter {
    /// A counter whose first issued id is `1` (upstream `next_request_id = 1`).
    #[must_use]
    pub fn new() -> Self {
        Self { next: 1 }
    }

    /// Issue the next request id, skipping `0` across the `u32` wraparound.
    #[must_use]
    pub fn next(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        if self.next == 0 {
            self.next = 1;
        }
        id
    }
}

/// Extract an `i64` from an integer `Value`, or else a message-shape error.
fn int_val(v: Value) -> Option<i64> {
    match v {
        Value::Integer(i) => i.as_i64(),
        _ => None,
    }
}

fn msgid(v: Value) -> Result<u32, DecodeError> {
    let Some(n) = int_val(v) else {
        return Err(DecodeError::Message("msgid must be an integer".into()));
    };
    u32::try_from(n).map_err(|_| DecodeError::Message(format!("msgid {n} out of u32 range")))
}

fn ox_str(v: Value) -> Result<OxStr, DecodeError> {
    match v {
        Value::String(s) => Ok(OxStr(s.into_bytes())),
        Value::Binary(b) => Ok(OxStr(b)),
        _ => Err(DecodeError::Message("expected a string".into())),
    }
}

fn params(v: Value) -> Result<Vec<Object>, DecodeError> {
    let Value::Array(items) = v else {
        return Err(DecodeError::Message("message args must be an array".into()));
    };
    items.into_iter().map(object_from_value).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::codec::IncrementalDecoder;
    use ox_types::BufHandle;

    // Round-trip a message's wire bytes back through the decoder.
    fn roundtrip(m: &Message) -> Message {
        let mut dec = IncrementalDecoder::new();
        let got = dec.feed(&m.encode_bytes()).unwrap();
        assert_eq!(got.len(), 1);
        got.into_iter().next().unwrap()
    }

    // Expected bytes are hand-derived from the msgpack spec and the exact
    // upstream packer (`channel.c serialize_request/serialize_response`).
    #[test]
    fn request_wire_shape() {
        let m = Message::Request {
            msgid: 5,
            method: OxStr::from("nvim_buf_line_count"),
            params: vec![Object::Buffer(BufHandle::try_from(1).unwrap())],
        };
        // [0, 5, "nvim_buf_line_count", [0xd4 0 1]]
        // "nvim_buf_line_count" is 19 bytes -> fixstr 0xb3.
        let expected: &[u8] = &[
            0x94, // array(4)
            0x00, // 0 (request)
            0x05, // msgid 5
            0xb3, // fixstr(19) "nvim_buf_line_count"
            b'n', b'v', b'i', b'm', b'_', b'b', b'u', b'f', b'_', b'l', b'i', b'n', b'e', b'_',
            b'c', b'o', b'u', b'n', b't',
            0x91,       // array(1)
            0xd4, 0, 1, // fixext1 type 0 (Buffer), value 1
        ];
        assert_eq!(m.encode_bytes(), expected);
        assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn response_success_shape() {
        let m = Message::Response { msgid: 3, result: Ok(Object::Integer(42)) };
        // [1, 3, nil, 42]
        let expected: &[u8] = &[0x94, 0x01, 0x03, 0xc0, 0x2a];
        assert_eq!(m.encode_bytes(), expected);
        assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn response_error_shape() {
        let m = Message::Response {
            msgid: 7,
            result: Err(ApiError::Validation("bad arg".into())),
        };
        // [1, 7, [1, "bad arg"], nil]
        let expected: &[u8] = &[
            0x94,       // array(4)
            0x01,       // 1 (response)
            0x07,       // msgid 7
            0x92,       // array(2): error
            0x01,       //   type 1 = validation
            0xa7, b'b', b'a', b'd', b' ', b'a', b'r', b'g', // "bad arg"
            0xc0, // nil result
        ];
        assert_eq!(m.encode_bytes(), expected);
        assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn notification_wire_shape() {
        let m = Message::Notification { method: OxStr::from("nvim_echo"), params: vec![] };
        // [2, "nvim_echo", []]
        let expected: &[u8] = &[
            0x93, // array(3)
            0x02, // 2 (notification)
            0xa9, b'n', b'v', b'i', b'm', b'_', b'e', b'c', b'h', b'o', // fixstr(9)
            0x90, // array(0)
        ];
        assert_eq!(m.encode_bytes(), expected);
        assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn request_decode_rejects_bad_shape() {
        // A request frame with only 3 elements (missing the args array) is
        // rejected with a typed message-shape error.
        let value = rmpv::Value::Array(vec![
            rmpv::Value::Integer(rmpv::Integer::from(0)),
            rmpv::Value::Integer(rmpv::Integer::from(1)),
            rmpv::Value::String(rmpv::Utf8String::from("x")),
        ]);
        let err = Message::from_value(value).unwrap_err();
        assert!(matches!(err, DecodeError::Message(_)), "{err:?}");

        // Unknown kind is rejected too.
        let value = rmpv::Value::Array(vec![rmpv::Value::Integer(rmpv::Integer::from(9))]);
        let err = Message::from_value(value).unwrap_err();
        assert!(matches!(err, DecodeError::Message(_)), "{err:?}");
    }

    #[test]
    fn msgid_counter_starts_at_one_and_wraps() {
        let mut c = MsgidCounter::new();
        assert_eq!(c.next(), 1);
        assert_eq!(c.next(), 2);
        // Force wraparound: skip to u32::MAX then confirm 0 is skipped.
        c.next = u32::MAX;
        assert_eq!(c.next(), u32::MAX);
        assert_eq!(c.next(), 1);
    }
}

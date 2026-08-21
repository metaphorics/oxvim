//! `OxStr`: a Vimscript byte string.

use std::borrow::Cow;
use std::fmt;

/// `Formatter::write_char` lives on the `fmt::Write` trait.
use std::fmt::Write as _;

/// A Vimscript string.
///
/// Vim strings are byte sequences and are not guaranteed to be valid UTF-8
/// (see upstream `String` in `api/private/defs.h` and `char_u *` handling).
/// This type deliberately stores raw bytes; decoding to text is a separate,
/// explicit step ([`OxStr::to_string_lossy`]).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OxStr(pub Vec<u8>);

impl OxStr {
    /// Borrow the underlying bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// Decode as lossy UTF-8, replacing any invalid sequences with U+FFFD.
    ///
    /// This is the display helper: Vim renders invalid bytes lossily and this
    /// mirrors that for client-facing output.
    #[must_use]
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    fn fmt_escaped(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            match b {
                b'\\' => f.write_str("\\\\")?,
                b'"' => f.write_str("\\\"")?,
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0x20..=0x7e => f.write_char(char::from(b))?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        Ok(())
    }
}

impl fmt::Debug for OxStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OxStr(")?;
        self.fmt_escaped(f)?;
        f.write_char(')')
    }
}

impl From<&str> for OxStr {
    fn from(s: &str) -> Self {
        OxStr(s.as_bytes().to_vec())
    }
}

impl From<&[u8]> for OxStr {
    fn from(b: &[u8]) -> Self {
        OxStr(b.to_vec())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::OxStr;

    #[test]
    fn debug_escapes_bytes() {
        let s = OxStr::from(&[b'a', b'\n', b'b', 0x00, b'c', 0xFF][..]);
        assert_eq!(format!("{s:?}"), "OxStr(a\\nb\\x00c\\xff)");
    }

    #[test]
    fn lossy_decode() {
        assert_eq!(OxStr::from("héllo").to_string_lossy().as_ref(), "héllo");
        assert_eq!(OxStr::from(&[0xFF, b'x'][..]).to_string_lossy().as_ref(), "\u{FFFD}x");
    }

    #[test]
    fn ordering_and_hash() {
        use std::collections::HashSet;

        assert!(OxStr::from("a") < OxStr::from("b"));
        let mut set = HashSet::new();
        set.insert(OxStr::from("k"));
        assert!(set.contains(&OxStr::from("k")));
    }
}

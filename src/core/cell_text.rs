//! Compact per-cell text storage.
//!
//! The terminal grid has one text payload per cell. Almost every cell contains
//! either nothing, a single ASCII byte, or one Unicode scalar value; allocating
//! a `String` for each of those is the dominant allocation source in the
//! structured pane path. `CellText` keeps those common cases inline and falls
//! back to a compact heap representation only for multi-codepoint grapheme
//! clusters.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use compact_str::CompactString;
use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// Text carried by one grid cell.
///
/// Equality and hashing are defined by the visible string, not by the storage
/// representation, so an inline `"a"` and an extended `"a"` compare equal.
#[derive(Clone, Debug)]
pub struct CellText {
    inline: [u8; 4],
    inline_len: u8,
    extended: Option<Box<CompactString>>,
}

impl CellText {
    /// Creates text from a single Unicode scalar value without allocating.
    pub fn from_char(ch: char) -> Self {
        let mut inline = [0_u8; 4];
        let inline_len = ch.encode_utf8(&mut inline).len() as u8;
        Self {
            inline,
            inline_len,
            extended: None,
        }
    }

    /// Creates text from a string slice, keeping one scalar value inline.
    pub fn from_str(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        let mut chars = text.chars();
        let first = chars.next().expect("non-empty text");
        if chars.next().is_none() {
            return Self::from_char(first);
        }
        Self {
            inline: [0; 4],
            inline_len: 0,
            extended: Some(Box::new(CompactString::new(text))),
        }
    }

    /// Returns the text as a string slice.
    pub fn as_str(&self) -> &str {
        if let Some(text) = &self.extended {
            return text.as_str();
        }
        std::str::from_utf8(&self.inline[..usize::from(self.inline_len)])
            .expect("inline cell text is valid utf-8")
    }

    /// Returns the UTF-8 bytes of this cell's text.
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }

    /// Whether this cell carries no text.
    pub fn is_empty(&self) -> bool {
        self.extended.is_none() && self.inline_len == 0
    }

    /// Number of bytes in the UTF-8 representation.
    pub fn len(&self) -> usize {
        self.as_str().len()
    }
}

impl Default for CellText {
    fn default() -> Self {
        Self {
            inline: [0; 4],
            inline_len: 0,
            extended: None,
        }
    }
}

impl Deref for CellText {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for CellText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CellText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for CellText {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for CellText {}

impl Hash for CellText {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

macro_rules! impl_eq_str {
    ($rhs:ty) => {
        impl PartialEq<$rhs> for CellText {
            fn eq(&self, other: &$rhs) -> bool {
                self.as_str() == <$rhs as AsRef<str>>::as_ref(other)
            }
        }

        impl PartialEq<CellText> for $rhs {
            fn eq(&self, other: &CellText) -> bool {
                <$rhs as AsRef<str>>::as_ref(self) == other.as_str()
            }
        }
    };
}

impl_eq_str!(str);
impl_eq_str!(&str);
impl_eq_str!(String);

impl From<char> for CellText {
    fn from(value: char) -> Self {
        Self::from_char(value)
    }
}

impl From<&str> for CellText {
    fn from(value: &str) -> Self {
        Self::from_str(value)
    }
}

impl From<String> for CellText {
    fn from(value: String) -> Self {
        Self::from_str(&value)
    }
}

impl From<&String> for CellText {
    fn from(value: &String) -> Self {
        Self::from_str(value)
    }
}

impl Serialize for CellText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CellText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = CompactString::deserialize(deserializer)?;
        Ok(Self::from_str(text.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::CellText;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash(text: &CellText) -> u64 {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn common_cells_stay_inline() {
        assert!(CellText::from_char('a').extended.is_none());
        assert!(CellText::from_char('漢').extended.is_none());
        assert!(CellText::from_str("").extended.is_none());
        assert!(CellText::from_str("e\u{301}").extended.is_some());
    }

    #[test]
    fn storage_representation_does_not_change_equality_or_hash() {
        let inline = CellText::from_char('a');
        let extended = CellText {
            inline: [0; 4],
            inline_len: 0,
            extended: Some(Box::new("a".into())),
        };
        assert_eq!(inline, extended);
        assert_eq!(hash(&inline), hash(&extended));
    }

    #[test]
    fn string_conversions_round_trip() {
        let value: CellText = "he\u{301}llo".into();
        assert_eq!(value.as_str(), "he\u{301}llo");
        assert_eq!(value, String::from("he\u{301}llo"));
    }
}

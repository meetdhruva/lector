use std::fmt;

/// A git object id (SHA-1, 20 bytes).
///
/// Thin, `Copy` wrapper around [`gix::ObjectId`] so that callers don't need to
/// depend on `gix`. `Display` prints the full lowercase hex form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(gix::ObjectId);

impl Oid {
    /// Parse a full hexadecimal object id.
    pub fn from_hex(hex: &str) -> Option<Oid> {
        gix::ObjectId::from_hex(hex.trim().as_bytes()).ok().map(Oid)
    }

    /// Build from raw bytes (20 bytes for SHA-1).
    pub fn from_bytes(bytes: &[u8]) -> Option<Oid> {
        gix::ObjectId::try_from(bytes).ok().map(Oid)
    }

    /// The raw bytes of the id.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// The abbreviated 7-character hex form used in the UI.
    pub fn short(&self) -> String {
        let mut hex = self.0.to_string();
        hex.truncate(7);
        hex
    }

    /// `true` for the all-zero id that git uses for "not committed yet".
    pub fn is_zero(&self) -> bool {
        self.0.is_null()
    }

    /// The underlying gix id.
    pub fn to_gix(self) -> gix::ObjectId {
        self.0
    }
}

impl From<gix::ObjectId> for Oid {
    fn from(id: gix::ObjectId) -> Self {
        Oid(id)
    }
}

impl From<&gix::oid> for Oid {
    fn from(id: &gix::oid) -> Self {
        Oid(id.to_owned())
    }
}

impl From<gix::Id<'_>> for Oid {
    fn from(id: gix::Id<'_>) -> Self {
        Oid(id.detach())
    }
}

impl From<Oid> for gix::ObjectId {
    fn from(id: Oid) -> Self {
        id.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.0)
    }
}

impl std::str::FromStr for Oid {
    type Err = crate::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Oid::from_hex(s).ok_or_else(|| crate::Error::Parse(format!("invalid object id '{s}'")))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Oid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Oid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Oid::from_hex(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid object id '{s}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_short() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let oid = Oid::from_hex(hex).unwrap();
        assert_eq!(oid.to_string(), hex);
        assert_eq!(oid.short(), "0123456");
        assert!(!oid.is_zero());
        assert!(Oid::from_hex("0000000000000000000000000000000000000000")
            .unwrap()
            .is_zero());
        assert!(Oid::from_hex("xyz").is_none());
        assert_eq!(Oid::from_bytes(oid.as_bytes()), Some(oid));
    }
}

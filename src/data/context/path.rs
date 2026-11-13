//! Portable relative file names, validated before filesystem access.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextRelPath(String);
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextPathError {
    #[error("context path is empty")]
    Empty,
    #[error("context path must be relative")]
    Absolute,
    #[error("context path has an empty segment")]
    EmptySegment,
    #[error("context path contains a dot segment")]
    DotSegment,
    #[error("context path contains a forbidden character")]
    ForbiddenCharacter,
    #[error("context path contains a reserved name")]
    ReservedName,
    #[error("context path is too long")]
    TooLong,
    #[error("context path is too deep")]
    TooDeep,
}
pub(crate) fn reserved_name(name: &str) -> bool {
    let upper = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|n| {
            n.len() == 1 && n.as_bytes()[0].is_ascii_digit() || matches!(n, "¹" | "²" | "³")
        })
    })
}
impl ContextRelPath {
    pub const MAX_BYTES: usize = 1024;
    pub const MAX_SEGMENT_BYTES: usize = 255;
    pub const MAX_DEPTH: usize = 16;
    pub fn parse(raw: &str) -> Result<Self, ContextPathError> {
        use ContextPathError::*;
        if raw.is_empty() {
            return Err(Empty);
        }
        if raw.starts_with('/') {
            return Err(Absolute);
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(TooLong);
        }
        if raw
            .chars()
            .any(|c| c.is_ascii_control() || matches!(c, '\\' | ':'))
        {
            return Err(ForbiddenCharacter);
        }
        if raw.split('/').count() > Self::MAX_DEPTH {
            return Err(TooDeep);
        }
        for segment in raw.split('/') {
            if segment.is_empty() {
                return Err(EmptySegment);
            }
            if matches!(segment, "." | "..") {
                return Err(DotSegment);
            }
            if segment.len() > Self::MAX_SEGMENT_BYTES {
                return Err(TooLong);
            }
            if segment.ends_with(['.', ' ']) || reserved_name(segment) {
                return Err(ReservedName);
            }
        }
        Ok(Self(raw.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
    pub fn fold_key(&self) -> String {
        self.0.to_lowercase()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsafe_paths() {
        for raw in [
            "", "/a", "../a", "a/../b", "./a", "a//b", "a/", "C:/a", "a\\b", "a\0b", "a\nb",
            "CON.txt", "a.", "a ",
        ] {
            assert!(ContextRelPath::parse(raw).is_err(), "{raw:?}");
        }
        assert!(ContextRelPath::parse(&"a/".repeat(17)).is_err());
        assert!(ContextRelPath::parse(&"a".repeat(256)).is_err());
        assert_eq!(
            ContextRelPath::parse("Notes/é.md").unwrap().fold_key(),
            "notes/é.md"
        );
    }
}

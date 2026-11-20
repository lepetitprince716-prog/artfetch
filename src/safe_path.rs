//! The one rule for every name artfetch writes under a directory it was given:
//! the name must be a single plain file-name component.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A validated single file-name component.
///
/// Non-empty, not `.`, free of `..`, of `/` and `\`, of `:` (a drive prefix or
/// an alternate data stream on Windows) and of control characters. Joining it
/// to a directory therefore always names an entry directly inside that
/// directory, whatever the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName(String);

impl FileName {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let problem = if raw.is_empty() {
            Some("is empty")
        } else if raw == "." {
            Some("is '.'")
        } else if raw.contains("..") {
            Some("contains '..'")
        } else if raw.contains(['/', '\\']) {
            Some("contains a path separator")
        } else if raw.contains(':') {
            Some("contains ':'")
        } else if raw.chars().any(char::is_control) {
            Some("contains a control character")
        } else {
            None
        };
        match problem {
            Some(why) => Err(format!(
                "{raw:?} {why}; a single plain file name is required"
            )),
            None => Ok(Self(raw.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `dir/<name>`, refused unless the result sits directly inside `dir`.
    pub fn path_in(&self, dir: &Path) -> Result<PathBuf, String> {
        let joined = dir.join(&self.0);
        if joined.parent() == Some(dir) && joined.file_name() == Some(OsStr::new(&self.0)) {
            Ok(joined)
        } else {
            Err(format!(
                "{:?} does not stay inside {}",
                self.0,
                dir.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileName;
    use std::path::Path;

    #[test]
    fn accepts_plain_names_and_refuses_everything_else() {
        for ok in [
            "img",
            "exa26",
            "twitter_1234567890_0.jpg",
            "a.b",
            "名前_001.jpg",
        ] {
            let name = FileName::parse(ok).unwrap();
            let dir = Path::new("/data/out");
            assert_eq!(name.path_in(dir).unwrap(), dir.join(ok));
        }
        for bad in [
            "", ".", "..", "../x", "x/..", "a..b", "a/b", "/abs", "a\\b", "C:x", "a:b", "a\nb",
            "a\0b",
        ] {
            assert!(FileName::parse(bad).is_err(), "{bad:?} accepted");
        }
    }
}

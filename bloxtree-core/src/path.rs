use crate::error::{Error, Result};

/// Validate a virtual path.
///
/// Paths are `/`-separated logical identifiers (e.g. `"documents/report.txt"`).
/// They must not be empty, must not contain null bytes, and must not have
/// leading or trailing whitespace.
/// Paths are not allowed to start with a `/`.
/// Path are not allowed to end with a `/`.
pub fn validate(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::InvalidPath("path is empty".into()));
    }
    if path.contains('\0') {
        return Err(Error::InvalidPath("path contains null byte".into()));
    }
    if path != path.trim() {
        return Err(Error::InvalidPath(
            "path has leading or trailing whitespace".into(),
        ));
    }
    if path.starts_with('/') {
        return Err(Error::InvalidPath(
            "path is not allowed to start with a slash".into(),
        ));
    }
    if path.ends_with('/') {
        return Err(Error::InvalidPath(
            "path is not allowed to end with a slash".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_paths() {
        assert!(validate("a").is_ok());
        assert!(validate("a/b").is_ok());
        assert!(validate("documents/report.txt").is_ok());
        assert!(validate("a/b/c/d/e").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate("").is_err());
    }

    #[test]
    fn rejects_null_byte() {
        assert!(validate("a\0b").is_err());
    }

    #[test]
    fn rejects_leading_whitespace() {
        assert!(validate(" a").is_err());
    }

    #[test]
    fn rejects_trailing_whitespace() {
        assert!(validate("a ").is_err());
    }
}

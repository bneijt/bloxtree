use std::{fs, io, path::PathBuf};

use blake3::{Hash, Hasher};

use crate::error::Error;

/// A `std::io::Read` wrapper that verifies the BLAKE3 hash of the data as it
/// is read. On EOF, the computed hash is compared to `expected` and an error
/// is returned if they don't match.
pub(crate) struct BloxtreeReader {
    inner: fs::File,
    hasher: Hasher,
    expected: Hash,
    verified: bool,
}

impl BloxtreeReader {
    /// Create a new reader for the object file at `path`, expected to have
    /// the given BLAKE3 hash.
    pub(crate) fn new(path: PathBuf, expected: Hash) -> io::Result<Self> {
        let inner = fs::File::open(path)?;
        Ok(Self {
            inner,
            hasher: Hasher::new(),
            expected,
            verified: false,
        })
    }
}

impl io::Read for BloxtreeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;

        if n == 0 && !self.verified {
            // EOF — verify hash.
            let actual = self.hasher.finalize();
            if actual != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    Error::HashMismatch {
                        expected: self.expected,
                        actual,
                    },
                ));
            }
            self.verified = true;
        } else {
            self.hasher.update(&buf[..n]);
        }

        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn read_matching_hash() {
        let data = b"hello world";
        let expected = blake3::hash(data);

        // Write test file.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        io::Write::write_all(&mut tmp.as_file(), data).unwrap();

        let mut reader = BloxtreeReader::new(tmp.path().to_path_buf(), expected).unwrap();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn read_mismatched_hash() {
        let data = b"hello world";
        let wrong = blake3::hash(b"different");

        let tmp = tempfile::NamedTempFile::new().unwrap();
        io::Write::write_all(&mut tmp.as_file(), data).unwrap();

        let mut reader = BloxtreeReader::new(tmp.path().to_path_buf(), wrong).unwrap();
        let mut buf = Vec::new();
        let err = reader.read_to_end(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}

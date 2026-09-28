use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::paths::hex_sha256;

pub fn file_sha256(path: &Path) -> Result<[u8; 32]> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

pub fn file_sha256_hex(path: &Path) -> Result<String> {
    Ok(hex_sha256(&file_sha256(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_has_known_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, []).unwrap();
        assert_eq!(
            file_sha256_hex(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(file_sha256(&dir.path().join("nope")).is_err());
    }
}

use std::io::Read;
use std::path::Path;

use md5::Md5;
use sha2::{Digest, Sha256, Sha512};
use subtle::ConstantTimeEq;

use maya_core::{ChecksumAlgorithm, ChecksumSpec, MayaError, Result};

const BUFFER_SIZE: usize = 64 * 1024;

/// Parse a `<algorithm>:<hex>` checksum specification string.
///
/// Supports `md5`, `sha256`, `sha512`, and `blake3` (case-insensitive algorithm prefix).
pub fn parse_checksum(raw: &str) -> Result<ChecksumSpec> {
    let (algo_raw, hex_raw) = raw.trim().split_once(':').ok_or_else(|| {
        MayaError::InvalidConfig(format!(
            "invalid checksum `{raw}`: expected `<md5|sha256|sha512|blake3>:<hex>`"
        ))
    })?;

    let algorithm = match algo_raw.trim().to_ascii_lowercase().as_str() {
        "md5" => ChecksumAlgorithm::Md5,
        "sha256" | "sha-256" => ChecksumAlgorithm::Sha256,
        "sha512" | "sha-512" => ChecksumAlgorithm::Sha512,
        "blake3" => ChecksumAlgorithm::Blake3,
        other => {
            return Err(MayaError::InvalidConfig(format!(
                "unsupported checksum algorithm `{other}` (expected `md5`, `sha256`, `sha512`, or `blake3`)"
            )));
        }
    };

    let normalized_bytes = decode_hex_digest(hex_raw, algorithm)?;
    Ok(ChecksumSpec {
        algorithm,
        expected_hex: encode_hex_lower(&normalized_bytes),
    })
}

/// Verify the file at `path` against `spec` using a buffered 64 KiB reader
/// and constant-time digest comparison.
pub async fn verify_file(path: &Path, spec: &ChecksumSpec) -> Result<()> {
    let path = path.to_path_buf();
    let algorithm = spec.algorithm;
    let expected_bytes = decode_hex_digest(&spec.expected_hex, algorithm)?;
    let expected_hex = encode_hex_lower(&expected_bytes);

    let actual_bytes = tokio::task::spawn_blocking(move || compute_digest_sync(&path, algorithm))
        .await
        .map_err(|err| MayaError::Storage(format!("checksum verification task failed: {err}")))??;

    if actual_bytes
        .as_slice()
        .ct_eq(expected_bytes.as_slice())
        .into()
    {
        Ok(())
    } else {
        Err(MayaError::ChecksumMismatch {
            expected: expected_hex,
            actual: encode_hex_lower(&actual_bytes),
        })
    }
}

fn expected_digest_len(algorithm: ChecksumAlgorithm) -> usize {
    match algorithm {
        ChecksumAlgorithm::Md5 => 16,
        ChecksumAlgorithm::Sha256 => 32,
        ChecksumAlgorithm::Blake3 => 32,
        ChecksumAlgorithm::Sha512 => 64,
    }
}

fn compute_digest_sync(path: &Path, algorithm: ChecksumAlgorithm) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; BUFFER_SIZE];

    match algorithm {
        ChecksumAlgorithm::Md5 => {
            let mut hasher = Md5::new();
            loop {
                let read = file.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
            }
            Ok(hasher.finalize().to_vec())
        }
        ChecksumAlgorithm::Sha256 => {
            let mut hasher = Sha256::new();
            loop {
                let read = file.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
            }
            Ok(hasher.finalize().to_vec())
        }
        ChecksumAlgorithm::Sha512 => {
            let mut hasher = Sha512::new();
            loop {
                let read = file.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
            }
            Ok(hasher.finalize().to_vec())
        }
        ChecksumAlgorithm::Blake3 => {
            let mut hasher = blake3::Hasher::new();
            loop {
                let read = file.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
            }
            Ok(hasher.finalize().as_bytes().to_vec())
        }
    }
}

fn decode_hex_digest(raw: &str, algorithm: ChecksumAlgorithm) -> Result<Vec<u8>> {
    let normalized = raw.trim();
    let digest_len = expected_digest_len(algorithm);
    let expected_chars = digest_len * 2;
    if normalized.len() != expected_chars {
        return Err(MayaError::InvalidConfig(format!(
            "invalid {algorithm:?} checksum length: expected {expected_chars} hex characters, got {}",
            normalized.len()
        )));
    }

    let bytes = normalized.as_bytes();
    let mut out = vec![0u8; digest_len];
    for (idx, out_byte) in out.iter_mut().enumerate() {
        let hi_byte = bytes[idx * 2];
        let lo_byte = bytes[idx * 2 + 1];
        let hi = decode_hex_nibble(hi_byte).ok_or_else(|| {
            MayaError::InvalidConfig(format!(
                "invalid hex character '{}' in checksum",
                hi_byte as char
            ))
        })?;
        let lo = decode_hex_nibble(lo_byte).ok_or_else(|| {
            MayaError::InvalidConfig(format!(
                "invalid hex character '{}' in checksum",
                lo_byte as char
            ))
        })?;
        *out_byte = (hi << 4) | lo;
    }

    Ok(out)
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_DIGITS[(b >> 4) as usize] as char);
        out.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "maya-storage-verify-{prefix}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn verifies_sha256_uppercase_and_whitespace() {
        let dir = TempDirGuard::new("sha256-ok");
        let file_path = dir.path.join("sample.bin");
        let payload = vec![0xABu8; 150_000];
        std::fs::write(&file_path, &payload).expect("write payload");

        let expected = encode_hex_lower(&sha2::Sha256::digest(&payload)).to_uppercase();
        let spec = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Sha256,
            expected_hex: format!("  {expected} \n"),
        };

        verify_file(&file_path, &spec)
            .await
            .expect("sha256 verification should succeed");
    }

    #[tokio::test]
    async fn verifies_blake3_ok() {
        let dir = TempDirGuard::new("blake3-ok");
        let file_path = dir.path.join("sample.bin");
        let payload = b"maya blake3 test payload across multiple blocks".repeat(2048);
        std::fs::write(&file_path, &payload).expect("write payload");

        let expected = blake3::hash(&payload).to_hex().to_string();
        let spec = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Blake3,
            expected_hex: expected,
        };

        verify_file(&file_path, &spec)
            .await
            .expect("blake3 verification should succeed");
    }

    #[tokio::test]
    async fn rejects_sha256_and_blake3_mismatch() {
        let dir = TempDirGuard::new("mismatch");
        let file_path = dir.path.join("data.bin");
        std::fs::write(&file_path, b"hello maya").expect("write payload");

        let wrong_hex = "0".repeat(64);
        for algorithm in [ChecksumAlgorithm::Sha256, ChecksumAlgorithm::Blake3] {
            let spec = ChecksumSpec {
                algorithm,
                expected_hex: wrong_hex.clone(),
            };
            let err = verify_file(&file_path, &spec)
                .await
                .expect_err("mismatch must fail");
            match err {
                MayaError::ChecksumMismatch { expected, actual } => {
                    assert_eq!(expected, wrong_hex);
                    assert_ne!(actual, wrong_hex);
                    assert_eq!(actual.len(), 64);
                }
                other => panic!("expected ChecksumMismatch, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn rejects_invalid_hex_with_invalid_config() {
        let dir = TempDirGuard::new("invalid-hex");
        let file_path = dir.path.join("data.bin");
        std::fs::write(&file_path, b"hello").expect("write payload");

        let bad_len = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Sha256,
            expected_hex: "deadbeef".to_string(),
        };
        assert!(matches!(
            verify_file(&file_path, &bad_len).await,
            Err(MayaError::InvalidConfig(_))
        ));

        let bad_char = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Blake3,
            expected_hex: format!("{}zz", "a".repeat(62)),
        };
        assert!(matches!(
            verify_file(&file_path, &bad_char).await,
            Err(MayaError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn verifies_md5_ok_and_rejects_mismatch() {
        let dir = TempDirGuard::new("md5");
        let file_path = dir.path.join("sample.bin");
        let payload = b"maya md5 multi-chunk verification payload".repeat(3000);
        std::fs::write(&file_path, &payload).expect("write payload");

        let expected = encode_hex_lower(&Md5::digest(&payload));
        assert_eq!(expected.len(), 32);
        let spec = parse_checksum(&format!("md5:{expected}")).expect("parse md5 checksum");
        assert_eq!(spec.algorithm, ChecksumAlgorithm::Md5);

        verify_file(&file_path, &spec)
            .await
            .expect("md5 verification should succeed");

        let wrong = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Md5,
            expected_hex: "0".repeat(32),
        };
        let err = verify_file(&file_path, &wrong)
            .await
            .expect_err("md5 mismatch must fail");
        assert!(matches!(err, MayaError::ChecksumMismatch { .. }));
    }

    #[tokio::test]
    async fn verifies_sha512_ok_and_rejects_mismatch() {
        let dir = TempDirGuard::new("sha512");
        let file_path = dir.path.join("sample.bin");
        let payload = b"maya sha512 verification payload".repeat(2500);
        std::fs::write(&file_path, &payload).expect("write payload");

        let expected = encode_hex_lower(&Sha512::digest(&payload));
        assert_eq!(expected.len(), 128);
        let spec = parse_checksum(&format!("SHA512:{}", expected.to_uppercase()))
            .expect("parse sha512 checksum");
        assert_eq!(spec.algorithm, ChecksumAlgorithm::Sha512);

        verify_file(&file_path, &spec)
            .await
            .expect("sha512 verification should succeed");

        let wrong = ChecksumSpec {
            algorithm: ChecksumAlgorithm::Sha512,
            expected_hex: "f".repeat(128),
        };
        let err = verify_file(&file_path, &wrong)
            .await
            .expect_err("sha512 mismatch must fail");
        assert!(matches!(err, MayaError::ChecksumMismatch { .. }));
    }

    #[test]
    fn parse_checksum_rejects_bad_format_and_unknown_algo() {
        assert!(matches!(
            parse_checksum("deadbeef"),
            Err(MayaError::InvalidConfig(_))
        ));
        assert!(matches!(
            parse_checksum("crc32:01020304"),
            Err(MayaError::InvalidConfig(_))
        ));
        assert!(matches!(
            parse_checksum("md5:1234"),
            Err(MayaError::InvalidConfig(_))
        ));
    }
}

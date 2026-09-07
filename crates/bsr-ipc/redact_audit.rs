// SPDX-License-Identifier: MIT
// bsr-ipc — Redaction Audit Log
//
// Seed-BSR-G3-02-11: When the sensitive-window-title transform masks an event,
// this module writes a one-line JSONL entry that contains ONLY the SHA-256
// hash of the original value — never the plaintext.
//
// Seed-BSR-G3-03-11: The file is rotated (renamed to .1, fresh file started)
// when it grows beyond `max_bytes`.

use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Write a single redaction audit entry.
///
/// # Arguments
/// * `original` — the unredacted value being masked (its *hash* is logged,
///   not the value itself).
/// * `rule` — identifier for the redaction rule that triggered (e.g.
///   `"window_title"`).
/// * `audit_path` — path to the JSONL audit log file.
/// * `max_bytes` — rotate the file once it exceeds this size (bytes).
///
/// All I/O errors are returned to the caller; the caller should decide
/// whether to log or ignore them — this function must not panic.
pub fn write_redaction_event(
    original: &str,
    rule: &str,
    audit_path: &str,
    max_bytes: u64,
) -> io::Result<()> {
    // Seed-BSR-G3-02-11: hash the original; never persist the plaintext.
    let hash = sha256_hex(original);
    let ts = Utc::now().to_rfc3339();

    let entry = format!(
        "{{\"ts\":\"{ts}\",\"original_hash\":\"sha256:{hash}\",\"rule\":\"{rule}\"}}\n",
        ts = ts,
        hash = hash,
        rule = rule,
    );

    let path = Path::new(audit_path);

    // Seed-BSR-G3-03-11: rotate if the file is at or beyond `max_bytes`.
    maybe_rotate(path, max_bytes)?;

    // Ensure parent directories exist.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    file.write_all(entry.as_bytes())?;
    Ok(())
}

/// Rename `path` → `path.1` (overwriting any existing `.1`) when the file
/// size meets or exceeds `max_bytes`.
fn maybe_rotate(path: &Path, max_bytes: u64) -> io::Result<()> {
    match path.metadata() {
        Ok(meta) if meta.len() >= max_bytes => {
            let rotated = path.with_extension("jsonl.1");
            fs::rename(path, &rotated)?;
            Ok(())
        }
        Ok(_) => Ok(()), // below threshold
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()), // first write
        Err(e) => Err(e),
    }
}

/// Compute the lower-hex SHA-256 digest of `input`.
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in result {
        write!(&mut hex, "{:02x}", byte).expect("write to String never fails");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn sha256_hex_known_value() {
        // echo -n "abc" | sha256sum  →  ba7816bf…
        let h = sha256_hex("abc");
        assert_eq!(
            &h[..8],
            "ba7816bf",
            "SHA-256(\"abc\") prefix mismatch: got {}",
            h
        );
    }

    #[test]
    fn write_creates_jsonl_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("redaction_audit.jsonl");
        let path_str = path.to_str().unwrap();

        write_redaction_event("SensitiveTitle", "window_title", path_str, 10_485_760).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("sha256:"), "hash missing from entry");
        assert!(contents.contains("\"rule\":\"window_title\""), "rule missing");
        assert!(!contents.contains("SensitiveTitle"), "plaintext must not be stored");
    }

    #[test]
    fn rotation_occurs_when_size_met() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("redaction_audit.jsonl");
        let path_str = path.to_str().unwrap();
        let max: u64 = 500;

        // Write enough to exceed max_bytes.
        for _ in 0..10 {
            write_redaction_event("SomeValue", "window_title", path_str, max).unwrap();
        }

        // After rotation the .1 file should exist.
        let rotated = dir.path().join("redaction_audit.jsonl.1");
        assert!(
            rotated.exists(),
            "expected rotation file .1 to exist after size threshold"
        );
        // Fresh log is small (only entries written since last rotation).
        let fresh_size = path.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(fresh_size < max, "fresh log should be below threshold");
    }
}

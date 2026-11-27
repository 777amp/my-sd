// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Native Rust attachment storage & streaming AES-256-CBC decryption service.
//!
//! Replaces `app/attachments.node.ts` and `app/attachment_channel.main.ts`.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::info;
use walkdir::WalkDir;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

pub const ATTACHMENTS_DIR: &str = "attachments.noindex";
pub const AVATARS_DIR: &str = "avatars.noindex";
pub const BADGES_DIR: &str = "badges.noindex";
pub const STICKERS_DIR: &str = "stickers.noindex";
pub const TEMP_DIR: &str = "temp";
pub const DRAFTS_DIR: &str = "drafts.noindex";
pub const DOWNLOADS_DIR: &str = "downloads.noindex";
pub const MEGAPHONES_DIR: &str = "megaphones.noindex";

const STALE_DOWNLOAD_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub struct AttachmentStore {
    user_data_dir: PathBuf,
}

impl AttachmentStore {
    pub fn new(user_data_dir: PathBuf) -> Self {
        Self { user_data_dir }
    }

    pub fn dir_path(&self, subdir: &str) -> PathBuf {
        self.user_data_dir.join(subdir)
    }

    /// Safely resolves `relative_path` inside `subdir`, rejecting path traversal (`..`).
    pub fn resolve_safe_path(&self, subdir: &str, relative_path: &str) -> Result<PathBuf> {
        let root = self.dir_path(subdir);
        let rel = Path::new(relative_path);

        if rel.is_absolute() {
            return Err(anyhow!("Absolute attachment path rejected: {relative_path}"));
        }

        let mut normalized = root.clone();
        for comp in rel.components() {
            match comp {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(anyhow!(
                        "Path traversal rejected in attachment path: {relative_path}"
                    ));
                }
            }
        }

        if !normalized.starts_with(&root) {
            return Err(anyhow!("Resolved path escapes root directory"));
        }

        Ok(normalized)
    }

    /// Deletes all files under a specific Signal storage subdirectory.
    pub fn erase_subdir(&self, subdir: &str) -> Result<usize> {
        let dir = self.dir_path(subdir);
        if !dir.exists() {
            return Ok(0);
        }
        let mut removed = 0usize;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
            removed += 1;
        }
        info!("Erased {removed} entries from {}", dir.display());
        Ok(removed)
    }

    /// Removes stale files older than 24 hours from `downloads.noindex`.
    pub fn cleanup_stale_downloads(&self) -> Result<usize> {
        let dir = self.dir_path(DOWNLOADS_DIR);
        if !dir.exists() {
            return Ok(0);
        }

        let now = SystemTime::now();
        let mut removed = 0usize;
        for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if now.duration_since(modified).unwrap_or_default() > STALE_DOWNLOAD_AGE {
                        if fs::remove_file(entry.path()).is_ok() {
                            removed += 1;
                        }
                    }
                }
            }
        }
        info!("Cleaned up {removed} stale downloads");
        Ok(removed)
    }

    /// Deletes files in `attachments.noindex` not present in `known_relative_paths`.
    pub fn cleanup_orphaned_attachments(
        &self,
        known_relative_paths: &HashSet<String>,
    ) -> Result<usize> {
        let dir = self.dir_path(ATTACHMENTS_DIR);
        if !dir.exists() {
            return Ok(0);
        }

        let mut removed = 0usize;
        for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(&dir) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if !known_relative_paths.contains(&rel_str) {
                    if fs::remove_file(entry.path()).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        info!("Cleaned up {removed} orphaned attachments");
        Ok(removed)
    }

    /// Reads a plaintext or encrypted attachment from disk and returns `(bytes, mime_type)`.
    pub fn read_attachment(
        &self,
        subdir: &str,
        relative_path: &str,
        keys_base64: Option<&str>,
    ) -> Result<(Vec<u8>, String)> {
        let path = self.resolve_safe_path(subdir, relative_path)?;
        let raw = fs::read(&path)
            .with_context(|| format!("Failed to read attachment at {}", path.display()))?;

        let bytes = match keys_base64 {
            Some(k) if !k.is_empty() => decrypt_signal_attachment(&raw, k)?,
            _ => raw,
        };

        let mime = mime_guess::from_path(&path)
            .first_or_octet_stream()
            .essence_str()
            .to_string();

        Ok((bytes, mime))
    }
}

/// Decrypts a Signal Desktop local/CDN attachment using Signal's 64-byte key format:
/// - `keys[0..32]` = AES-256-CBC key
/// - `keys[32..64]` = HMAC-SHA256 key
/// - `data` = `[16-byte IV || AES-256-CBC(PKCS7) ciphertext || 32-byte HMAC-SHA256]`
pub fn decrypt_signal_attachment(data: &[u8], keys_base64: &str) -> Result<Vec<u8>> {
    let keys = base64::engine::general_purpose::STANDARD
        .decode(keys_base64.trim())
        .context("Invalid base64 attachment keys")?;

    if keys.len() != 64 {
        return Err(anyhow!(
            "Expected 64-byte Signal attachment key, got {} bytes",
            keys.len()
        ));
    }
    if data.len() < 16 + 16 + 32 {
        return Err(anyhow!("Encrypted attachment is too short"));
    }

    let aes_key = &keys[0..32];
    let mac_key = &keys[32..64];

    let (iv_and_ct, expected_mac) = data.split_at(data.len() - 32);
    let mut mac = HmacSha256::new_from_slice(mac_key)?;
    mac.update(iv_and_ct);
    mac.verify_slice(expected_mac)
        .map_err(|_| anyhow!("Attachment HMAC-SHA256 verification failed"))?;

    let (iv, ct) = iv_and_ct.split_at(16);
    let mut buf = ct.to_vec();
    let plaintext = Aes256CbcDec::new_from_slices(aes_key, iv)
        .map_err(|e| anyhow!("AES-256-CBC init failed: {e}"))?
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("AES-256-CBC decrypt failed: {e}"))?;

    Ok(plaintext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;
    use tempfile::TempDir;

    type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;

    #[test]
    fn test_path_traversal_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let store = AttachmentStore::new(tmp.path().to_path_buf());

        assert!(store
            .resolve_safe_path(ATTACHMENTS_DIR, "../../etc/passwd")
            .is_err());
        assert!(store
            .resolve_safe_path(ATTACHMENTS_DIR, "/etc/passwd")
            .is_err());
        assert!(store
            .resolve_safe_path(ATTACHMENTS_DIR, "ab/abc123.jpg")
            .is_ok());
    }

    #[test]
    fn test_signal_attachment_decrypt_roundtrip() {
        let keys = [0x42u8; 64];
        let aes_key = &keys[0..32];
        let mac_key = &keys[32..64];
        let iv = [0x11u8; 16];
        let plaintext = b"hello raspberry pi 4b signal attachment!";

        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let ct = Aes256CbcEnc::new_from_slices(aes_key, &iv)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap();

        let mut payload = iv.to_vec();
        payload.extend_from_slice(ct);
        let mut mac = HmacSha256::new_from_slice(mac_key).unwrap();
        mac.update(&payload);
        payload.extend_from_slice(&mac.finalize().into_bytes());

        let keys_b64 = base64::engine::general_purpose::STANDARD.encode(keys);
        let decrypted = decrypt_signal_attachment(&payload, &keys_b64).unwrap();
        assert_eq!(decrypted, plaintext);

        // Tampered byte must fail HMAC verification
        let mut tampered = payload.clone();
        tampered[20] ^= 0xff;
        assert!(decrypt_signal_attachment(&tampered, &keys_b64).is_err());
    }
}

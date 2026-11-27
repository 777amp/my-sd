// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Configuration management and Electron `safeStorage` key compatibility.
//!
//! Replaces `app/config.main.ts`, `app/user_config.main.ts`,
//! `app/ephemeral_config.main.ts`, and `app/base_config.node.ts`.
//!
//! Supports reading existing Signal Desktop `~/.config/Signal/config.json` files,
//! including decrypting Electron Linux `safeStorage` `v10` (Chromium default
//! "peanuts" key) and `v11` (FreeDesktop Secret Service) `encryptedKey` values.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde_json::{Map, Value};
use sha1::Sha1;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Default Signal Desktop user data directory (`~/.config/Signal` on Linux).
pub fn resolve_user_data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("SIGNAL_STORAGE_PATH") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }

    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    if let Ok(profile) = std::env::var("SIGNAL_STORAGE_PROFILE") {
        if !profile.is_empty() {
            return base.join(format!("Signal-{profile}"));
        }
    }

    base.join("Signal")
}

/// Manages persistent `~/.config/Signal/config.json`, ephemeral config,
/// and merged static app config (`config/default.json` + `config/production.json`).
pub struct ConfigStore {
    user_data_dir: PathBuf,
    user_config_path: PathBuf,
    user_data: RwLock<Map<String, Value>>,
    ephemeral_data: RwLock<Map<String, Value>>,
    app_config: Value,
}

impl ConfigStore {
    pub fn load(user_data_dir: PathBuf, resource_dir: Option<&Path>) -> Result<Self> {
        fs::create_dir_all(&user_data_dir).with_context(|| {
            format!(
                "Failed to create user data directory: {}",
                user_data_dir.display()
            )
        })?;

        let user_config_path = user_data_dir.join("config.json");
        let user_map = if user_config_path.exists() {
            let raw = fs::read_to_string(&user_config_path).with_context(|| {
                format!("Failed to read {}", user_config_path.display())
            })?;
            serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
        } else {
            Map::new()
        };

        let app_config = load_merged_app_config(resource_dir);

        Ok(Self {
            user_data_dir,
            user_config_path,
            user_data: RwLock::new(user_map),
            ephemeral_data: RwLock::new(Map::new()),
            app_config,
        })
    }

    pub fn user_data_dir(&self) -> &Path {
        &self.user_data_dir
    }

    pub fn app_config(&self) -> &Value {
        &self.app_config
    }

    pub fn get_user(&self, key: &str) -> Option<Value> {
        self.user_data.read().get(key).cloned()
    }

    pub fn set_user(&self, key: &str, value: Value) -> Result<()> {
        {
            let mut guard = self.user_data.write();
            guard.insert(key.to_string(), value);
        }
        self.flush_user_config()
    }

    pub fn remove_user(&self, key: &str) -> Result<()> {
        {
            let mut guard = self.user_data.write();
            guard.remove(key);
        }
        self.flush_user_config()
    }

    pub fn remove_all_user_config(&self) -> Result<()> {
        {
            let mut guard = self.user_data.write();
            guard.clear();
        }
        if self.user_config_path.exists() {
            fs::remove_file(&self.user_config_path)?;
        }
        Ok(())
    }

    pub fn get_ephemeral(&self, key: &str) -> Option<Value> {
        self.ephemeral_data.read().get(key).cloned()
    }

    pub fn set_ephemeral(&self, key: &str, value: Value) {
        self.ephemeral_data
            .write()
            .insert(key.to_string(), value);
    }

    pub fn remove_all_ephemeral(&self) {
        self.ephemeral_data.write().clear();
    }

    /// Atomically writes `config.json` via temporary file + rename (0600 permissions on Unix).
    fn flush_user_config(&self) -> Result<()> {
        let serialized = {
            let guard = self.user_data.read();
            serde_json::to_string_pretty(&*guard)?
        };

        let tmp_path = self.user_data_dir.join(".config.json.tmp");
        fs::write(&tmp_path, serialized)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600));
        }

        fs::rename(&tmp_path, &self.user_config_path)?;
        Ok(())
    }

    /// Resolves the 64-character hex SQLCipher key from `~/.config/Signal/config.json`.
    ///
    /// Order of precedence (matching Signal Desktop on Linux):
    /// 1. `encryptedKey` (Electron `safeStorage` hex-encoded `v10` or `v11` ciphertext)
    /// 2. `key` (legacy / plaintext 64-char hex key)
    /// 3. Generate a new 32-byte (64-char hex) random key and persist it.
    pub async fn get_or_create_sql_key(&self) -> Result<String> {
        if let Some(Value::String(enc_hex)) = self.get_user("encryptedKey") {
            match decrypt_electron_safe_storage_hex(&enc_hex).await {
                Ok(key) if is_valid_hex_key(&key) => {
                    info!("Decrypted existing Electron safeStorage SQLCipher key");
                    return Ok(key);
                }
                Ok(_) => {
                    warn!("Decrypted safeStorage key was not valid hex; checking plaintext fallback");
                }
                Err(err) => {
                    warn!("Unable to decrypt Electron safeStorage encryptedKey: {err:#}");
                }
            }
        }

        if let Some(Value::String(plain_key)) = self.get_user("key") {
            if is_valid_hex_key(&plain_key) {
                info!("Using existing plaintext SQLCipher key from config.json");
                return Ok(plain_key);
            }
        }

        // Generate a fresh 256-bit (64 hex chars) SQLCipher key
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let new_key = hex::encode(bytes);
        self.set_user("key", Value::String(new_key.clone()))?;
        info!("Generated new 256-bit SQLCipher key and saved to config.json");
        Ok(new_key)
    }

    /// Builds the combined renderer `get-config` payload expected by `SignalContext`.
    pub fn build_renderer_config(&self, version: &str) -> Value {
        let mut merged = match self.app_config.as_object() {
            Some(obj) => obj.clone(),
            None => Map::new(),
        };

        merged.insert("version".into(), Value::String(version.to_string()));
        merged.insert("name".into(), Value::String("Signal".into()));
        merged.insert(
            "environment".into(),
            Value::String(
                std::env::var("SIGNAL_ENV").unwrap_or_else(|_| "production".into()),
            ),
        );
        merged.insert(
            "userDataPath".into(),
            Value::String(self.user_data_dir.to_string_lossy().into_owned()),
        );
        merged.insert("backend".into(), Value::String("rust-tauri-v2".into()));

        if let Some(theme) = self.get_user("theme") {
            merged.insert("theme".into(), theme);
        }

        Value::Object(merged)
    }
}

pub fn is_valid_hex_key(key: &str) -> bool {
    !key.is_empty() && key.len() % 2 == 0 && key.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decrypts an Electron Linux `safeStorage.encryptString(...).toString('hex')` value.
///
/// Chromium / Electron Linux `os_crypt` format:
/// - Prefix `b"v10"` -> PBKDF2-HMAC-SHA1(password = `"peanuts"`, salt = `"saltysalt"`, 1 iter, 16 bytes)
/// - Prefix `b"v11"` -> PBKDF2-HMAC-SHA1(password = FreeDesktop Secret Service `"Chromium Safe Storage"`, salt = `"saltysalt"`, 1 iter, 16 bytes)
/// - Cipher: AES-128-CBC with IV = 16 ASCII space bytes (`b"                "`), PKCS#7 padding.
pub async fn decrypt_electron_safe_storage_hex(enc_hex: &str) -> Result<String> {
    let raw = hex::decode(enc_hex.trim()).context("encryptedKey is not valid hex")?;
    if raw.len() <= 3 {
        return Err(anyhow!("encryptedKey too short"));
    }

    let (prefix, ciphertext) = raw.split_at(3);
    let password = match prefix {
        b"v10" => b"peanuts".to_vec(),
        b"v11" => lookup_linux_secret_service_password().await?,
        other => {
            return Err(anyhow!(
                "Unsupported safeStorage prefix: {:?}",
                String::from_utf8_lossy(other)
            ));
        }
    };

    decrypt_chromium_os_crypt(&password, ciphertext)
}

pub fn decrypt_chromium_os_crypt(password: &[u8], ciphertext: &[u8]) -> Result<String> {
    let mut derived_key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password, b"saltysalt", 1, &mut derived_key);

    let iv = [b' '; 16];
    let mut buf = ciphertext.to_vec();
    let plaintext = Aes128CbcDec::new(&derived_key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("AES-128-CBC PKCS7 decryption failed: {e}"))?;

    String::from_utf8(plaintext.to_vec()).context("Decrypted safeStorage payload is not UTF-8")
}

#[cfg(target_os = "linux")]
async fn lookup_linux_secret_service_password() -> Result<Vec<u8>> {
    use secret_service::{EncryptionType, SecretService};
    use std::collections::HashMap;

    let ss = SecretService::connect(EncryptionType::Dh)
        .await
        .context("Failed to connect to FreeDesktop Secret Service")?;

    let mut attrs = HashMap::new();
    attrs.insert("application", "Signal");
    let items = ss.search_items(attrs).await?;
    for item in items.unlocked.into_iter().chain(items.locked.into_iter()) {
        let _ = item.unlock().await;
        if let Ok(secret) = item.get_secret().await {
            if !secret.is_empty() {
                return Ok(secret);
            }
        }
    }

    // Fallback: Chromium Safe Storage attribute used by Electron on Linux
    let mut chrome_attrs = HashMap::new();
    chrome_attrs.insert("xdg:schema", "chrome_libsecret_os_crypt_password_v2");
    let chrome_items = ss.search_items(chrome_attrs).await?;
    for item in chrome_items
        .unlocked
        .into_iter()
        .chain(chrome_items.locked.into_iter())
    {
        let _ = item.unlock().await;
        if let Ok(secret) = item.get_secret().await {
            if !secret.is_empty() {
                return Ok(secret);
            }
        }
    }

    Err(anyhow!(
        "No matching v11 secret found in FreeDesktop Secret Service"
    ))
}

#[cfg(not(target_os = "linux"))]
async fn lookup_linux_secret_service_password() -> Result<Vec<u8>> {
    Err(anyhow!("v11 Secret Service is only supported on Linux"))
}

fn load_merged_app_config(resource_dir: Option<&Path>) -> Value {
    let mut base = Map::new();

    let candidates = [
        resource_dir.map(|d| d.join("config")),
        Some(PathBuf::from("config")),
        Some(PathBuf::from("../config")),
    ];

    for dir in candidates.into_iter().flatten() {
        let default_path = dir.join("default.json");
        let prod_path = dir.join("production.json");
        if default_path.exists() {
            if let Ok(text) = fs::read_to_string(&default_path) {
                if let Ok(Value::Object(obj)) = serde_json::from_str(&text) {
                    merge_json_maps(&mut base, obj);
                }
            }
            if prod_path.exists() {
                if let Ok(text) = fs::read_to_string(&prod_path) {
                    if let Ok(Value::Object(obj)) = serde_json::from_str(&text) {
                        merge_json_maps(&mut base, obj);
                    }
                }
            }
            break;
        }
    }

    Value::Object(base)
}

fn merge_json_maps(target: &mut Map<String, Value>, overlay: Map<String, Value>) {
    for (k, v) in overlay {
        match (target.get_mut(&k), v) {
            (Some(Value::Object(existing)), Value::Object(new_map)) => {
                merge_json_maps(existing, new_map);
            }
            (_, val) => {
                target.insert(k, val);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;
    use tempfile::TempDir;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    #[tokio::test]
    async fn test_electron_v10_safe_storage_decryption() {
        let secret_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // Encrypt using Chromium v10 format
        let mut derived = [0u8; 16];
        pbkdf2_hmac::<Sha1>(b"peanuts", b"saltysalt", 1, &mut derived);
        let iv = [b' '; 16];
        let mut buf = vec![0u8; secret_key.len() + 16];
        buf[..secret_key.len()].copy_from_slice(secret_key.as_bytes());
        let ct = Aes128CbcEnc::new(&derived.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, secret_key.len())
            .unwrap();

        let mut payload = b"v10".to_vec();
        payload.extend_from_slice(ct);
        let enc_hex = hex::encode(payload);

        let decrypted = decrypt_electron_safe_storage_hex(&enc_hex).await.unwrap();
        assert_eq!(decrypted, secret_key);
    }

    #[tokio::test]
    async fn test_config_store_roundtrip_and_key_generation() {
        let tmp = TempDir::new().unwrap();
        let store = ConfigStore::load(tmp.path().to_path_buf(), None).unwrap();

        let key1 = store.get_or_create_sql_key().await.unwrap();
        assert_eq!(key1.len(), 64);
        assert!(is_valid_hex_key(&key1));

        // Reloading from disk must return the exact same key
        let store2 = ConfigStore::load(tmp.path().to_path_buf(), None).unwrap();
        let key2 = store2.get_or_create_sql_key().await.unwrap();
        assert_eq!(key1, key2);
    }
}

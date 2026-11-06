//! Encrypted per-user face profiles under the state directory
//! (`/var/lib/faceauth/<user>/models.enc`).

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result};
use faceauth_core::is_safe_username;
use log::{info, warn};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserProfile {
    pub user: String,
    pub embeddings: Vec<Vec<f32>>,
    /// Recognizer model the embeddings came from. `None` for profiles written
    /// before 1.0: those came from the old INT8 MobileFaceNet file, which no
    /// longer ships, so they never match and have to be set up again.
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub created: u64,
    #[serde(default, alias = "last_updated")]
    pub updated: u64,
}

impl UserProfile {
    pub fn model_matches(&self, current: &str) -> bool {
        self.model_id.as_deref() == Some(current)
    }
}

pub struct SecureStorage {
    base_dir: PathBuf,
    key: Key<Aes256Gcm>,
}

impl SecureStorage {
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&base_dir)
            .with_context(|| format!("Failed to create {}", base_dir.display()))?;
        fs::set_permissions(&base_dir, fs::Permissions::from_mode(0o700))?;

        // The key sits on the same disk as the data it protects, so it only
        // helps against someone copying a profile file on its own. A TPM- or
        // keyring-sealed key would be needed for real protection at rest.
        let key_path = base_dir.join("master.key");
        let key = if key_path.exists() {
            let key_bytes = fs::read(&key_path)?;
            if key_bytes.len() != 32 {
                anyhow::bail!(
                    "{} is {} bytes, expected 32. Restore it from backup, or delete it and set up face unlock again.",
                    key_path.display(),
                    key_bytes.len()
                );
            }
            *Key::<Aes256Gcm>::from_slice(&key_bytes)
        } else {
            warn!("Generating a new master key at {}", key_path.display());
            let mut key = Key::<Aes256Gcm>::default();
            OsRng.fill_bytes(&mut key);
            write_private(&key_path, &key, true)?;
            key
        };
        Ok(Self { base_dir, key })
    }

    /// Directory for `username`, refusing anything that could escape `base_dir`.
    fn user_dir(&self, username: &str) -> Result<PathBuf> {
        if !is_safe_username(username) {
            anyhow::bail!("Refusing unsafe username {username:?}");
        }
        Ok(self.base_dir.join(username))
    }

    /// Encrypt and atomically replace the profile.
    pub fn save_user(&self, profile: &UserProfile) -> Result<()> {
        let user_dir = self.user_dir(&profile.user)?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&user_dir)?;

        let plaintext = serde_json::to_vec(profile)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = Aes256Gcm::new(&self.key)
            .encrypt(&nonce, plaintext.as_ref())
            .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;
        let mut content = nonce.to_vec();
        content.extend_from_slice(&ciphertext);

        let final_path = user_dir.join("models.enc");
        let temp_path = user_dir.join("models.enc.tmp");
        write_private(&temp_path, &content, false)?;
        fs::rename(&temp_path, &final_path).context("Failed to replace profile")?;
        // Make the rename itself durable.
        if let Ok(dir) = fs::File::open(&user_dir) {
            let _ = dir.sync_all();
        }
        info!("Saved profile for {} ({} samples)", profile.user, profile.embeddings.len());
        Ok(())
    }

    pub fn load_user(&self, username: &str) -> Result<Option<UserProfile>> {
        let path = self.user_dir(username)?.join("models.enc");
        let content = match fs::read(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
        };
        if content.len() < 12 {
            anyhow::bail!("{} is truncated", path.display());
        }
        let (nonce, ciphertext) = content.split_at(12);
        let plaintext = Aes256Gcm::new(&self.key)
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("{} could not be decrypted (wrong key or damaged file)", path.display()))?;
        Ok(Some(serde_json::from_slice(&plaintext)?))
    }

    pub fn list_users(&self) -> Result<Vec<UserProfile>> {
        let mut profiles = Vec::new();
        for entry in fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !is_safe_username(&name) {
                continue;
            }
            match self.load_user(&name) {
                Ok(Some(p)) => profiles.push(p),
                Ok(None) => {}
                Err(e) => warn!("Skipping profile {name}: {e:#}"),
            }
        }
        profiles.sort_by(|a, b| a.user.cmp(&b.user));
        Ok(profiles)
    }

    pub fn delete_user(&self, username: &str) -> Result<()> {
        let user_dir = self.user_dir(username)?;
        match fs::remove_dir_all(&user_dir) {
            Ok(()) => {
                info!("Deleted face data for {username}");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Write `data` to `path` with mode 0600 from the moment the file exists.
fn write_private(path: &Path, data: &[u8], create_new: bool) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).mode(0o600);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    let mut file = opts.open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("faceauth-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trip_and_delete() {
        let dir = temp_dir("roundtrip");
        let store = SecureStorage::new(dir.clone()).unwrap();
        let profile = UserProfile { user: "alice".into(), embeddings: vec![vec![0.6, 0.8]], model_id: Some("abc".into()), created: 1, updated: 2 };
        store.save_user(&profile).unwrap();
        let back = store.load_user("alice").unwrap().unwrap();
        assert_eq!(back.embeddings, profile.embeddings);
        assert_eq!(store.list_users().unwrap().len(), 1);
        let mode = fs::metadata(dir.join("alice/models.enc")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        store.delete_user("alice").unwrap();
        assert!(store.load_user("alice").unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_traversal() {
        let dir = temp_dir("traversal");
        let store = SecureStorage::new(dir.clone()).unwrap();
        for bad in ["/", "..", "../x", "/etc"] {
            assert!(store.delete_user(bad).is_err());
            assert!(store.load_user(bad).is_err());
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_legacy_profiles() {
        let legacy = r#"{"user":"bob","name":"bob","embeddings":[[1.0]],"last_updated":42}"#;
        let p: UserProfile = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.updated, 42);
        assert!(!p.model_matches("anything"), "pre-1.0 profiles must be set up again");
    }
}

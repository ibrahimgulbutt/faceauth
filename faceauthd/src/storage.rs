use anyhow::{Result, Context};
use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce, Key
};
use rand::RngCore;
use std::fs;
use std::path::PathBuf;
use serde::{Serialize, Deserialize};
use log::{info, warn};

#[derive(Serialize, Deserialize)]
pub struct UserProfile {
    pub user: String,
    pub name: String,
    pub embeddings: Vec<Vec<f32>>,
    pub last_updated: u64,
}

pub struct SecureStorage {
    base_dir: PathBuf,
    key: Key<Aes256Gcm>,
}

impl SecureStorage {
    pub fn new() -> Result<Self> {
        let base_dir = dirs::data_local_dir()
            .context("Could not find local data directory")?
            .join("faceauth");
        
        fs::create_dir_all(&base_dir)?;

        // In a real production app, this key should come from TPM or a protected keyring.
        // For Phase 2, we will store a key locally but warn about it.
        let key_path = base_dir.join("master.key");
        let key = if key_path.exists() {
            let key_bytes = fs::read(&key_path)?;
            *Key::<Aes256Gcm>::from_slice(&key_bytes)
        } else {
            warn!("Generating new master key. This is not TPM-backed yet!");
            let mut key = Key::<Aes256Gcm>::default();
            OsRng.fill_bytes(&mut key);
            fs::write(&key_path, &key)?;
            key
        };

        Ok(Self { base_dir, key })
    }

    pub fn save_user(&self, profile: &UserProfile) -> Result<()> {
        let user_dir = self.base_dir.join(&profile.user);
        fs::create_dir_all(&user_dir)?;

        let data = serde_json::to_vec(profile)?;
        let cipher = Aes256Gcm::new(&self.key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher.encrypt(&nonce, data.as_ref())
            .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

        let mut file_content = nonce.to_vec();
        file_content.extend_from_slice(&ciphertext);

        fs::write(user_dir.join("models.enc"), file_content)?;
        info!("Saved encrypted profile for user {}", profile.user);
        Ok(())
    }


    pub fn load_user(&self, username: &str) -> Result<Option<UserProfile>> {
        let path = self.base_dir.join(username).join("models.enc");
        if !path.exists() {
            return Ok(None);
        }

        let file_content = fs::read(path)?;
        if file_content.len() < 12 {
            anyhow::bail!("File too short");
        }

        let (nonce_bytes, ciphertext) = file_content.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new(&self.key);

        let plaintext = cipher.decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

        let profile: UserProfile = serde_json::from_slice(&plaintext)?;
        Ok(Some(profile))
    }

    pub fn list_users(&self) -> Result<Vec<(String, usize)>> {
        let mut results = Vec::new();
        if !self.base_dir.exists() {
            return Ok(results);
        }

        for entry in fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Ok(username) = entry.file_name().into_string() {
                    if let Ok(Some(profile)) = self.load_user(&username) {
                        results.push((username, profile.embeddings.len()));
                    }
                }
            }
        }
        Ok(results)
    }
}

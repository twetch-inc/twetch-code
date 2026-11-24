//! Where the recovery phrase and the gateway key live: the OS keychain.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use zeroize::Zeroizing;

pub const MNEMONIC: &str = "recovery-phrase";
pub const GATEWAY_KEY: &str = "gateway-key";

pub trait SecretStore: Send + Sync {
    /// `None` only when the store answered that there is no such secret.
    ///
    /// # Errors
    /// When the store cannot be read (locked, denied). Never treat that as empty.
    fn get(&self, name: &str) -> anyhow::Result<Option<Zeroizing<String>>>;
    /// # Errors
    /// When the secret cannot be stored.
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// # Errors
    /// When the secret cannot be removed.
    fn delete(&self, name: &str) -> anyhow::Result<()>;
}

/// macOS Keychain, Windows Credential Manager, or the Linux Secret Service.
pub struct Keychain {
    service: String,
}

impl Keychain {
    /// The default home owns the plain service name; another home (a second
    /// profile, a test run) gets its own entries and so its own wallet.
    #[must_use]
    pub fn for_home(home: &Path, default_home: &Path) -> Self {
        let base = "com.twetch.code.wallet";
        let service = if home == default_home {
            base.to_string()
        } else {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(home.to_string_lossy().as_bytes());
            format!("{base}.{}", hex::encode(&digest[..8]))
        };
        Self { service }
    }

    fn entry(&self, name: &str) -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new(&self.service, name)?)
    }
}

impl SecretStore for Keychain {
    fn get(&self, name: &str) -> anyhow::Result<Option<Zeroizing<String>>> {
        match self.entry(name)?.get_password() {
            Ok(v) => Ok(Some(Zeroizing::new(v))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("the system keychain refused: {e}")),
        }
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        Ok(self.entry(name)?.set_password(value)?)
    }

    fn delete(&self, name: &str) -> anyhow::Result<()> {
        match self.entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// For tests.
#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<HashMap<String, String>>,
    pub locked: std::sync::atomic::AtomicBool,
}

impl SecretStore for MemorySecrets {
    fn get(&self, name: &str) -> anyhow::Result<Option<Zeroizing<String>>> {
        anyhow::ensure!(!self.locked.load(std::sync::atomic::Ordering::SeqCst), "keychain locked");
        Ok(self.map.lock().expect("secrets").get(name).cloned().map(Zeroizing::new))
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!self.locked.load(std::sync::atomic::Ordering::SeqCst), "keychain locked");
        self.map.lock().expect("secrets").insert(name.into(), value.into());
        Ok(())
    }

    fn delete(&self, name: &str) -> anyhow::Result<()> {
        self.map.lock().expect("secrets").remove(name);
        Ok(())
    }
}

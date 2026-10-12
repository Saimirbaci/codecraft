//! Password storage. Data source passwords live in the OS credential store (the macOS Keychain),
//! never in project files. Tests use [`MemoryStore`].

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::{DbError, DbResult};

/// Where data source passwords are kept. Calls may block (the Keychain can show a prompt), so
/// call them from worker threads.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> DbResult<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> DbResult<()>;
    /// Removing a missing entry is not an error.
    fn delete(&self, account: &str) -> DbResult<()>;
}

/// The OS credential store (Keychain on macOS) under one service name.
pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    /// The service name CodeCraft uses for data source passwords.
    pub const SERVICE: &'static str = "CodeCraft Database";

    pub fn new(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn entry(&self, account: &str) -> DbResult<keyring::Entry> {
        keyring::Entry::new(&self.service, account).map_err(|e| DbError::Secret(e.to_string()))
    }
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new(Self::SERVICE)
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, account: &str) -> DbResult<Option<String>> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(DbError::Secret(e.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &str) -> DbResult<()> {
        self.entry(account)?.set_password(secret).map_err(|e| DbError::Secret(e.to_string()))
    }

    fn delete(&self, account: &str) -> DbResult<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(DbError::Secret(e.to_string())),
        }
    }
}

/// An in-memory store (tests, and sessions that must not touch the Keychain).
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> DbResult<Option<String>> {
        Ok(self.map.lock().unwrap_or_else(PoisonError::into_inner).get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> DbResult<()> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner).insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> DbResult<()> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner).remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let s = MemoryStore::default();
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", "pw").unwrap();
        assert_eq!(s.get("a").unwrap().as_deref(), Some("pw"));
        s.delete("a").unwrap();
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
    }

    /// Touches the real Keychain with a test-only service name, and removes what it creates.
    /// Opt in with `CODECRAFT_TEST_KEYCHAIN=1` (a locked keychain would show a prompt).
    #[test]
    fn keychain_round_trip_when_enabled() {
        if std::env::var("CODECRAFT_TEST_KEYCHAIN").as_deref() != Ok("1") {
            eprintln!("skipping: set CODECRAFT_TEST_KEYCHAIN=1 to test the Keychain");
            return;
        }
        let store = KeychainStore::new(format!("codecraft-db-test-{}", std::process::id()));
        let account = "test-account";
        store.set(account, "s3cret").unwrap();
        let got = store.get(account);
        store.delete(account).unwrap();
        assert_eq!(got.unwrap().as_deref(), Some("s3cret"));
        assert_eq!(store.get(account).unwrap(), None);
    }
}

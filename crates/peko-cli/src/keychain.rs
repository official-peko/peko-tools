//! Shared access to the operating system keychain.
//!
//! One process-wide credential store is selected by host operating system on
//! the first call. macOS and iOS use the Apple keychain, Windows uses the
//! Credential Manager, and Linux uses the Secret Service over D-Bus. Other
//! hosts have no store and every operation returns an error. Secrets are
//! addressed by a service string and an account string.

use std::sync::Once;

use thiserror::Error;

/// A keychain operation failed.
#[derive(Debug, Error)]
pub enum KeychainError {
    /// The entry could not be opened.
    #[error("keychain open failed: {0}")]
    Open(String),

    /// The secret could not be written.
    #[error("keychain write failed: {0}")]
    Write(String),

    /// The secret could not be deleted.
    #[error("keychain delete failed: {0}")]
    Delete(String),
}

/// Select this process's default credential store on the first call. Later
/// calls are no-ops.
pub fn ensure_store() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Ok(store) = apple_native_keyring_store::keychain::Store::new() {
            keyring_core::set_default_store(store);
        }
        #[cfg(target_os = "windows")]
        if let Ok(store) = windows_native_keyring_store::Store::new() {
            keyring_core::set_default_store(store);
        }
        #[cfg(target_os = "linux")]
        if let Ok(store) = dbus_secret_service_keyring_store::Store::new() {
            keyring_core::set_default_store(store);
        }
    });
}

/// Store a secret under a service and account, replacing any existing value.
pub fn set(service: &str, account: &str, secret: &str) -> Result<(), KeychainError> {
    ensure_store();
    let entry = keyring_core::Entry::new(service, account)
        .map_err(|e| KeychainError::Open(e.to_string()))?;
    entry
        .set_password(secret)
        .map_err(|e| KeychainError::Write(e.to_string()))
}

/// Read a secret, or `None` when no entry exists.
pub fn get(service: &str, account: &str) -> Option<String> {
    ensure_store();
    let entry = keyring_core::Entry::new(service, account).ok()?;
    entry.get_password().ok()
}

/// Remove a secret. A missing entry is not an error.
pub fn delete(service: &str, account: &str) -> Result<(), KeychainError> {
    ensure_store();
    let entry = keyring_core::Entry::new(service, account)
        .map_err(|e| KeychainError::Open(e.to_string()))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(KeychainError::Delete(e.to_string())),
    }
}

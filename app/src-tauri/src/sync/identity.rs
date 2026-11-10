//! This device's sync identity: an Ed25519 key kept in the OS credential
//! store (Keychain on macOS, Credential Manager on Windows), and a display
//! name.

use iroh::SecretKey;
use serde::Serialize;
use std::path::Path;

const SERVICE: &str = "com.clipflow.app";
const ACCOUNT: &str = "sync-device-key";
/// Used only when the OS credential store is unavailable.
const FALLBACK_FILE: &str = "sync_identity.key";

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum KeyStorage {
    Keychain,
    File,
}

fn parse(hex: &str) -> Option<SecretKey> {
    super::pairing::from_hex32(hex.trim()).map(|bytes| SecretKey::from_bytes(&bytes))
}

fn read_file(data_dir: &Path) -> Option<SecretKey> {
    std::fs::read_to_string(data_dir.join(FALLBACK_FILE)).ok().and_then(|s| parse(&s))
}

/// Loads the device key, creating it on first use. A key left in the
/// fallback file by an earlier run moves into the credential store once the
/// store works.
pub fn load_or_create(data_dir: &Path) -> Result<(SecretKey, KeyStorage), String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT);
    if let Ok(entry) = &entry {
        match entry.get_password() {
            Ok(hex) => {
                if let Some(key) = parse(&hex) {
                    return Ok((key, KeyStorage::Keychain));
                }
                // Unreadable stored value: replace it below.
            }
            Err(keyring::Error::NoEntry) => {}
            Err(e) => return fallback(data_dir, &format!("credential store unavailable: {e}")),
        }
        let key = read_file(data_dir).unwrap_or_else(SecretKey::generate);
        let hex = super::pairing::to_hex(&key.to_bytes());
        if entry.set_password(&hex).is_ok() && entry.get_password().ok().as_deref() == Some(hex.as_str()) {
            let _ = std::fs::remove_file(data_dir.join(FALLBACK_FILE));
            return Ok((key, KeyStorage::Keychain));
        }
        return fallback(data_dir, "credential store refused the key");
    }
    fallback(data_dir, "credential store unavailable")
}

fn fallback(data_dir: &Path, why: &str) -> Result<(SecretKey, KeyStorage), String> {
    eprintln!("[clipet] sync key kept in a file: {why}");
    if let Some(key) = read_file(data_dir) {
        return Ok((key, KeyStorage::File));
    }
    let key = SecretKey::generate();
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    std::fs::write(data_dir.join(FALLBACK_FILE), super::pairing::to_hex(&key.to_bytes()))
        .map_err(|e| format!("could not save the device key: {e}"))?;
    Ok((key, KeyStorage::File))
}

/// The computer's own name, used until the user renames this device.
pub fn default_device_name() -> String {
    #[cfg(target_os = "windows")]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok());
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let name: Option<String> = None;
    name.map(|n| clean_name(&n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "This device".to_string())
}

/// Device names come from users and from other devices: single line, no
/// control characters, at most 64 characters.
pub fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Kim's\nPC\u{7}  "), "Kim'sPC");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 64);
    }

    /// Touches the real OS credential store (under a throwaway test entry,
    /// deleted afterwards), so it only runs on request.
    #[test]
    #[ignore]
    fn os_credential_store_round_trips_a_key() {
        let account = format!("test-{}", super::super::pairing::to_hex(&super::super::pairing::new_secret().unwrap()[..6]));
        let entry = keyring::Entry::new("com.clipflow.app.test", &account).unwrap();
        let hex = super::super::pairing::to_hex(&SecretKey::generate().to_bytes());
        entry.set_password(&hex).unwrap();
        let read_back = entry.get_password().unwrap();
        entry.delete_credential().unwrap();
        assert_eq!(read_back, hex);
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }

    #[test]
    fn fallback_file_key_is_stable() {
        let dir = crate::sync::test_dir("identity");
        let (a, storage) = fallback(&dir, "test").unwrap();
        assert_eq!(storage, KeyStorage::File);
        let (b, _) = fallback(&dir, "test").unwrap();
        assert_eq!(a.public(), b.public());
    }
}

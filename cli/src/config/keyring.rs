use anyhow::Result;
use keyring_core::Entry;

const SERVICE: &str = "postr";
const USER: &str = "default";

/// Register the platform's native credential store as the keyring-core
/// default. Call once at process start, before any `save_token` /
/// `load_token` / `delete_token`. keyring-core itself ships no backend;
/// each platform crate (apple-native / dbus-secret-service / windows-native)
/// provides a `Store::new()` we hand off here.
pub fn init() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use apple_native_keyring_store::keychain;
        keyring_core::set_default_store(keychain::Store::new()?);
    }
    #[cfg(target_os = "linux")]
    {
        use dbus_secret_service_keyring_store as store;
        keyring_core::set_default_store(store::Store::new()?);
    }
    #[cfg(target_os = "windows")]
    {
        use windows_native_keyring_store as store;
        keyring_core::set_default_store(store::Store::new()?);
    }
    Ok(())
}

fn entry() -> Result<Entry> {
    Ok(Entry::new(SERVICE, USER)?)
}

pub fn save_token(token: &str) -> Result<()> {
    entry()?.set_password(token)?;
    Ok(())
}

pub fn load_token() -> Result<Option<String>> {
    match entry()?.get_password() {
        Ok(t) => Ok(Some(t)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_token() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

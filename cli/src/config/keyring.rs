use anyhow::Result;
use keyring::Entry;

const SERVICE: &str = "postr";
const USER: &str = "default";

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
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_token() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

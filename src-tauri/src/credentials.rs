//! The GitHub token entered in Settings, kept in the macOS Keychain as a generic
//! password under the app's bundle identifier.

const ACCOUNT: &str = "github-token";

#[cfg(target_os = "macos")]
mod store {
    use super::ACCOUNT;
    use crate::error::{Error, Result};
    use security_framework::passwords;

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    pub fn load(service: &str) -> Result<Option<String>> {
        match passwords::get_generic_password(service, ACCOUNT) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| Error::msg("The Keychain entry is not valid text")),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(Error::msg(format!("Keychain: {e}"))),
        }
    }

    pub fn save(service: &str, token: &str) -> Result<()> {
        passwords::set_generic_password(service, ACCOUNT, token.as_bytes())
            .map_err(|e| Error::msg(format!("Keychain: {e}")))
    }

    pub fn delete(service: &str) -> Result<()> {
        match passwords::delete_generic_password(service, ACCOUNT) {
            Err(e) if e.code() != NOT_FOUND => Err(Error::msg(format!("Keychain: {e}"))),
            _ => Ok(()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod store {
    use crate::error::{Error, Result};

    pub fn load(_service: &str) -> Result<Option<String>> {
        Ok(None)
    }

    pub fn save(_service: &str, _token: &str) -> Result<()> {
        Err(Error::msg(
            "Saving a token is only supported on macOS; set GITHUB_TOKEN instead.",
        ))
    }

    pub fn delete(_service: &str) -> Result<()> {
        Ok(())
    }
}

pub use store::{delete, load, save};

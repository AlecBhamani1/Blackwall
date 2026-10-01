//! Platform credential stores: macOS Keychain, Windows Credential Manager, and the
//! freedesktop Secret Service (GNOME Keyring, KWallet) on Linux.
//! Service and account names are identical on every platform.

/// The credential store's user-facing name, for use mid-sentence.
pub(crate) const STORE: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(windows) {
    "Windows Credential Manager"
} else {
    "the system keyring"
};
/// The recovery step for an unresponsive store, written to precede ", then …".
pub(crate) const RECOVER: &str = if cfg!(target_os = "macos") {
    "Respond to any macOS Keychain prompt and unlock your login keychain if needed"
} else if cfg!(windows) {
    "Make sure you are signed in to Windows and Credential Manager is available"
} else {
    "Respond to any keyring unlock prompt and make sure GNOME Keyring, KWallet, or another Secret Service provider is running"
};

const UNAVAILABLE: &str = "No system keyring is available. Install and unlock a Secret Service provider such as GNOME Keyring or KWallet, then try again.";

#[derive(Debug, PartialEq)]
pub(crate) enum SecretError {
    /// The saved value is not valid UTF-8.
    Invalid,
    /// The platform cannot store a value this large.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    TooLong,
    /// No credential store is running or reachable.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unavailable,
    /// The store refused or failed the operation; callers explain what was affected.
    Failed,
}
impl SecretError {
    pub(crate) fn explain(self, failure: impl Into<String>) -> String {
        match self {
            Self::Invalid => "The saved access key is not valid text.".into(),
            Self::TooLong => format!("This access key is too long for {STORE}."),
            Self::Unavailable => UNAVAILABLE.into(),
            Self::Failed => failure.into(),
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::SecretError;
    use security_framework::passwords::{
        delete_generic_password, generic_password, set_generic_password, PasswordOptions,
    };
    const NOT_FOUND: i32 = -25300;

    pub fn get(service: &str, account: &str) -> Result<Option<String>, SecretError> {
        match generic_password(PasswordOptions::new_generic_password(service, account)) {
            Ok(value) => String::from_utf8(value)
                .map(Some)
                .map_err(|_| SecretError::Invalid),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(_) => Err(SecretError::Failed),
        }
    }
    pub fn set(service: &str, account: &str, value: &str) -> Result<(), SecretError> {
        set_generic_password(service, account, value.as_bytes()).map_err(|_| SecretError::Failed)
    }
    pub fn delete(service: &str, account: &str) -> Result<(), SecretError> {
        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == NOT_FOUND => Ok(()),
            Err(_) => Err(SecretError::Failed),
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
mod platform {
    use super::SecretError;
    use keyring::{Entry, Error};

    fn classify(error: Error) -> SecretError {
        match error {
            Error::BadEncoding(_) => SecretError::Invalid,
            Error::TooLong(..) => SecretError::TooLong,
            // Secret Service reports a missing or unreachable D-Bus provider as a platform failure.
            Error::PlatformFailure(_) if cfg!(target_os = "linux") => SecretError::Unavailable,
            _ => SecretError::Failed,
        }
    }
    fn entry(service: &str, account: &str) -> Result<Entry, SecretError> {
        Entry::new(service, account).map_err(classify)
    }
    pub fn get(service: &str, account: &str) -> Result<Option<String>, SecretError> {
        match entry(service, account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(classify(error)),
        }
    }
    pub fn set(service: &str, account: &str, value: &str) -> Result<(), SecretError> {
        entry(service, account)?
            .set_password(value)
            .map_err(classify)
    }
    pub fn delete(service: &str, account: &str) -> Result<(), SecretError> {
        match entry(service, account)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(error) => Err(classify(error)),
        }
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use super::SecretError;
    pub fn get(_: &str, _: &str) -> Result<Option<String>, SecretError> {
        Ok(None)
    }
    pub fn set(_: &str, _: &str, _: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable)
    }
    pub fn delete(_: &str, _: &str) -> Result<(), SecretError> {
        Ok(())
    }
}

/// Blocking: call from `spawn_blocking` or `crate::keychain::read`.
pub(crate) fn get(service: &str, account: &str) -> Result<Option<String>, SecretError> {
    platform::get(service, account)
}
/// Blocking: call from `spawn_blocking`.
pub(crate) fn set(service: &str, account: &str, value: &str) -> Result<(), SecretError> {
    platform::set(service, account, value)
}
/// Blocking: call from `spawn_blocking`. Removing a missing value succeeds.
pub(crate) fn delete(service: &str, account: &str) -> Result<(), SecretError> {
    platform::delete(service, account)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errors_explain_store_specific_failures_without_values() {
        assert_eq!(
            SecretError::Failed.explain("Could not save."),
            "Could not save."
        );
        assert!(SecretError::TooLong.explain("unused").contains(STORE));
        assert!(SecretError::Unavailable
            .explain("unused")
            .contains("Secret Service"));
        assert_ne!(SecretError::Invalid.explain("unused"), "unused");
    }
}

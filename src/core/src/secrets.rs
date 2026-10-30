//! Platform credential stores: macOS Keychain, Windows Credential Manager, and the
//! freedesktop Secret Service (GNOME Keyring, KWallet) on Linux.
//! Service and account names are identical on every platform.

/// The credential store's user-facing name, for use mid-sentence.
pub const STORE: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(windows) {
    "Windows Credential Manager"
} else {
    "the system keyring"
};
/// The recovery step for an unresponsive store, written to precede ", then …".
pub const RECOVER: &str = if cfg!(target_os = "macos") {
    "Respond to any macOS Keychain prompt and unlock your login keychain if needed"
} else if cfg!(windows) {
    "Make sure you are signed in to Windows and Credential Manager is available"
} else {
    "Respond to any keyring unlock prompt and make sure GNOME Keyring, KWallet, or another Secret Service provider is running"
};

const UNAVAILABLE: &str = "No system keyring is available. Install and unlock a Secret Service provider such as GNOME Keyring or KWallet, then try again.";

/// What a save that only the keyring can hold says when there is no keyring.
pub const KEYRING_OFF_NOTE: &str = "No system keyring is available, so this can't be saved. Start GNOME Keyring or KWallet, then restart Blackwall.";

/// A secret write, replaceable in tests.
pub type SetSecret = fn(&str, &str, &str) -> Result<(), SecretError>;

#[derive(Debug, PartialEq)]
pub enum SecretError {
    /// The saved value is not valid UTF-8.
    Invalid,
    /// The platform cannot store a value this large.
    TooLong,
    /// No credential store is running or reachable.
    Unavailable,
    /// The store refused or failed the operation; callers explain what was affected.
    Failed,
}
impl SecretError {
    pub fn explain(self, failure: impl Into<String>) -> String {
        match self {
            Self::Invalid => "The saved access key is not valid text.".into(),
            Self::TooLong => format!("This access key is too long for {STORE}."),
            Self::Unavailable => UNAVAILABLE.into(),
            Self::Failed => failure.into(),
        }
    }
    /// Like [`explain`](Self::explain) for a save that cannot be kept anywhere else.
    pub fn explain_save(self, failure: impl Into<String>) -> String {
        match self {
            Self::Unavailable => KEYRING_OFF_NOTE.into(),
            other => other.explain(failure),
        }
    }
}

#[cfg(all(feature = "credential-store", target_os = "macos"))]
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

#[cfg(all(feature = "credential-store", any(windows, target_os = "linux")))]
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

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use super::*;
        #[test]
        fn locked_or_dismissed_keyring_is_failed_not_unavailable() {
            assert_eq!(
                classify(Error::NoStorageAccess(Box::new(std::io::Error::other(
                    "dismissed"
                )))),
                SecretError::Failed
            );
            assert_eq!(
                classify(Error::PlatformFailure(Box::new(std::io::Error::other(
                    "no dbus"
                )))),
                SecretError::Unavailable
            );
        }
    }
}

#[cfg(not(all(
    feature = "credential-store",
    any(target_os = "macos", windows, target_os = "linux")
)))]
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

/// Blocking: call from `spawn_blocking` or the desktop `keychain::read`.
pub fn get(service: &str, account: &str) -> Result<Option<String>, SecretError> {
    platform::get(service, account)
}
/// Blocking: call from `spawn_blocking`.
pub fn set(service: &str, account: &str, value: &str) -> Result<(), SecretError> {
    platform::set(service, account, value)
}
/// Blocking: call from `spawn_blocking`. Removing a missing value succeeds.
pub fn delete(service: &str, account: &str) -> Result<(), SecretError> {
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

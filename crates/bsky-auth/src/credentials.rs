//! Read app-password credentials from a TOML file on disk.
//!
//! Phase 1 dev-time only — Phase 2 will replace this with an IME-driven login
//! screen and the credentials file will go away once a `session.json` exists.

use serde::Deserialize;

use crate::AuthError;

#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    pub handle: String,
    pub app_password: String,
}

/// Read TOML from `path`. Returns `AuthError::Credentials` if the file is
/// missing or malformed. We deliberately don't fall back silently — if the
/// dev forgot to push credentials, fail loudly.
pub fn load_credentials(path: &str) -> Result<Credentials, AuthError> {
    let bytes = std::fs::read(path).map_err(|e| {
        AuthError::Credentials(format!("could not read {path}: {e}"))
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        AuthError::Credentials(format!("{path} is not utf-8: {e}"))
    })?;
    let creds: Credentials = toml::from_str(text).map_err(|e| {
        AuthError::Credentials(format!("could not parse {path}: {e}"))
    })?;
    if creds.handle.is_empty() || creds.app_password.is_empty() {
        return Err(AuthError::Credentials(
            "handle and app_password must both be non-empty".to_string(),
        ));
    }
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_toml() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("creds.toml");
        std::fs::write(
            &path,
            "handle = \"alice.example.com\"\napp_password = \"abcd-1234-efgh-5678\"\n",
        )
        .expect("write");
        let creds = load_credentials(path.to_str().unwrap()).expect("parse");
        assert_eq!(creds.handle, "alice.example.com");
        assert_eq!(creds.app_password, "abcd-1234-efgh-5678");
    }

    #[test]
    fn rejects_empty_fields() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("creds.toml");
        std::fs::write(&path, "handle = \"\"\napp_password = \"x\"\n").unwrap();
        assert!(load_credentials(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn rejects_missing_file() {
        assert!(load_credentials("/no/such/file/exists.toml").is_err());
    }
}

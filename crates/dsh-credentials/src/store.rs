//! One JSON object of provider secrets in a local file.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Longest accepted provider name, in bytes.
pub const MAX_PROVIDER_NAME_BYTES: usize = 64;

const TEMPORARY_SUFFIX: &str = ".tmp";

/// Credential file failures.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// The file could not be read.
    #[error("cannot read credential file {path}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file or its temporary sibling could not be written.
    #[error("cannot write credential file {path}: {source}")]
    Write {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file does not hold a JSON object of string secrets.
    #[error("credential file encoding failure: {0}")]
    Encoding(String),
    /// A provider name is empty, too long, or outside the accepted characters.
    #[error(
        "invalid provider name {name:?}: use 1 to {MAX_PROVIDER_NAME_BYTES} ASCII letters, digits, '-', '_', or '.'"
    )]
    InvalidProvider {
        /// The rejected name.
        name: String,
    },
    /// A secret must hold at least one character.
    #[error("secret for provider {provider:?} must not be empty")]
    EmptySecret {
        /// Provider the rejected secret was meant for.
        provider: String,
    },
}

/// A JSON file mapping provider name to secret.
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    /// Wrap a path. The file need not exist yet.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Path of the backing file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read every stored provider.
    pub fn load(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(source) => {
                return Err(CredentialError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        serde_json::from_slice(&bytes).map_err(|error| CredentialError::Encoding(error.to_string()))
    }

    /// Read the secret stored for one provider.
    pub fn get(&self, provider: &str) -> Result<Option<String>, CredentialError> {
        validate_provider(provider)?;
        Ok(self.load()?.get(provider).cloned())
    }

    /// Store or replace the secret for one provider.
    pub fn set(&self, provider: &str, secret: &str) -> Result<(), CredentialError> {
        validate_provider(provider)?;
        if secret.is_empty() {
            return Err(CredentialError::EmptySecret {
                provider: provider.to_string(),
            });
        }
        let mut credentials = self.load()?;
        credentials.insert(provider.to_string(), secret.to_string());
        self.save(&credentials)
    }

    /// Drop one provider, reporting whether it was present.
    pub fn remove(&self, provider: &str) -> Result<bool, CredentialError> {
        validate_provider(provider)?;
        let mut credentials = self.load()?;
        if credentials.remove(provider).is_none() {
            return Ok(false);
        }
        self.save(&credentials)?;
        Ok(true)
    }

    /// Provider names in sorted order.
    pub fn providers(&self) -> Result<Vec<String>, CredentialError> {
        Ok(self.load()?.into_keys().collect())
    }

    /// The object reaches disk through a sibling temporary file and a rename,
    /// so a concurrent reader sees either the old file or the new one.
    fn save(&self, credentials: &BTreeMap<String, String>) -> Result<(), CredentialError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| CredentialError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let encoded = serde_json::to_vec_pretty(credentials)
            .map_err(|error| CredentialError::Encoding(error.to_string()))?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, encoded).map_err(|source| CredentialError::Write {
            path: temporary.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(
                |source| CredentialError::Write {
                    path: temporary.clone(),
                    source,
                },
            )?;
        }
        fs::rename(&temporary, &self.path).map_err(|source| CredentialError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(TEMPORARY_SUFFIX);
    PathBuf::from(name)
}

fn validate_provider(provider: &str) -> Result<(), CredentialError> {
    let accepted = !provider.is_empty()
        && provider.len() <= MAX_PROVIDER_NAME_BYTES
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if accepted {
        return Ok(());
    }
    Err(CredentialError::InvalidProvider {
        name: provider.to_string(),
    })
}

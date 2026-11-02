//! Durable provider routes in one JSON file.

use dsh_core::{ProviderConfig, ProviderError, default_home, parse_providers};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub struct ProviderFile {
    path: PathBuf,
}

impl ProviderFile {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A missing or blank file is an empty list.
    pub fn load(&self) -> Result<Vec<ProviderConfig>, ProviderFileError> {
        let Some(raw) = self.read_raw()? else {
            return Ok(Vec::new());
        };
        parse_providers(&raw).map_err(|source| ProviderFileError::Invalid {
            path: self.path.clone(),
            source,
        })
    }

    /// Returns the raw table because configuration parses it as `DSH_PROVIDERS`.
    pub fn load_json(&self) -> Result<Option<String>, ProviderFileError> {
        let Some(raw) = self.read_raw()? else {
            return Ok(None);
        };
        parse_providers(&raw).map_err(|source| ProviderFileError::Invalid {
            path: self.path.clone(),
            source,
        })?;
        Ok(Some(raw))
    }

    pub fn add(&self, route: ProviderConfig) -> Result<(), ProviderFileError> {
        let mut routes = self.load()?;
        routes.retain(|existing| existing.name != route.name);
        routes.push(route);
        self.save(&routes)
    }

    pub fn remove(&self, name: &str) -> Result<bool, ProviderFileError> {
        let mut routes = self.load()?;
        let before = routes.len();
        routes.retain(|route| route.name != name);
        if routes.len() == before {
            return Ok(false);
        }
        self.save(&routes)?;
        Ok(true)
    }

    /// The array is validated before the atomic write, so a rejected route
    /// leaves the previous file in place.
    pub fn save(&self, routes: &[ProviderConfig]) -> Result<(), ProviderFileError> {
        let value = serde_json::Value::Array(routes.iter().map(route_value).collect());
        let encoded = serde_json::to_string(&value).map_err(|error| {
            ProviderFileError::Encoding(format!("{}: {error}", self.path.display()))
        })?;
        parse_providers(&encoded).map_err(|source| ProviderFileError::Invalid {
            path: self.path.clone(),
            source,
        })?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| ProviderFileError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, encoded.as_bytes()).map_err(|source| ProviderFileError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| ProviderFileError::Write {
            path: self.path.clone(),
            source,
        })
    }

    fn read_raw(&self) -> Result<Option<String>, ProviderFileError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) if raw.trim().is_empty() => Ok(None),
            Ok(raw) => Ok(Some(raw)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ProviderFileError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

/// `$DSH_HOME/providers.json`, or the platform data directory.
#[must_use]
pub fn default_path() -> PathBuf {
    std::env::var_os("DSH_HOME")
        .map_or_else(default_home, PathBuf::from)
        .join("providers.json")
}

#[derive(Debug, Error)]
pub enum ProviderFileError {
    #[error("cannot read provider file {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("cannot write provider file {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("provider file encoding failure: {0}")]
    Encoding(String),
    #[error("invalid provider file {path}: {source}")]
    Invalid {
        path: PathBuf,
        source: ProviderError,
    },
}

fn route_value(route: &ProviderConfig) -> serde_json::Value {
    serde_json::json!({
        "name": route.name.as_str(),
        "base_url": route.base_url.as_str(),
        "model": route.model.as_str(),
        "api_key_env": route.api_key_env.as_deref(),
        "auth": route.auth.as_str(),
    })
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::AuthStyle;

    fn file(directory: &tempfile::TempDir) -> ProviderFile {
        ProviderFile::new(directory.path().join("providers.json"))
    }

    fn route(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            base_url: format!("http://{name}.example"),
            model: format!("{name}-model"),
            api_key_env: None,
            auth: AuthStyle::XApiKey,
        }
    }

    #[test]
    fn routes_round_trip_without_leaving_a_temporary_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let mut with_variable = route("env-gw");
        with_variable.api_key_env = Some("ENV_GW_KEY".to_string());
        with_variable.auth = AuthStyle::Bearer;
        let routes = vec![route("local-gw"), with_variable];
        file.save(&routes).expect("save");
        assert_eq!(file.load().expect("load"), routes);
        let raw = fs::read_to_string(file.path()).expect("read");
        assert_eq!(
            file.load_json().expect("json").as_deref(),
            Some(raw.as_str())
        );

        let mut replaced = routes;
        replaced[0].model = "replaced-model".to_string();
        file.save(&replaced).expect("save again");
        assert_eq!(file.load().expect("load"), replaced);
        assert!(!temporary_path(file.path()).exists());
    }

    #[test]
    fn adding_a_name_again_replaces_that_route() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        file.add(route("first")).expect("add");
        file.add(route("second")).expect("add");
        let mut replacement = route("first");
        replacement.model = "new-model".to_string();
        file.add(replacement.clone()).expect("replace");
        assert_eq!(
            file.load().expect("load"),
            vec![route("second"), replacement]
        );
    }

    #[test]
    fn removing_a_route_keeps_the_others() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        file.save(&[route("first"), route("second")]).expect("save");
        assert!(file.remove("first").expect("remove"));
        assert_eq!(file.load().expect("load"), vec![route("second")]);
        assert!(!file.remove("first").expect("remove missing"));
        assert!(!file.remove("unknown").expect("remove unknown"));
        assert_eq!(file.load().expect("load"), vec![route("second")]);
    }

    #[test]
    fn an_absent_file_is_empty() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        assert_eq!(file.load().expect("load"), Vec::new());
        assert_eq!(file.load_json().expect("json"), None);
        assert!(!file.path().exists());
    }

    #[test]
    fn a_blank_file_is_empty() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        fs::write(file.path(), " \n\t").expect("write");
        assert_eq!(file.load().expect("load"), Vec::new());
        assert_eq!(file.load_json().expect("json"), None);
    }

    #[test]
    fn a_malformed_file_names_its_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        fs::write(file.path(), "{").expect("write");
        let path = file.path().display().to_string();
        let error = file.load().expect_err("malformed");
        assert!(matches!(error, ProviderFileError::Invalid { .. }));
        assert!(error.to_string().contains(&path), "{error}");
        let error = file.load_json().expect_err("malformed");
        assert!(error.to_string().contains(&path), "{error}");
    }

    #[test]
    fn a_rejected_add_leaves_the_file_unchanged() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        file.add(route("kept")).expect("add");
        let before = fs::read(file.path()).expect("read");
        let mut broken = route("broken");
        broken.base_url = "ftp://broken".to_string();
        assert!(matches!(
            file.add(broken),
            Err(ProviderFileError::Invalid { .. })
        ));
        assert_eq!(fs::read(file.path()).expect("read"), before);
        assert_eq!(file.load().expect("load"), vec![route("kept")]);
    }

    #[test]
    fn the_builtin_name_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let error = file.add(route("deepseek")).expect_err("reserved");
        assert!(matches!(
            error,
            ProviderFileError::Invalid {
                source: ProviderError::Entry { .. },
                ..
            }
        ));
        assert!(error.to_string().contains("reserved"));
        assert!(!file.path().exists());
    }

    #[test]
    fn saving_creates_the_parent_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = ProviderFile::new(directory.path().join("state").join("providers.json"));
        file.add(route("gw")).expect("add");
        assert_eq!(file.load().expect("load"), vec![route("gw")]);
    }

    #[test]
    fn unreadable_paths_are_read_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("directory");
        fs::create_dir(&path).expect("mkdir");
        let file = ProviderFile::new(&path);
        assert!(matches!(file.load(), Err(ProviderFileError::Read { .. })));
    }

    #[test]
    fn unwritable_paths_are_write_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let blocker = directory.path().join("blocker");
        fs::write(&blocker, b"").expect("write");
        let file = ProviderFile::new(blocker.join("providers.json"));
        assert!(matches!(
            file.save(&[route("gw")]),
            Err(ProviderFileError::Write { .. })
        ));
    }
}

use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AdapterKind, ExecutionProfile, adapter::ProfileValidationError};

pub const CONFIG_API_VERSION: &str = "agent-dock/config/v1alpha1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub api_version: String,
    pub profiles: Vec<ExecutionProfile>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConfigRevision(String);

impl fmt::Debug for ConfigRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConfigRevision(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceConfigOutcome {
    Replaced {
        config: Config,
        backup_path: PathBuf,
    },
    Changed,
}

impl Config {
    pub fn defaults() -> Self {
        Self {
            api_version: CONFIG_API_VERSION.to_owned(),
            profiles: [
                ("claude-default", "Claude (default)", AdapterKind::Claude),
                ("codex-default", "Codex (default)", AdapterKind::Codex),
                ("gemini-default", "Gemini (default)", AdapterKind::Gemini),
            ]
            .into_iter()
            .map(|(id, name, adapter)| ExecutionProfile {
                id: id.to_owned(),
                name: name.to_owned(),
                adapter,
                executable: None,
                model: None,
                args: Vec::new(),
            })
            .collect(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let document: toml::Value =
            toml::from_str(&source).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let api_version = document
            .get("api_version")
            .map(ToString::to_string)
            .unwrap_or_else(|| "missing".to_owned());
        if document.get("api_version").and_then(toml::Value::as_str) != Some(CONFIG_API_VERSION) {
            return Err(ConfigError::UnsupportedApi {
                found: api_version,
                expected: CONFIG_API_VERSION,
                revision: Some(ConfigRevision(source)),
            });
        }
        let config: Self = toml::from_str(&source).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn backup_and_replace_default_if_unchanged(
        path: &Path,
        expected: &ConfigRevision,
    ) -> Result<ReplaceConfigOutcome, ConfigError> {
        let config = Self::defaults();
        config.validate()?;
        let source = toml::to_string_pretty(&config).map_err(ConfigError::Serialize)?;
        let parent = path
            .parent()
            .ok_or_else(|| ConfigError::MissingParent(path.to_path_buf()))?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml");
        let (temporary_path, mut temporary) =
            create_unique_file(parent, &format!(".{file_name}"), "tmp")?;
        let result = (|| {
            temporary
                .write_all(source.as_bytes())
                .and_then(|()| temporary.sync_all())
                .map_err(|source| ConfigError::Write {
                    path: temporary_path.clone(),
                    source,
                })?;
            if fs::read_to_string(path).ok().as_deref() != Some(expected.0.as_str()) {
                return Ok(ReplaceConfigOutcome::Changed);
            }

            let (backup_path, mut backup) = create_unique_file(parent, file_name, "backup")?;
            backup
                .write_all(expected.0.as_bytes())
                .and_then(|()| backup.sync_all())
                .map_err(|source| ConfigError::Write {
                    path: backup_path.clone(),
                    source,
                })?;
            if fs::read_to_string(path).ok().as_deref() != Some(expected.0.as_str()) {
                let _ = fs::remove_file(&backup_path);
                return Ok(ReplaceConfigOutcome::Changed);
            }
            fs::rename(&temporary_path, path).map_err(|source| ConfigError::Replace {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(ReplaceConfigOutcome::Replaced {
                config,
                backup_path,
            })
        })();
        if temporary_path.exists() {
            let _ = fs::remove_file(temporary_path);
        }
        result
    }

    pub fn create_default(path: &Path) -> Result<Self, ConfigError> {
        let config = Self::defaults();
        config.validate()?;
        let source = toml::to_string_pretty(&config).map_err(ConfigError::Serialize)?;
        let parent = path
            .parent()
            .ok_or_else(|| ConfigError::MissingParent(path.to_path_buf()))?;
        fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| ConfigError::Create {
                path: path.to_path_buf(),
                source,
            })?;
        file.write_all(source.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.api_version != CONFIG_API_VERSION {
            return Err(ConfigError::UnsupportedApi {
                found: self.api_version.clone(),
                expected: CONFIG_API_VERSION,
                revision: None,
            });
        }
        if self.profiles.is_empty() {
            return Err(ConfigError::NoProfiles);
        }
        let mut ids = HashSet::new();
        for profile in &self.profiles {
            profile.validate()?;
            if !ids.insert(&profile.id) {
                return Err(ConfigError::DuplicateProfile(profile.id.clone()));
            }
        }
        Ok(())
    }
}

fn create_unique_file(
    parent: &Path,
    file_name: &str,
    purpose: &str,
) -> Result<(PathBuf, File), ConfigError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for sequence in 0..u32::MAX {
        let path = parent.join(format!(
            "{file_name}.{purpose}.{}.{}.{sequence}",
            std::process::id(),
            timestamp
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(ConfigError::Create { path, source }),
        }
    }
    unreachable!("u32 path suffixes were exhausted")
}

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    let base = BaseDirs::new().ok_or(ConfigError::NoConfigDirectory)?;
    Ok(base.config_dir().join("agent-dock").join("config.toml"))
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the user configuration directory")]
    NoConfigDirectory,
    #[error("configuration path `{0}` has no parent directory")]
    MissingParent(PathBuf),
    #[error("could not read configuration `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse configuration `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not create configuration directory `{path}`: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not create configuration `{path}` without overwriting it: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write configuration `{path}`: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not atomically replace configuration `{path}`: {source}")]
    Replace {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not serialize default configuration: {0}")]
    Serialize(toml::ser::Error),
    #[error("unsupported configuration API version {found}; expected {expected}")]
    UnsupportedApi {
        found: String,
        expected: &'static str,
        revision: Option<ConfigRevision>,
    },
    #[error("configuration must define at least one profile")]
    NoProfiles,
    #[error("duplicate profile id `{0}`")]
    DuplicateProfile(String),
    #[error(transparent)]
    InvalidProfile(#[from] ProfileValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_loads_default_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");
        let created = Config::create_default(&path).unwrap();
        assert_eq!(created, Config::load(&path).unwrap());
        assert!(matches!(
            Config::create_default(&path),
            Err(ConfigError::Create { .. })
        ));
    }

    #[test]
    fn rejects_unknown_schema() {
        let mut config = Config::defaults();
        config.api_version = "agent-dock/config/v1alpha2".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnsupportedApi { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_profile_ids() {
        let mut config = Config::defaults();
        config.profiles.push(config.profiles[0].clone());
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateProfile(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_adapter_while_loading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "api_version = \"agent-dock/config/v1alpha1\"\n[[profiles]]\nid = \"unknown\"\nname = \"Unknown\"\nadapter = \"other\"\n",
        )
        .unwrap();

        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn backs_up_and_atomically_replaces_an_unchanged_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = 1\n").unwrap();
        let revision = match Config::load(&path).unwrap_err() {
            ConfigError::UnsupportedApi {
                revision: Some(revision),
                ..
            } => revision,
            error => panic!("unexpected error: {error}"),
        };

        let ReplaceConfigOutcome::Replaced {
            config,
            backup_path,
        } = Config::backup_and_replace_default_if_unchanged(&path, &revision).unwrap()
        else {
            panic!("configuration was not replaced");
        };

        assert_eq!(
            fs::read_to_string(backup_path).unwrap(),
            "schema_version = 1\n"
        );
        assert_eq!(Config::load(&path).unwrap(), config);
    }
}

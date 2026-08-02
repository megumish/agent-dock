use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AdapterKind, ExecutionProfile, adapter::ProfileValidationError};

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub schema_version: u32,
    pub profiles: Vec<ExecutionProfile>,
}

impl Config {
    pub fn defaults() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            profiles: [
                ("codex-default", "Codex (default)", AdapterKind::Codex),
                ("claude-default", "Claude (default)", AdapterKind::Claude),
                ("gemini-default", "Gemini (default)", AdapterKind::Gemini),
                (
                    "antigravity-default",
                    "Antigravity (default)",
                    AdapterKind::Antigravity,
                ),
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
        let config: Self = toml::from_str(&source).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
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
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
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

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(ConfigError::NoConfigDirectory)?;
    Ok(home
        .join("Library")
        .join("Application Support")
        .join("agent-dock")
        .join("config.toml"))
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
    #[error("could not serialize default configuration: {0}")]
    Serialize(toml::ser::Error),
    #[error("unsupported configuration schema version {0}")]
    UnsupportedSchema(u32),
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
    fn uses_codex_as_the_default_profile() {
        let config = Config::defaults();
        assert_eq!(config.profiles[0].id, "codex-default");
    }

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
        config.schema_version = CONFIG_SCHEMA_VERSION + 1;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnsupportedSchema(_))
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
            "schema_version = 1\n[[profiles]]\nid = \"unknown\"\nname = \"Unknown\"\nadapter = \"other\"\n",
        )
        .unwrap();

        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
    }
}

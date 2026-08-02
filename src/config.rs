use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{CliKind, ProfileDeclaration, adapter::ProfileValidationError};

pub const CONFIG_SCHEMA_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub schema_version: String,
    pub record_prompt: bool,
    pub tag_candidates: Vec<String>,
    pub profiles: Vec<ProfileDeclaration>,
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
    pub fn defaults(record_prompt: bool) -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION.to_owned(),
            record_prompt,
            tag_candidates: Vec::new(),
            profiles: [
                ("Codex (default)", CliKind::Codex),
                ("Claude (default)", CliKind::Claude),
                ("Gemini (default)", CliKind::Gemini),
                ("Antigravity (default)", CliKind::Antigravity),
            ]
            .into_iter()
            .map(|(name, cli)| ProfileDeclaration {
                name: name.to_owned(),
                cli,
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
        let schema_version = document
            .get("schema_version")
            .map(ToString::to_string)
            .unwrap_or_else(|| "missing".to_owned());
        if document.get("schema_version").and_then(toml::Value::as_str)
            != Some(CONFIG_SCHEMA_VERSION)
        {
            return Err(ConfigError::UnsupportedSchema {
                found: schema_version,
                expected: CONFIG_SCHEMA_VERSION,
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

    pub fn create_default(path: &Path, record_prompt: bool) -> Result<Self, ConfigError> {
        let config = Self::defaults(record_prompt);
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
            .mode(0o600)
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

    pub fn backup_and_replace_default_if_unchanged(
        path: &Path,
        expected: &ConfigRevision,
        record_prompt: bool,
    ) -> Result<ReplaceConfigOutcome, ConfigError> {
        let config = Self::defaults(record_prompt);
        config.validate()?;
        let source = toml::to_string_pretty(&config).map_err(ConfigError::Serialize)?;
        let parent = path
            .parent()
            .ok_or_else(|| ConfigError::MissingParent(path.to_path_buf()))?;
        fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let lock_path = config_lock_path(path);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|source| ConfigError::OpenLock {
                path: lock_path.clone(),
                source,
            })?;
        let _lock = ConfigFileLock::acquire(lock_file, &lock_path)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml");
        let temporary_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::now_v7()));
        let backup_path = parent.join(format!("{file_name}.backup.{}", Uuid::now_v7()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary_path)
                .map_err(|source| ConfigError::Create {
                    path: temporary_path.clone(),
                    source,
                })?;
            file.write_all(source.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|source| ConfigError::Write {
                    path: temporary_path.clone(),
                    source,
                })?;
            match fs::read_to_string(path) {
                Ok(current) if current == expected.0 => {}
                Ok(_) => return Ok(ReplaceConfigOutcome::Changed),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ReplaceConfigOutcome::Changed);
                }
                Err(source) => {
                    return Err(ConfigError::Read {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
            let mut backup = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&backup_path)
                .map_err(|source| ConfigError::Create {
                    path: backup_path.clone(),
                    source,
                })?;
            backup
                .write_all(expected.0.as_bytes())
                .and_then(|()| backup.sync_all())
                .map_err(|source| ConfigError::Write {
                    path: backup_path.clone(),
                    source,
                })?;
            match fs::read_to_string(path) {
                Ok(current) if current == expected.0 => {}
                Ok(_) => return Ok(ReplaceConfigOutcome::Changed),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ReplaceConfigOutcome::Changed);
                }
                Err(source) => {
                    return Err(ConfigError::Read {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
            fs::rename(&temporary_path, path).map_err(|source| ConfigError::Replace {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(ReplaceConfigOutcome::Replaced {
                config,
                backup_path: backup_path.clone(),
            })
        })();
        if temporary_path.exists() {
            let _ = fs::remove_file(&temporary_path);
        }
        if !matches!(&result, Ok(ReplaceConfigOutcome::Replaced { .. })) && backup_path.exists() {
            let _ = fs::remove_file(&backup_path);
        }
        result
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema {
                found: self.schema_version.clone(),
                expected: CONFIG_SCHEMA_VERSION,
                revision: None,
            });
        }
        if self.profiles.is_empty() {
            return Err(ConfigError::NoProfiles);
        }
        let mut names = HashSet::new();
        let mut identities = HashSet::new();
        for profile in &self.profiles {
            profile.validate(false)?;
            if profile.name == crate::safe_test_declaration().name {
                return Err(ConfigError::ReservedSafeTestName(profile.name.clone()));
            }
            if !names.insert(&profile.name) {
                return Err(ConfigError::DuplicateProfileName(profile.name.clone()));
            }
            let identity = profile.canonical_identity()?;
            if !identities.insert(identity) {
                return Err(ConfigError::DuplicateProfileIdentity(profile.id()?));
            }
        }
        Ok(())
    }
}

fn config_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

struct ConfigFileLock {
    file: File,
}

impl ConfigFileLock {
    fn acquire(file: File, path: &Path) -> Result<Self, ConfigError> {
        // SAFETY: flock receives an open file descriptor and a supported operation.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            Ok(Self { file })
        } else {
            Err(ConfigError::Lock {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            })
        }
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        // SAFETY: the guard owns the open file until after flock returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
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
    #[error("could not open configuration lock `{path}`: {source}")]
    OpenLock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not lock configuration `{path}`: {source}")]
    Lock {
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
    #[error("unsupported configuration schema version {found}; expected {expected}")]
    UnsupportedSchema {
        found: String,
        expected: &'static str,
        revision: Option<ConfigRevision>,
    },
    #[error("configuration must define at least one profile")]
    NoProfiles,
    #[error("duplicate profile name `{0}`")]
    DuplicateProfileName(String),
    #[error("profile name `{0}` is reserved for the system safe-test profile")]
    ReservedSafeTestName(String),
    #[error("duplicate declared profile identity `{0}`")]
    DuplicateProfileIdentity(String),
    #[error(transparent)]
    InvalidProfile(#[from] ProfileValidationError),
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;

    #[test]
    fn creates_and_loads_default_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");
        let created = Config::create_default(&path, true).unwrap();
        assert_eq!(created.profiles[0].cli, CliKind::Codex);
        assert_eq!(created, Config::load(&path).unwrap());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(matches!(
            Config::create_default(&path, false),
            Err(ConfigError::Create { .. })
        ));
    }

    #[test]
    fn atomically_replaces_an_unchanged_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = \"old\"\n").unwrap();
        let revision = match Config::load(&path).unwrap_err() {
            ConfigError::UnsupportedSchema {
                revision: Some(revision),
                ..
            } => revision,
            error => panic!("unexpected error: {error}"),
        };

        let replaced =
            Config::backup_and_replace_default_if_unchanged(&path, &revision, true).unwrap();

        let ReplaceConfigOutcome::Replaced {
            config,
            backup_path,
        } = replaced
        else {
            panic!("configuration was not replaced");
        };
        assert!(config.record_prompt);
        assert_eq!(
            fs::read_to_string(&backup_path).unwrap(),
            "schema_version = \"old\"\n"
        );
        assert_eq!(
            fs::metadata(backup_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Config::load(&path).unwrap().record_prompt);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn preserves_a_configuration_changed_after_it_was_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = \"old\"\n").unwrap();
        let revision = match Config::load(&path).unwrap_err() {
            ConfigError::UnsupportedSchema {
                revision: Some(revision),
                ..
            } => revision,
            error => panic!("unexpected error: {error}"),
        };
        let newer = toml::to_string_pretty(&Config::defaults(false)).unwrap();
        fs::write(&path, &newer).unwrap();

        let outcome =
            Config::backup_and_replace_default_if_unchanged(&path, &revision, true).unwrap();

        assert_eq!(outcome, ReplaceConfigOutcome::Changed);
        assert_eq!(fs::read_to_string(path).unwrap(), newer);
    }

    #[test]
    fn serializes_concurrent_configuration_replacements() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = \"old\"\n").unwrap();
        let revision = match Config::load(&path).unwrap_err() {
            ConfigError::UnsupportedSchema {
                revision: Some(revision),
                ..
            } => revision,
            error => panic!("unexpected error: {error}"),
        };
        let barrier = Arc::new(Barrier::new(3));
        let mut replacements = Vec::new();
        for record_prompt in [false, true] {
            let path = path.clone();
            let revision = revision.clone();
            let barrier = barrier.clone();
            replacements.push(thread::spawn(move || {
                barrier.wait();
                Config::backup_and_replace_default_if_unchanged(&path, &revision, record_prompt)
                    .unwrap()
            }));
        }
        barrier.wait();

        let outcomes: Vec<_> = replacements
            .into_iter()
            .map(|replacement| replacement.join().unwrap())
            .collect();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ReplaceConfigOutcome::Replaced { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ReplaceConfigOutcome::Changed))
                .count(),
            1
        );
        let replaced = outcomes
            .iter()
            .find_map(|outcome| match outcome {
                ReplaceConfigOutcome::Replaced { config, .. } => Some(config),
                ReplaceConfigOutcome::Changed => None,
            })
            .unwrap();
        assert_eq!(Config::load(&path).unwrap(), *replaced);
        assert_eq!(
            fs::metadata(config_lock_path(&path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn rejects_duplicate_profile_names_and_identities() {
        let mut config = Config::defaults(false);
        config.profiles.push(config.profiles[0].clone());
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateProfileName(_))
        ));
        let mut config = Config::defaults(false);
        let mut duplicate = config.profiles[0].clone();
        duplicate.name = "Renamed".to_owned();
        config.profiles.push(duplicate);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateProfileIdentity(_))
        ));

        let mut config = Config::defaults(false);
        config.profiles[0].name = crate::safe_test_declaration().name;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::ReservedSafeTestName(_))
        ));
    }

    #[test]
    fn rejects_invalid_current_schema_configurations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let cases = [
            (
                "unknown adapter",
                format!(
                    "schema_version = {CONFIG_SCHEMA_VERSION:?}\nrecord_prompt = false\ntag_candidates = []\n[[profiles]]\nname = \"Unknown\"\ncli = \"other\"\n"
                ),
            ),
            (
                "missing recording fields",
                format!(
                    "schema_version = {CONFIG_SCHEMA_VERSION:?}\n[[profiles]]\nname = \"Codex\"\ncli = \"codex\"\n"
                ),
            ),
        ];

        for (case, source) in cases {
            fs::write(&path, source).unwrap();
            assert!(
                matches!(Config::load(&path), Err(ConfigError::Parse { .. })),
                "case={case}"
            );
        }
    }

    #[test]
    fn rejects_and_redacts_configuration_from_another_application_version() {
        let mut config = Config::defaults(false);
        config.schema_version = "other".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnsupportedSchema { .. })
        ));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = 1\nsecret = \"do-not-print\"\n").unwrap();

        let error = Config::load(&path).unwrap_err();

        assert!(matches!(error, ConfigError::UnsupportedSchema { .. }));
        assert!(!format!("{error:?}").contains("do-not-print"));
    }
}

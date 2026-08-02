use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{AdapterKind, ExecutionProfile};

pub const EVENT_SCHEMA_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: String,
    pub event_id: Uuid,
    pub task_id: Uuid,
    pub occurred_at: Timestamp,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(task_id: Uuid, kind: EventKind) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: Uuid::now_v7(),
            task_id,
            occurred_at: Timestamp::now(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    TaskReceived {
        tags: Vec<String>,
        prompt: Option<String>,
        prompt_chars: usize,
    },
    Assigned {
        profile_id: String,
    },
    Executed {
        profile: ProfileSnapshot,
        working_directory: PathBuf,
        started_at: Timestamp,
        elapsed_ms: u64,
        outcome: RecordedExecutionOutcome,
        cost: Option<()>,
    },
    Judged {
        execution_event_id: Uuid,
        verdict: Verdict,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub id: String,
    pub name: String,
    pub adapter: AdapterKind,
    pub executable: PathBuf,
    pub model: Option<String>,
    pub args: Vec<String>,
}

impl From<&ExecutionProfile> for ProfileSnapshot {
    fn from(profile: &ExecutionProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            adapter: profile.adapter,
            executable: profile.executable(),
            model: profile.model.clone(),
            args: profile.args.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecordedExecutionOutcome {
    Completed {
        exit_code: Option<i32>,
    },
    Cancelled,
    Failed {
        failure_kind: FailureKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Spawn,
    ProcessSetup,
    Io,
    Signal,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedLine {
    pub line_number: usize,
    pub reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReadEvents {
    pub events: Vec<Event>,
    pub skipped_lines: Vec<SkippedLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: &Event) -> Result<(), RecordError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| RecordError::MissingParent(self.path.clone()))?;
        fs::create_dir_all(parent).map_err(|source| RecordError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|source| RecordError::Open {
                path: self.path.clone(),
                source,
            })?;
        let _lock = FileLock::acquire(&file, libc::LOCK_EX, &self.path)?;
        let mut encoded = serde_json::to_vec(event).map_err(RecordError::Serialize)?;
        encoded.push(b'\n');
        file.write_all(&encoded)
            .and_then(|()| file.sync_data())
            .map_err(|source| RecordError::Write {
                path: self.path.clone(),
                source,
            })
    }

    pub fn read_all(&self) -> Result<ReadEvents, RecordError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReadEvents::default());
            }
            Err(source) => {
                return Err(RecordError::Open {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let _lock = FileLock::acquire(&file, libc::LOCK_SH, &self.path)?;
        let mut result = ReadEvents::default();
        for (index, line) in BufReader::new(&file).lines().enumerate() {
            let line_number = index + 1;
            let line = line.map_err(|source| RecordError::Read {
                path: self.path.clone(),
                source,
            })?;
            match serde_json::from_str::<Event>(&line) {
                Ok(event) if event.schema_version == EVENT_SCHEMA_VERSION => {
                    result.events.push(event);
                }
                Ok(event) => result.skipped_lines.push(SkippedLine {
                    line_number,
                    reason: format!(
                        "unsupported schema version {}; expected {}",
                        event.schema_version, EVENT_SCHEMA_VERSION
                    ),
                }),
                Err(source) => result.skipped_lines.push(SkippedLine {
                    line_number,
                    reason: source.to_string(),
                }),
            }
        }
        Ok(result)
    }
}

pub fn default_events_path() -> Result<PathBuf, RecordError> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(RecordError::NoDataDirectory)?;
    Ok(home
        .join("Library")
        .join("Application Support")
        .join("agent-dock")
        .join("events.jsonl"))
}

struct FileLock {
    file_descriptor: RawFd,
}

impl FileLock {
    fn acquire(file: &File, operation: libc::c_int, path: &Path) -> Result<Self, RecordError> {
        // SAFETY: flock receives an open file descriptor and a supported operation.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            Ok(Self {
                file_descriptor: file.as_raw_fd(),
            })
        } else {
            Err(RecordError::Lock {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            })
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: callers keep the file open until after the guard is dropped.
        unsafe {
            libc::flock(self.file_descriptor, libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Error)]
pub enum RecordError {
    #[error("could not determine the event data directory")]
    NoDataDirectory,
    #[error("event log path `{0}` has no parent directory")]
    MissingParent(PathBuf),
    #[error("could not create event log directory `{path}`: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not open event log `{path}`: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not lock event log `{path}`: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not read event log `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write event log `{path}`: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not serialize an event: {0}")]
    Serialize(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, sync::Arc, thread};

    use super::*;

    fn task_event(task_id: Uuid, prompt: Option<&str>) -> Event {
        Event::new(
            task_id,
            EventKind::TaskReceived {
                tags: vec!["rust".to_owned()],
                prompt: prompt.map(str::to_owned),
                prompt_chars: prompt.map_or(0, |value| value.chars().count()),
            },
        )
    }

    #[test]
    fn serializes_the_versioned_adjacent_tag_shape() {
        let event = task_event(Uuid::now_v7(), None);
        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
        assert_eq!(value["kind"], "task_received");
        assert!(value["data"]["prompt"].is_null());
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
    }

    #[test]
    fn round_trips_every_event_and_execution_outcome() {
        let task_id = Uuid::now_v7();
        let profile = ProfileSnapshot {
            id: "codex-test".to_owned(),
            name: "Codex test".to_owned(),
            adapter: AdapterKind::Codex,
            executable: PathBuf::from("/usr/local/bin/codex"),
            model: Some("test-model".to_owned()),
            args: vec!["--safe-option".to_owned()],
        };
        let executed = [
            RecordedExecutionOutcome::Completed { exit_code: Some(7) },
            RecordedExecutionOutcome::Completed { exit_code: None },
            RecordedExecutionOutcome::Cancelled,
            RecordedExecutionOutcome::Failed {
                failure_kind: FailureKind::Spawn,
                message: "could not start process".to_owned(),
            },
        ]
        .into_iter()
        .map(|outcome| {
            Event::new(
                task_id,
                EventKind::Executed {
                    profile: profile.clone(),
                    working_directory: PathBuf::from("/work"),
                    started_at: Timestamp::now(),
                    elapsed_ms: 42,
                    outcome,
                    cost: None,
                },
            )
        });
        let events: Vec<_> = [
            Event::new(
                task_id,
                EventKind::Assigned {
                    profile_id: profile.id.clone(),
                },
            ),
            Event::new(
                task_id,
                EventKind::Judged {
                    execution_event_id: Uuid::now_v7(),
                    verdict: Verdict::Rejected,
                },
            ),
        ]
        .into_iter()
        .chain(executed)
        .collect();

        for event in events {
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
        }
    }

    #[test]
    fn appends_reads_and_skips_bad_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let event = task_event(Uuid::now_v7(), Some("こんにちは"));

        log.append(&event).unwrap();
        fs::write(
            &path,
            format!("{}\nnot-json\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        let read = log.read_all().unwrap();

        assert_eq!(read.events, vec![event]);
        assert_eq!(read.skipped_lines.len(), 1);
        assert_eq!(read.skipped_lines[0].line_number, 2);
    }

    #[test]
    fn skips_events_from_another_application_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let mut event = task_event(Uuid::now_v7(), None);
        event.schema_version = "0.0.0".to_owned();
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();

        let read = EventLog::new(path).read_all().unwrap();

        assert!(read.events.is_empty());
        assert_eq!(read.skipped_lines.len(), 1);
        assert!(read.skipped_lines[0].reason.contains("0.0.0"));
    }

    #[test]
    fn creates_a_private_event_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        EventLog::new(&path)
            .append(&task_event(Uuid::now_v7(), None))
            .unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn serializes_concurrent_writers() {
        let directory = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::new(directory.path().join("events.jsonl")));
        let task_id = Uuid::now_v7();
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let log = log.clone();
                thread::spawn(move || log.append(&task_event(task_id, None)).unwrap())
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        assert_eq!(log.read_all().unwrap().events.len(), 8);
    }
}

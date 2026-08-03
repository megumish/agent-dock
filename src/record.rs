use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{CliKind, ExecutionProfile, ProfileDeclaration};

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
    pub cli: CliKind,
    pub executable_override: Option<PathBuf>,
    pub model: Option<String>,
    pub args: Vec<String>,
    pub resolved_executable: PathBuf,
}

impl From<&ExecutionProfile> for ProfileSnapshot {
    fn from(profile: &ExecutionProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name().to_owned(),
            cli: profile.cli(),
            executable_override: profile.declaration.executable.clone(),
            model: profile.declaration.model.clone(),
            args: profile.declaration.args.clone(),
            resolved_executable: profile.resolved_executable.clone(),
        }
    }
}

impl ProfileSnapshot {
    pub fn declaration(&self) -> ProfileDeclaration {
        ProfileDeclaration {
            name: self.name.clone(),
            cli: self.cli,
            executable: self.executable_override.clone(),
            model: self.model.clone(),
            args: self.args.clone(),
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
    pub reason: SkippedLineReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkippedLineReason {
    #[error("unsupported schema version {found}; expected {expected}")]
    UnsupportedSchema {
        found: String,
        expected: &'static str,
    },
    #[error("{message}")]
    InvalidEvent { message: String },
}

#[derive(Debug, Default, Clone)]
pub struct ReadEvents {
    pub events: Vec<Event>,
    pub skipped_lines: Vec<SkippedLine>,
    revision: Option<EventLogRevision>,
}

impl PartialEq for ReadEvents {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events && self.skipped_lines == other.skipped_lines
    }
}

impl Eq for ReadEvents {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventLogRevision {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl From<&fs::Metadata> for EventLogRevision {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupEventLogOutcome {
    BackedUpAndDeleted { backup_path: PathBuf },
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendJudgementOutcome {
    Appended,
    CurrentVerdictChanged { current: Option<Verdict> },
    ExecutionMissing,
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

    pub fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    pub fn append(&self, event: &Event) -> Result<(), RecordError> {
        let _lock = FileLock::acquire(self.open_lock_file()?, libc::LOCK_EX, &self.lock_path())?;
        self.append_unlocked(event)
    }

    pub fn append_judgement_if_current(
        &self,
        execution_event_id: Uuid,
        expected_current: Option<Verdict>,
        verdict: Verdict,
    ) -> Result<AppendJudgementOutcome, RecordError> {
        let _lock = FileLock::acquire(self.open_lock_file()?, libc::LOCK_EX, &self.lock_path())?;
        let history = self.read_all_unlocked()?;
        let Some(task_id) = history.events.iter().find_map(|event| {
            (event.event_id == execution_event_id
                && matches!(event.kind, EventKind::Executed { .. }))
            .then_some(event.task_id)
        }) else {
            return Ok(AppendJudgementOutcome::ExecutionMissing);
        };
        let current = history.events.iter().fold(None, |current, event| {
            if let EventKind::Judged {
                execution_event_id: target,
                verdict,
            } = event.kind
                && target == execution_event_id
            {
                Some(verdict)
            } else {
                current
            }
        });
        if current != expected_current {
            return Ok(AppendJudgementOutcome::CurrentVerdictChanged { current });
        }
        self.append_unlocked(&Event::new(
            task_id,
            EventKind::Judged {
                execution_event_id,
                verdict,
            },
        ))?;
        Ok(AppendJudgementOutcome::Appended)
    }

    fn append_unlocked(&self, event: &Event) -> Result<(), RecordError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|source| RecordError::Open {
                path: self.path.clone(),
                source,
            })?;
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
        let _lock = FileLock::acquire(self.open_lock_file()?, libc::LOCK_SH, &self.lock_path())?;
        self.read_all_unlocked()
    }

    fn read_all_unlocked(&self) -> Result<ReadEvents, RecordError> {
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
        let revision =
            EventLogRevision::from(&file.metadata().map_err(|source| RecordError::Read {
                path: self.path.clone(),
                source,
            })?);
        let mut result = ReadEvents {
            revision: Some(revision),
            ..ReadEvents::default()
        };
        for (index, line) in BufReader::new(&file).lines().enumerate() {
            let line_number = index + 1;
            let line = line.map_err(|source| RecordError::Read {
                path: self.path.clone(),
                source,
            })?;
            match decode_event_line(&line) {
                Ok(event) => result.events.push(event),
                Err(reason) => result.skipped_lines.push(SkippedLine {
                    line_number,
                    reason,
                }),
            }
        }
        Ok(result)
    }

    pub fn backup_and_delete_if_unchanged(
        &self,
        expected: &ReadEvents,
    ) -> Result<BackupEventLogOutcome, RecordError> {
        let _lock = FileLock::acquire(self.open_lock_file()?, libc::LOCK_EX, &self.lock_path())?;
        let current_revision = match fs::metadata(&self.path) {
            Ok(metadata) => Some(EventLogRevision::from(&metadata)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(RecordError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if current_revision != expected.revision {
            return Ok(BackupEventLogOutcome::Changed);
        }
        if current_revision.is_none() {
            return Ok(BackupEventLogOutcome::Changed);
        }
        let backup_path = self.backup_path();
        let mut source = File::open(&self.path).map_err(|source| RecordError::Open {
            path: self.path.clone(),
            source,
        })?;
        let mut backup = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup_path)
            .map_err(|source| RecordError::CreateBackup {
                path: backup_path.clone(),
                source,
            })?;
        if let Err(source) = std::io::copy(&mut source, &mut backup).and_then(|_| backup.sync_all())
        {
            let _ = fs::remove_file(&backup_path);
            return Err(RecordError::Write {
                path: backup_path,
                source,
            });
        }
        let revision_after_backup = match fs::metadata(&self.path) {
            Ok(metadata) => Some(EventLogRevision::from(&metadata)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                let _ = fs::remove_file(&backup_path);
                return Err(RecordError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if revision_after_backup != expected.revision {
            let _ = fs::remove_file(&backup_path);
            return Ok(BackupEventLogOutcome::Changed);
        }
        fs::remove_file(&self.path).map_err(|source| RecordError::Delete {
            path: self.path.clone(),
            source,
        })?;
        Ok(BackupEventLogOutcome::BackedUpAndDeleted { backup_path })
    }

    fn open_lock_file(&self) -> Result<File, RecordError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| RecordError::MissingParent(self.path.clone()))?;
        fs::create_dir_all(parent).map_err(|source| RecordError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let path = self.lock_path();
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| RecordError::OpenLock { path, source })
    }

    fn backup_path(&self) -> PathBuf {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("events.jsonl");
        parent.join(format!("{file_name}.backup.{}", Uuid::now_v7()))
    }
}

fn decode_event_line(line: &str) -> Result<Event, SkippedLineReason> {
    let document = serde_json::from_str::<serde_json::Value>(line).map_err(|source| {
        SkippedLineReason::InvalidEvent {
            message: source.to_string(),
        }
    })?;
    if let Some(found) = document
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .filter(|found| *found != EVENT_SCHEMA_VERSION)
    {
        return Err(SkippedLineReason::UnsupportedSchema {
            found: found.to_owned(),
            expected: EVENT_SCHEMA_VERSION,
        });
    }
    serde_json::from_value(document).map_err(|source| SkippedLineReason::InvalidEvent {
        message: source.to_string(),
    })
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
    file: File,
}

impl FileLock {
    fn acquire(file: File, operation: libc::c_int, path: &Path) -> Result<Self, RecordError> {
        // SAFETY: flock receives an open file descriptor and a supported operation.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            Ok(Self { file })
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
        // SAFETY: the guard owns the open file until after flock returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
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
    #[error("could not open event log lock `{path}`: {source}")]
    OpenLock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not create event log backup `{path}`: {source}")]
    CreateBackup {
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
    #[error("could not delete event log `{path}`: {source}")]
    Delete {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not serialize an event: {0}")]
    Serialize(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier},
        thread,
    };

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

    fn completed_execution(task_id: Uuid) -> Event {
        Event::new(
            task_id,
            EventKind::Executed {
                profile: ProfileSnapshot {
                    id: "test-default".to_owned(),
                    name: "Safe test".to_owned(),
                    cli: CliKind::Test,
                    executable_override: None,
                    model: None,
                    args: Vec::new(),
                    resolved_executable: PathBuf::from("/usr/bin/true"),
                },
                working_directory: PathBuf::from("/tmp/project"),
                started_at: Timestamp::now(),
                elapsed_ms: 10,
                outcome: RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                cost: None,
            },
        )
    }

    #[test]
    fn conditionally_appends_a_judgement_while_preserving_the_log_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let execution = completed_execution(Uuid::now_v7());
        let prior = Event::new(
            execution.task_id,
            EventKind::Judged {
                execution_event_id: execution.event_id,
                verdict: Verdict::Rejected,
            },
        );
        log.append(&execution).unwrap();
        log.append(&prior).unwrap();
        let original = fs::read(&path).unwrap();

        assert_eq!(
            log.append_judgement_if_current(
                execution.event_id,
                Some(Verdict::Rejected),
                Verdict::Accepted,
            )
            .unwrap(),
            AppendJudgementOutcome::Appended
        );

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(&original));
        let events = log.read_all().unwrap().events;
        assert_eq!(&events[..2], &[execution.clone(), prior]);
        assert!(matches!(
            events[2].kind,
            EventKind::Judged {
                execution_event_id,
                verdict: Verdict::Accepted,
            } if execution_event_id == execution.event_id
        ));
    }

    #[test]
    fn conditional_judgement_reports_stale_or_missing_targets_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        assert_eq!(
            log.append_judgement_if_current(Uuid::now_v7(), None, Verdict::Accepted)
                .unwrap(),
            AppendJudgementOutcome::ExecutionMissing
        );
        let execution = completed_execution(Uuid::now_v7());
        log.append(&execution).unwrap();
        log.append(&Event::new(
            execution.task_id,
            EventKind::Judged {
                execution_event_id: execution.event_id,
                verdict: Verdict::Accepted,
            },
        ))
        .unwrap();
        let original = fs::read(&path).unwrap();

        assert_eq!(
            log.append_judgement_if_current(execution.event_id, None, Verdict::Rejected)
                .unwrap(),
            AppendJudgementOutcome::CurrentVerdictChanged {
                current: Some(Verdict::Accepted)
            }
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn only_one_concurrent_judgement_can_match_the_same_expected_verdict() {
        let directory = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::new(directory.path().join("events.jsonl")));
        let execution = completed_execution(Uuid::now_v7());
        log.append(&execution).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for verdict in [Verdict::Accepted, Verdict::Rejected] {
            let log = log.clone();
            let barrier = barrier.clone();
            let execution_event_id = execution.event_id;
            handles.push(thread::spawn(move || {
                barrier.wait();
                log.append_judgement_if_current(execution_event_id, None, verdict)
                    .unwrap()
            }));
        }
        barrier.wait();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == AppendJudgementOutcome::Appended)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome,
                        AppendJudgementOutcome::CurrentVerdictChanged { current: Some(_) }
                    )
                })
                .count(),
            1
        );
        assert_eq!(log.read_all().unwrap().events.len(), 2);
    }

    #[test]
    fn serializes_the_task_event_shape_and_prompt_states() {
        let unrecorded = task_event(Uuid::now_v7(), None);
        let empty = task_event(Uuid::now_v7(), Some(""));
        let unrecorded_value = serde_json::to_value(&unrecorded).unwrap();
        let empty_value = serde_json::to_value(&empty).unwrap();

        assert_eq!(unrecorded_value["schema_version"], EVENT_SCHEMA_VERSION);
        assert_eq!(unrecorded_value["kind"], "task_received");
        assert!(unrecorded_value["data"]["prompt"].is_null());
        assert_eq!(empty_value["data"]["prompt"], "");
        assert_eq!(
            serde_json::from_value::<Event>(unrecorded_value).unwrap(),
            unrecorded
        );
        assert_eq!(serde_json::from_value::<Event>(empty_value).unwrap(), empty);
    }

    #[test]
    fn round_trips_every_event_and_execution_outcome() {
        let task_id = Uuid::now_v7();
        let profile = ProfileSnapshot {
            id: "codex-test".to_owned(),
            name: "Codex test".to_owned(),
            cli: CliKind::Codex,
            executable_override: None,
            model: Some("test-model".to_owned()),
            args: vec!["--safe-option".to_owned()],
            resolved_executable: PathBuf::from("/usr/local/bin/codex"),
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
        assert!(matches!(
            &read.skipped_lines[0].reason,
            SkippedLineReason::InvalidEvent { .. }
        ));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(log.lock_path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn classifies_event_lines_by_schema() {
        let mut event = task_event(Uuid::now_v7(), None);
        event.schema_version = "0.0.0".to_owned();
        let old_event = serde_json::to_string(&event).unwrap();
        for (line, expected) in [
            (old_event.as_str(), "0.0.0"),
            ("{\"schema_version\":\"old\"}", "old"),
        ] {
            assert!(
                matches!(
                    decode_event_line(line),
                    Err(SkippedLineReason::UnsupportedSchema { found, .. }) if found == expected
                ),
                "line={line:?}"
            );
        }
        for line in [
            "{}",
            "{\"schema_version\":null}",
            "{\"schema_version\":1}",
            "{\"schema_version\":[]}",
            "{\"schema_version\":{}}",
            "[]",
            "null",
        ] {
            assert!(
                matches!(
                    decode_event_line(line),
                    Err(SkippedLineReason::InvalidEvent { .. })
                ),
                "line={line:?}"
            );
        }
    }

    #[test]
    fn backs_up_and_deletes_only_the_event_log_revision_that_was_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let event = task_event(Uuid::now_v7(), None);
        log.append(&event).unwrap();
        let original = fs::read(&path).unwrap();
        let read = log.read_all().unwrap();

        let BackupEventLogOutcome::BackedUpAndDeleted { backup_path } =
            log.backup_and_delete_if_unchanged(&read).unwrap()
        else {
            panic!("event log was not backed up");
        };
        assert!(!path.exists());
        assert_eq!(fs::read(&backup_path).unwrap(), original);
        assert_eq!(
            fs::metadata(backup_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn preserves_events_appended_after_the_log_was_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let first = task_event(Uuid::now_v7(), None);
        let second = task_event(Uuid::now_v7(), None);
        log.append(&first).unwrap();
        let stale = log.read_all().unwrap();
        log.append(&second).unwrap();

        assert_eq!(
            log.backup_and_delete_if_unchanged(&stale).unwrap(),
            BackupEventLogOutcome::Changed
        );
        assert_eq!(log.read_all().unwrap().events, [first, second]);
    }

    #[test]
    fn does_not_lose_an_event_when_append_and_delete_start_concurrently() {
        for _ in 0..32 {
            let directory = tempfile::tempdir().unwrap();
            let log = Arc::new(EventLog::new(directory.path().join("events.jsonl")));
            let original = task_event(Uuid::now_v7(), None);
            let appended = task_event(Uuid::now_v7(), None);
            log.append(&original).unwrap();
            let stale = log.read_all().unwrap();
            let barrier = Arc::new(Barrier::new(3));

            let deleting_log = log.clone();
            let deleting_barrier = barrier.clone();
            let deleting = thread::spawn(move || {
                deleting_barrier.wait();
                deleting_log.backup_and_delete_if_unchanged(&stale).unwrap()
            });
            let appending_log = log.clone();
            let appending_barrier = barrier.clone();
            let appended_for_thread = appended.clone();
            let appending = thread::spawn(move || {
                appending_barrier.wait();
                appending_log.append(&appended_for_thread).unwrap();
            });
            barrier.wait();

            let delete_outcome = deleting.join().unwrap();
            appending.join().unwrap();
            let events = log.read_all().unwrap().events;

            assert!(events.contains(&appended));
            match delete_outcome {
                BackupEventLogOutcome::BackedUpAndDeleted { backup_path } => {
                    assert_eq!(events, [appended]);
                    assert!(backup_path.exists());
                }
                BackupEventLogOutcome::Changed => assert_eq!(events, [original, appended]),
            }
            assert!(log.lock_path().exists());
        }
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

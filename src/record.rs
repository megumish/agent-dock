use std::{
    collections::BTreeMap,
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

use crate::{CliKind, ExecutionPlatform, ExecutionProfile, ProfileDeclaration};

pub const EVENT_FORMAT_VERSION: &str = "agent-dock/events/v1alpha4";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub format_version: String,
    pub event_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    pub occurred_at: Timestamp,
    pub provenance: Provenance,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(task_id: Uuid, kind: EventKind) -> Self {
        Self::for_task(task_id, Provenance::Dock, kind)
    }

    pub fn for_task(task_id: Uuid, provenance: Provenance, kind: EventKind) -> Self {
        Self {
            format_version: EVENT_FORMAT_VERSION.to_owned(),
            event_id: Uuid::now_v7(),
            task_id: Some(task_id),
            occurred_at: Timestamp::now(),
            provenance,
            kind,
        }
    }

    pub fn for_session(provenance: Provenance, kind: EventKind) -> Self {
        Self {
            format_version: EVENT_FORMAT_VERSION.to_owned(),
            event_id: Uuid::now_v7(),
            task_id: None,
            occurred_at: Timestamp::now(),
            provenance,
            kind,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Dock,
    ManualMark,
    CliHook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOrigin {
    Brokered,
    DirectObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum EvidenceSegment {
    Overall,
    Untagged,
    Tag(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceAxisSummary {
    pub count: u64,
    pub latest_started_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceAcceptance {
    pub summary: EvidenceAxisSummary,
    pub accepted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceDuration {
    pub summary: EvidenceAxisSummary,
    pub median_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCost {
    pub summary: EvidenceAxisSummary,
    pub currency: Option<String>,
    pub total_minor_units: Option<u64>,
    pub missing_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceEvidence {
    pub segment: EvidenceSegment,
    pub acceptance: EvidenceAcceptance,
    pub duration: EvidenceDuration,
    pub cost: EvidenceCost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceSegmentCount {
    pub segment: EvidenceSegment,
    pub max_judged: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AdviceReason {
    NoEvidence { max_judged: Vec<AdviceSegmentCount> },
    InsufficientEvidence { max_judged: Vec<AdviceSegmentCount> },
    NoUniqueLeader { profile_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum AdviceOutcome {
    Proposed {
        profile_id: String,
        evidence: Vec<AdviceEvidence>,
    },
    Abstained {
        reason: AdviceReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    TaskReceived {
        tags: Vec<String>,
        prompt: Option<String>,
        prompt_chars: usize,
    },
    Advised {
        referenced_segments: Vec<EvidenceSegment>,
        outcome: AdviceOutcome,
        threshold: u64,
    },
    Approved {
        advice_event_id: Uuid,
        profile_id: String,
    },
    Declined {
        advice_event_id: Uuid,
    },
    Designated {
        advice_event_id: Uuid,
        profile_id: String,
    },
    Executed {
        origin: ExecutionOrigin,
        profile: ProfileSnapshot,
        working_directory: PathBuf,
        started_at: Timestamp,
        elapsed_ms: u64,
        outcome: RecordedExecutionOutcome,
        cost: Option<()>,
    },
    Judged {
        verdict: Verdict,
    },
    SessionObserved {
        session_id: Uuid,
        cli: CliKind,
        phase: SessionPhase,
        external_session_id: Option<String>,
        working_directory: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        configuration: Option<ObservedConfiguration>,
    },
    ObservedTaskStarted {
        session_id: Uuid,
        origin: ExecutionOrigin,
        tags: Vec<String>,
        prompt: Option<String>,
        prompt_chars: usize,
    },
    ObservedTaskEnded {
        session_id: Uuid,
        status: ObservedTaskStatus,
    },
    ConfigurationObserved {
        session_id: Uuid,
        boundary: ConfigurationBoundary,
        configuration: ObservedConfiguration,
    },
    ProfileAttributed {
        session_id: Uuid,
        attribution: ProfileAttribution,
    },
    MeasurementObserved {
        session_id: Uuid,
        elapsed_ms: Option<u64>,
        actual_cost: Option<ActualCost>,
    },
    ObservationAnomaly {
        session_id: Option<Uuid>,
        anomaly: ObservationAnomaly,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Started,
    Resumed,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedTaskStatus {
    Completed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationBoundary {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum ObservedValue<T> {
    Observed(T),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedConfiguration {
    pub cli: CliKind,
    pub execution_platform: ExecutionPlatform,
    pub model: ObservedValue<Option<String>>,
    pub identity: BTreeMap<String, ObservedValue<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProfileAttribution {
    Matched { profile: ProfileSnapshot },
    Unattributed { reason: AttributionFailure },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionFailure {
    MissingConfiguration,
    ConfigurationChanged,
    NoMatchingProfile,
    AmbiguousProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActualCost {
    pub currency: String,
    pub minor_units: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationAnomaly {
    OrphanTaskEnd,
    SessionEndMissing,
    UnsupportedHook { provider: String, event: String },
    InvalidHook { provider: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub id: String,
    pub name: String,
    pub cli: CliKind,
    pub execution_platform: ExecutionPlatform,
    pub executable_override: Option<PathBuf>,
    pub model: Option<String>,
    pub args: Vec<String>,
    pub identity: BTreeMap<String, String>,
    pub resolved_executable: Option<PathBuf>,
}

impl From<&ExecutionProfile> for ProfileSnapshot {
    fn from(profile: &ExecutionProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name().to_owned(),
            cli: profile.cli(),
            execution_platform: profile.execution_platform(),
            executable_override: profile.declaration.executable.clone(),
            model: profile.declaration.model.clone(),
            args: profile.declaration.args.clone(),
            identity: profile.declaration.identity.clone(),
            resolved_executable: Some(profile.resolved_executable.clone()),
        }
    }
}

impl ProfileSnapshot {
    pub fn declaration(&self) -> ProfileDeclaration {
        ProfileDeclaration {
            name: self.name.clone(),
            cli: self.cli,
            execution_platform: self.execution_platform,
            executable: self.executable_override.clone(),
            model: self.model.clone(),
            args: self.args.clone(),
            identity: self.identity.clone(),
        }
    }

    pub fn observed(
        declaration: &ProfileDeclaration,
    ) -> Result<Self, crate::adapter::ProfileValidationError> {
        Ok(Self {
            id: declaration.id()?,
            name: declaration.name.clone(),
            cli: declaration.cli,
            execution_platform: declaration.execution_platform,
            executable_override: declaration.executable.clone(),
            model: declaration.model.clone(),
            args: declaration.args.clone(),
            identity: declaration.identity.clone(),
            resolved_executable: None,
        })
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
    #[error("unsupported event format {found}; expected {expected}")]
    UnsupportedFormat {
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
pub enum AppendBatchOutcome {
    Appended,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendJudgementOutcome {
    Appended,
    CurrentVerdictChanged { current: Option<Verdict> },
    TaskMissing,
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

    pub fn append_batch_if_unchanged(
        &self,
        expected: &ReadEvents,
        events: &[Event],
    ) -> Result<AppendBatchOutcome, RecordError> {
        let mut encoded = Vec::new();
        for event in events {
            serde_json::to_writer(&mut encoded, event).map_err(RecordError::Serialize)?;
            encoded.push(b'\n');
        }
        let _lock = FileLock::acquire(self.open_lock_file()?, libc::LOCK_EX, &self.lock_path())?;
        let current = self.read_all_unlocked()?;
        if current.revision != expected.revision {
            return Ok(AppendBatchOutcome::Changed);
        }
        if encoded.is_empty() {
            return Ok(AppendBatchOutcome::Appended);
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| RecordError::MissingParent(self.path.clone()))?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("events.jsonl");
        let temporary_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::now_v7()));
        let mut replacement = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(RecordError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        replacement.extend(encoded);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)
            .map_err(|source| RecordError::Open {
                path: temporary_path.clone(),
                source,
            })?;
        let result = file
            .write_all(&replacement)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary_path, &self.path));
        if let Err(source) = result {
            let _ = fs::remove_file(&temporary_path);
            return Err(RecordError::Write {
                path: self.path.clone(),
                source,
            });
        }
        Ok(AppendBatchOutcome::Appended)
    }

    pub fn append_judgement_if_current(
        &self,
        task_id: Uuid,
        expected_current: Option<Verdict>,
        verdict: Verdict,
    ) -> Result<AppendJudgementOutcome, RecordError> {
        let _lock = FileLock::acquire(self.open_lock_file()?, libc::LOCK_EX, &self.lock_path())?;
        let history = self.read_all_unlocked()?;
        let task_exists = history.events.iter().any(|event| {
            event.task_id == Some(task_id)
                && matches!(
                    event.kind,
                    EventKind::Executed {
                        outcome: RecordedExecutionOutcome::Completed { .. }
                            | RecordedExecutionOutcome::Cancelled,
                        ..
                    } | EventKind::ObservedTaskEnded {
                        status: ObservedTaskStatus::Completed,
                        ..
                    }
                )
        });
        if !task_exists {
            return Ok(AppendJudgementOutcome::TaskMissing);
        }
        let current = history.events.iter().fold(None, |current, event| {
            if let EventKind::Judged { verdict } = event.kind
                && event.task_id == Some(task_id)
            {
                Some(verdict)
            } else {
                current
            }
        });
        if current != expected_current {
            return Ok(AppendJudgementOutcome::CurrentVerdictChanged { current });
        }
        self.append_unlocked(&Event::new(task_id, EventKind::Judged { verdict }))?;
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
    let found = document
        .get("format_version")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| {
            document
                .get("schema_version")
                .map(|value| format!("legacy schema_version {}", value))
                .unwrap_or_else(|| "missing".to_owned())
        });
    if found != EVENT_FORMAT_VERSION {
        return Err(SkippedLineReason::UnsupportedFormat {
            found,
            expected: EVENT_FORMAT_VERSION,
        });
    }
    let event: Event =
        serde_json::from_value(document).map_err(|source| SkippedLineReason::InvalidEvent {
            message: source.to_string(),
        })?;
    let requires_task = !matches!(
        event.kind,
        EventKind::SessionObserved { .. } | EventKind::ObservationAnomaly { .. }
    );
    if requires_task != event.task_id.is_some() {
        return Err(SkippedLineReason::InvalidEvent {
            message: if requires_task {
                "task event is missing task_id".to_owned()
            } else {
                "session event must not contain task_id".to_owned()
            },
        });
    }
    let semantics_valid = match &event.kind {
        EventKind::ObservedTaskStarted { origin, .. } => {
            *origin == ExecutionOrigin::DirectObservation
                && event.provenance == Provenance::ManualMark
        }
        EventKind::ObservedTaskEnded { .. } => event.provenance == Provenance::ManualMark,
        EventKind::ConfigurationObserved { .. } => event.provenance == Provenance::ManualMark,
        EventKind::Executed { origin, .. } => *origin == ExecutionOrigin::Brokered,
        _ => true,
    };
    if !semantics_valid {
        return Err(SkippedLineReason::InvalidEvent {
            message: "event provenance or execution origin is inconsistent with its kind"
                .to_owned(),
        });
    }
    Ok(event)
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
                    execution_platform: ExecutionPlatform::Headless,
                    executable_override: None,
                    model: None,
                    args: Vec::new(),
                    identity: Default::default(),
                    resolved_executable: Some(PathBuf::from("/usr/bin/true")),
                },
                origin: ExecutionOrigin::Brokered,
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
            execution.task_id.unwrap(),
            EventKind::Judged {
                verdict: Verdict::Rejected,
            },
        );
        log.append(&execution).unwrap();
        log.append(&prior).unwrap();
        let original = fs::read(&path).unwrap();

        assert_eq!(
            log.append_judgement_if_current(
                execution.task_id.unwrap(),
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
        assert_eq!(events[2].task_id, execution.task_id);
        assert!(matches!(
            events[2].kind,
            EventKind::Judged {
                verdict: Verdict::Accepted
            }
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
            AppendJudgementOutcome::TaskMissing
        );
        let execution = completed_execution(Uuid::now_v7());
        log.append(&execution).unwrap();
        log.append(&Event::new(
            execution.task_id.unwrap(),
            EventKind::Judged {
                verdict: Verdict::Accepted,
            },
        ))
        .unwrap();
        let original = fs::read(&path).unwrap();

        assert_eq!(
            log.append_judgement_if_current(execution.task_id.unwrap(), None, Verdict::Rejected)
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
            let task_id = execution.task_id.unwrap();
            handles.push(thread::spawn(move || {
                barrier.wait();
                log.append_judgement_if_current(task_id, None, verdict)
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
    fn serializes_the_versioned_adjacent_tag_shape() {
        let event = task_event(Uuid::now_v7(), None);
        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["format_version"], EVENT_FORMAT_VERSION);
        assert_eq!(value["kind"], "task_received");
        assert!(value["data"]["prompt"].is_null());
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
    }

    #[test]
    fn distinguishes_an_unrecorded_prompt_from_an_empty_prompt() {
        let unrecorded = task_event(Uuid::now_v7(), None);
        let empty = task_event(Uuid::now_v7(), Some(""));

        let unrecorded: Event =
            serde_json::from_str(&serde_json::to_string(&unrecorded).unwrap()).unwrap();
        let empty: Event = serde_json::from_str(&serde_json::to_string(&empty).unwrap()).unwrap();

        assert!(matches!(
            unrecorded.kind,
            EventKind::TaskReceived { prompt: None, .. }
        ));
        assert!(matches!(
            empty.kind,
            EventKind::TaskReceived {
                prompt: Some(prompt),
                ..
            } if prompt.is_empty()
        ));
    }

    #[test]
    fn round_trips_every_event_and_execution_outcome() {
        let task_id = Uuid::now_v7();
        let profile = ProfileSnapshot {
            id: "codex-test".to_owned(),
            name: "Codex test".to_owned(),
            cli: CliKind::Codex,
            execution_platform: ExecutionPlatform::Headless,
            executable_override: None,
            model: Some("test-model".to_owned()),
            args: vec!["--safe-option".to_owned()],
            identity: Default::default(),
            resolved_executable: Some(PathBuf::from("/usr/local/bin/codex")),
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
                    origin: ExecutionOrigin::Brokered,
                    profile: profile.clone(),
                    working_directory: PathBuf::from("/work"),
                    started_at: Timestamp::now(),
                    elapsed_ms: 42,
                    outcome,
                    cost: None,
                },
            )
        });
        let advice_event_id = Uuid::now_v7();
        let evidence = AdviceEvidence {
            segment: EvidenceSegment::Tag("rust".to_owned()),
            acceptance: EvidenceAcceptance {
                summary: EvidenceAxisSummary {
                    count: 3,
                    latest_started_at: Some(Timestamp::now()),
                },
                accepted: 2,
            },
            duration: EvidenceDuration {
                summary: EvidenceAxisSummary {
                    count: 2,
                    latest_started_at: Some(Timestamp::now()),
                },
                median_ms: None,
            },
            cost: EvidenceCost {
                summary: EvidenceAxisSummary {
                    count: 2,
                    latest_started_at: Some(Timestamp::now()),
                },
                currency: None,
                total_minor_units: None,
                missing_count: 1,
            },
        };
        let events: Vec<_> = [
            Event::new(
                task_id,
                EventKind::Advised {
                    referenced_segments: vec![EvidenceSegment::Tag("rust".to_owned())],
                    outcome: AdviceOutcome::Proposed {
                        profile_id: profile.id.clone(),
                        evidence: vec![evidence],
                    },
                    threshold: 3,
                },
            ),
            Event::new(
                task_id,
                EventKind::Approved {
                    advice_event_id,
                    profile_id: profile.id.clone(),
                },
            ),
            Event::new(task_id, EventKind::Declined { advice_event_id }),
            Event::new(
                task_id,
                EventKind::Designated {
                    advice_event_id,
                    profile_id: profile.id.clone(),
                },
            ),
            Event::new(
                task_id,
                EventKind::Judged {
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
    fn round_trips_every_advice_outcome_and_abstention_reason() {
        let task_id = Uuid::now_v7();
        let max_judged = vec![AdviceSegmentCount {
            segment: EvidenceSegment::Untagged,
            max_judged: 0,
        }];
        let some_evidence = AdviceEvidence {
            segment: EvidenceSegment::Tag("rust".to_owned()),
            acceptance: EvidenceAcceptance {
                summary: EvidenceAxisSummary {
                    count: 3,
                    latest_started_at: Some(Timestamp::now()),
                },
                accepted: 2,
            },
            duration: EvidenceDuration {
                summary: EvidenceAxisSummary {
                    count: 3,
                    latest_started_at: Some(Timestamp::now()),
                },
                median_ms: Some(1_234),
            },
            cost: EvidenceCost {
                summary: EvidenceAxisSummary {
                    count: 3,
                    latest_started_at: Some(Timestamp::now()),
                },
                currency: Some("USD".to_owned()),
                total_minor_units: Some(42),
                missing_count: 0,
            },
        };
        let none_evidence = AdviceEvidence {
            segment: EvidenceSegment::Untagged,
            acceptance: EvidenceAcceptance {
                summary: EvidenceAxisSummary {
                    count: 0,
                    latest_started_at: None,
                },
                accepted: 0,
            },
            duration: EvidenceDuration {
                summary: EvidenceAxisSummary {
                    count: 0,
                    latest_started_at: None,
                },
                median_ms: None,
            },
            cost: EvidenceCost {
                summary: EvidenceAxisSummary {
                    count: 0,
                    latest_started_at: None,
                },
                currency: None,
                total_minor_units: None,
                missing_count: 0,
            },
        };
        let outcomes = [
            AdviceOutcome::Proposed {
                profile_id: "profile-id".to_owned(),
                evidence: vec![some_evidence, none_evidence],
            },
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoEvidence {
                    max_judged: max_judged.clone(),
                },
            },
            AdviceOutcome::Abstained {
                reason: AdviceReason::InsufficientEvidence {
                    max_judged: max_judged.clone(),
                },
            },
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoUniqueLeader {
                    profile_ids: vec!["first".to_owned(), "second".to_owned()],
                },
            },
        ];
        for outcome in outcomes {
            let event = Event::new(
                task_id,
                EventKind::Advised {
                    referenced_segments: vec![EvidenceSegment::Untagged],
                    outcome,
                    threshold: 3,
                },
            );
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
    }

    #[test]
    fn skips_events_from_another_application_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let mut event = task_event(Uuid::now_v7(), None);
        event.format_version = "agent-dock/events/v0".to_owned();
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();

        let read = EventLog::new(path).read_all().unwrap();

        assert!(read.events.is_empty());
        assert_eq!(read.skipped_lines.len(), 1);
        assert!(matches!(
            &read.skipped_lines[0].reason,
            SkippedLineReason::UnsupportedFormat { found, .. }
                if found == "agent-dock/events/v0"
        ));
    }

    #[test]
    fn recognizes_an_old_schema_before_deserializing_the_event_shape() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        fs::write(&path, "{\"format_version\":\"old\"}\n").unwrap();

        let read = EventLog::new(path).read_all().unwrap();

        assert!(matches!(
            &read.skipped_lines[0].reason,
            SkippedLineReason::UnsupportedFormat { found, .. } if found == "old"
        ));
    }

    #[test]
    fn treats_missing_legacy_and_non_string_format_versions_as_unsupported() {
        for line in [
            "{}",
            "{\"schema_version\":\"0.0.1\"}",
            "{\"format_version\":null}",
            "{\"format_version\":1}",
            "{\"format_version\":[]}",
            "{\"format_version\":{}}",
            "[]",
            "null",
        ] {
            assert!(matches!(
                decode_event_line(line),
                Err(SkippedLineReason::UnsupportedFormat { .. })
            ));
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
    fn batch_append_preserves_prefix_and_commits_every_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let first = task_event(Uuid::now_v7(), None);
        let second = task_event(Uuid::now_v7(), Some("second"));
        let third = task_event(Uuid::now_v7(), Some("third"));
        log.append(&first).unwrap();
        let expected = log.read_all().unwrap();
        let prefix = fs::read(&path).unwrap();

        assert_eq!(
            log.append_batch_if_unchanged(&expected, &[second.clone(), third.clone()])
                .unwrap(),
            AppendBatchOutcome::Appended
        );

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(&prefix));
        assert_eq!(log.read_all().unwrap().events, [first, second, third]);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn stale_batch_append_changes_no_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let first = task_event(Uuid::now_v7(), None);
        let intervening = task_event(Uuid::now_v7(), Some("intervening"));
        let stale_batch = task_event(Uuid::now_v7(), Some("stale"));
        log.append(&first).unwrap();
        let stale = log.read_all().unwrap();
        log.append(&intervening).unwrap();
        let before = fs::read(&path).unwrap();

        assert_eq!(
            log.append_batch_if_unchanged(&stale, &[stale_batch])
                .unwrap(),
            AppendBatchOutcome::Changed
        );
        assert_eq!(fs::read(path).unwrap(), before);
        assert_eq!(log.read_all().unwrap().events, [first, intervening]);
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
    fn creates_a_private_event_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        log.append(&task_event(Uuid::now_v7(), None)).unwrap();

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

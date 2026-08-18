use std::collections::HashMap;

use jiff::Timestamp;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    AppendBatchOutcome, AttributionFailure, CliKind, ConfigurationBoundary, Event, EventKind,
    EventLog, ExecutionOrigin, ExecutionPlatform, ObservationAnomaly, ObservedConfiguration,
    ObservedTaskStatus, ObservedValue, ProfileAttribution, ProfileDeclaration, ProfileSnapshot,
    Provenance, ReadEvents, RecordError, SessionPhase,
};

const MAX_CAS_ATTEMPTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationCommand {
    ObserveSession {
        session_id: Option<Uuid>,
        cli: CliKind,
        phase: SessionPhase,
        external_session_id: Option<String>,
        working_directory: Option<std::path::PathBuf>,
        configuration: Option<ObservedConfiguration>,
        provenance: Provenance,
    },
    StartTask {
        session_id: Uuid,
        tags: Vec<String>,
        prompt: Option<String>,
        prompt_chars: usize,
        configuration: Option<ObservedConfiguration>,
    },
    EndTask {
        session_id: Uuid,
        status: ObservedTaskStatus,
        configuration: Option<ObservedConfiguration>,
    },
    RecordAnomaly {
        session_id: Uuid,
        anomaly: ObservationAnomaly,
        provenance: Provenance,
    },
    CorrectAttribution {
        session_id: Uuid,
        task_id: Uuid,
        attribution: ProfileAttribution,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationOutcome {
    pub session_id: Uuid,
    pub task_id: Option<Uuid>,
    pub anomaly_recorded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionState {
    cli: CliKind,
    ended: bool,
    configuration: Option<(Timestamp, ObservedConfiguration)>,
    active_task: Option<ActiveTask>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTask {
    task_id: Uuid,
    started_at: Timestamp,
    configuration: ObservedConfiguration,
}

#[derive(Debug, Default)]
struct ObservationState {
    sessions: HashMap<Uuid, SessionState>,
    external_sessions: HashMap<(CliKind, String), Uuid>,
    tasks: HashMap<Uuid, ObservedTaskState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedTaskState {
    session_id: Uuid,
    ended: bool,
}

pub fn apply_observation(
    event_log: &EventLog,
    profiles: &[ProfileDeclaration],
    command: ObservationCommand,
) -> Result<ObservationOutcome, ObservationError> {
    for _ in 0..MAX_CAS_ATTEMPTS {
        let history = event_log.read_all()?;
        if !history.skipped_lines.is_empty() {
            return Err(ObservationError::InvalidHistory);
        }
        let (outcome, events) = decide(&history, profiles, &command)?;
        match event_log.append_batch_if_unchanged(&history, &events)? {
            AppendBatchOutcome::Appended => return Ok(outcome),
            AppendBatchOutcome::Changed => continue,
        }
    }
    Err(ObservationError::ConcurrentModification)
}

pub fn resolve_observed_session(
    history: &ReadEvents,
    cli: CliKind,
    external_session_id: &str,
) -> Result<Option<Uuid>, ObservationError> {
    if !history.skipped_lines.is_empty() {
        return Err(ObservationError::InvalidHistory);
    }
    Ok(reconstruct(&history.events)?
        .external_sessions
        .get(&(cli, external_session_id.to_owned()))
        .copied())
}

fn decide(
    history: &ReadEvents,
    profiles: &[ProfileDeclaration],
    command: &ObservationCommand,
) -> Result<(ObservationOutcome, Vec<Event>), ObservationError> {
    let state = reconstruct(&history.events)?;
    let now = Timestamp::now();
    match command {
        ObservationCommand::ObserveSession {
            session_id,
            cli,
            phase,
            external_session_id,
            working_directory,
            configuration,
            provenance,
        } => {
            let correlated = external_session_id.as_ref().and_then(|external| {
                state
                    .external_sessions
                    .get(&(*cli, external.clone()))
                    .copied()
            });
            if let (Some(explicit), Some(correlated)) = (session_id, correlated)
                && explicit != &correlated
            {
                return Err(ObservationError::ExternalSessionConflict {
                    external_session_id: external_session_id.clone().unwrap_or_default(),
                    existing: correlated,
                    requested: *explicit,
                });
            }
            let session_id = session_id.or(correlated).unwrap_or_else(Uuid::now_v7);
            if let Some(existing) = state.sessions.get(&session_id)
                && existing.cli != *cli
            {
                return Err(ObservationError::SessionCliMismatch {
                    session_id,
                    expected: existing.cli,
                    found: *cli,
                });
            }
            if state
                .sessions
                .get(&session_id)
                .is_some_and(|session| session.ended && *phase == SessionPhase::Started)
            {
                return Err(ObservationError::InvalidSessionTransition {
                    session_id,
                    phase: *phase,
                });
            }
            if let Some(configuration) = configuration
                && (configuration.cli != *cli
                    || configuration.execution_platform != ExecutionPlatform::Interactive)
            {
                return Err(ObservationError::InvalidConfiguration(session_id));
            }
            let mut phase = *phase;
            if state
                .sessions
                .get(&session_id)
                .is_some_and(|session| !session.ended)
                && phase == SessionPhase::Started
            {
                phase = SessionPhase::Resumed;
            }
            let event = at(
                Event::for_session(
                    *provenance,
                    EventKind::SessionObserved {
                        session_id,
                        cli: *cli,
                        phase,
                        external_session_id: external_session_id.clone(),
                        working_directory: working_directory.clone(),
                        configuration: configuration.clone(),
                    },
                ),
                now,
            );
            Ok((
                ObservationOutcome {
                    session_id,
                    task_id: state
                        .sessions
                        .get(&session_id)
                        .and_then(|session| session.active_task.as_ref())
                        .map(|task| task.task_id),
                    anomaly_recorded: false,
                },
                vec![event],
            ))
        }
        ObservationCommand::StartTask {
            session_id,
            tags,
            prompt,
            prompt_chars,
            configuration,
        } => {
            let session = state
                .sessions
                .get(session_id)
                .ok_or(ObservationError::UnknownSession(*session_id))?;
            if session.ended {
                return Err(ObservationError::EndedSession(*session_id));
            }
            if let Some(active) = &session.active_task {
                return Err(ObservationError::TaskAlreadyActive {
                    session_id: *session_id,
                    task_id: active.task_id,
                });
            }
            let configuration = configuration
                .clone()
                .or_else(|| {
                    session
                        .configuration
                        .as_ref()
                        .map(|(_, configuration)| configuration.clone())
                })
                .unwrap_or_else(|| missing_configuration(session.cli));
            if configuration.cli != session.cli
                || configuration.execution_platform != ExecutionPlatform::Interactive
            {
                return Err(ObservationError::InvalidConfiguration(*session_id));
            }
            let task_id = Uuid::now_v7();
            let started = at(
                Event::for_task(
                    task_id,
                    Provenance::ManualMark,
                    EventKind::ObservedTaskStarted {
                        session_id: *session_id,
                        origin: ExecutionOrigin::DirectObservation,
                        tags: tags.clone(),
                        prompt: prompt.clone(),
                        prompt_chars: *prompt_chars,
                    },
                ),
                now,
            );
            let configuration = at(
                Event::for_task(
                    task_id,
                    Provenance::ManualMark,
                    EventKind::ConfigurationObserved {
                        session_id: *session_id,
                        boundary: ConfigurationBoundary::Start,
                        configuration,
                    },
                ),
                now,
            );
            Ok((
                ObservationOutcome {
                    session_id: *session_id,
                    task_id: Some(task_id),
                    anomaly_recorded: false,
                },
                vec![started, configuration],
            ))
        }
        ObservationCommand::EndTask {
            session_id,
            status,
            configuration,
        } => {
            let Some(session) = state.sessions.get(session_id) else {
                let anomaly = at(
                    Event::for_session(
                        Provenance::ManualMark,
                        EventKind::ObservationAnomaly {
                            session_id: Some(*session_id),
                            anomaly: ObservationAnomaly::OrphanTaskEnd,
                        },
                    ),
                    now,
                );
                return Ok((
                    ObservationOutcome {
                        session_id: *session_id,
                        task_id: None,
                        anomaly_recorded: true,
                    },
                    vec![anomaly],
                ));
            };
            let Some(active) = &session.active_task else {
                let anomaly = at(
                    Event::for_session(
                        Provenance::ManualMark,
                        EventKind::ObservationAnomaly {
                            session_id: Some(*session_id),
                            anomaly: ObservationAnomaly::OrphanTaskEnd,
                        },
                    ),
                    now,
                );
                return Ok((
                    ObservationOutcome {
                        session_id: *session_id,
                        task_id: None,
                        anomaly_recorded: true,
                    },
                    vec![anomaly],
                ));
            };
            let configuration = configuration
                .clone()
                .or_else(|| {
                    session
                        .configuration
                        .as_ref()
                        .filter(|(observed_at, _)| *observed_at >= active.started_at)
                        .map(|(_, configuration)| configuration.clone())
                })
                .unwrap_or_else(|| missing_configuration(session.cli));
            if configuration.cli != session.cli
                || configuration.execution_platform != ExecutionPlatform::Interactive
            {
                return Err(ObservationError::InvalidConfiguration(*session_id));
            }
            let task_id = active.task_id;
            let elapsed_ms = elapsed_millis(active.started_at, now);
            let attribution = attribute(&active.configuration, &configuration, profiles)?;
            let events = vec![
                at(
                    Event::for_task(
                        task_id,
                        Provenance::ManualMark,
                        EventKind::ObservedTaskEnded {
                            session_id: *session_id,
                            status: *status,
                        },
                    ),
                    now,
                ),
                at(
                    Event::for_task(
                        task_id,
                        Provenance::ManualMark,
                        EventKind::ConfigurationObserved {
                            session_id: *session_id,
                            boundary: ConfigurationBoundary::End,
                            configuration,
                        },
                    ),
                    now,
                ),
                at(
                    Event::for_task(
                        task_id,
                        Provenance::Dock,
                        EventKind::ProfileAttributed {
                            session_id: *session_id,
                            attribution,
                        },
                    ),
                    now,
                ),
                at(
                    Event::for_task(
                        task_id,
                        Provenance::Dock,
                        EventKind::MeasurementObserved {
                            session_id: *session_id,
                            elapsed_ms,
                            actual_cost: None,
                        },
                    ),
                    now,
                ),
            ];
            Ok((
                ObservationOutcome {
                    session_id: *session_id,
                    task_id: Some(task_id),
                    anomaly_recorded: false,
                },
                events,
            ))
        }
        ObservationCommand::RecordAnomaly {
            session_id,
            anomaly,
            provenance,
        } => {
            if !state.sessions.contains_key(session_id) {
                return Err(ObservationError::UnknownSession(*session_id));
            }
            Ok((
                ObservationOutcome {
                    session_id: *session_id,
                    task_id: None,
                    anomaly_recorded: true,
                },
                vec![at(
                    Event::for_session(
                        *provenance,
                        EventKind::ObservationAnomaly {
                            session_id: Some(*session_id),
                            anomaly: anomaly.clone(),
                        },
                    ),
                    now,
                )],
            ))
        }
        ObservationCommand::CorrectAttribution {
            session_id,
            task_id,
            attribution,
        } => {
            let task = state
                .tasks
                .get(task_id)
                .ok_or(ObservationError::UnknownTask(*task_id))?;
            if task.session_id != *session_id || !task.ended {
                return Err(ObservationError::InvalidAttributionCorrection(*task_id));
            }
            if let ProfileAttribution::Matched { profile } = attribution {
                let matching = profiles
                    .iter()
                    .filter(|candidate| {
                        candidate.id().as_deref() == Ok(profile.id.as_str())
                            && candidate.canonical_identity().ok()
                                == profile.declaration().canonical_identity().ok()
                    })
                    .count();
                if matching != 1 {
                    return Err(ObservationError::InvalidAttributionCorrection(*task_id));
                }
            }
            Ok((
                ObservationOutcome {
                    session_id: *session_id,
                    task_id: Some(*task_id),
                    anomaly_recorded: false,
                },
                vec![at(
                    Event::for_task(
                        *task_id,
                        Provenance::Dock,
                        EventKind::ProfileAttributed {
                            session_id: *session_id,
                            attribution: attribution.clone(),
                        },
                    ),
                    now,
                )],
            ))
        }
    }
}

fn reconstruct(events: &[Event]) -> Result<ObservationState, ObservationError> {
    let mut state = ObservationState::default();
    let mut task_sessions = HashMap::new();
    let mut ended_tasks = std::collections::HashSet::new();
    for event in events {
        match &event.kind {
            EventKind::SessionObserved {
                session_id,
                cli,
                phase,
                external_session_id,
                configuration,
                ..
            } => {
                if event.task_id.is_some() {
                    return Err(ObservationError::InvalidHistory);
                }
                if state
                    .sessions
                    .get(session_id)
                    .is_some_and(|session| session.ended && *phase == SessionPhase::Started)
                {
                    return Err(ObservationError::InvalidHistory);
                }
                let session = state.sessions.entry(*session_id).or_insert(SessionState {
                    cli: *cli,
                    ended: false,
                    configuration: None,
                    active_task: None,
                });
                if session.cli != *cli {
                    return Err(ObservationError::InvalidHistory);
                }
                session.ended = *phase == SessionPhase::Ended;
                if let Some(configuration) = configuration {
                    if configuration.cli != *cli
                        || configuration.execution_platform != ExecutionPlatform::Interactive
                    {
                        return Err(ObservationError::InvalidHistory);
                    }
                    session.configuration = Some((event.occurred_at, configuration.clone()));
                }
                if let Some(external) = external_session_id
                    && let Some(existing) = state
                        .external_sessions
                        .insert((*cli, external.clone()), *session_id)
                    && existing != *session_id
                {
                    return Err(ObservationError::InvalidHistory);
                }
            }
            EventKind::ObservedTaskStarted {
                session_id, origin, ..
            } => {
                let Some(task_id) = event.task_id else {
                    return Err(ObservationError::InvalidHistory);
                };
                let Some(session) = state.sessions.get_mut(session_id) else {
                    return Err(ObservationError::InvalidHistory);
                };
                if *origin != ExecutionOrigin::DirectObservation
                    || event.provenance != Provenance::ManualMark
                    || session.active_task.is_some()
                    || task_sessions.insert(task_id, *session_id).is_some()
                {
                    return Err(ObservationError::InvalidHistory);
                }
                session.active_task = Some(ActiveTask {
                    task_id,
                    started_at: event.occurred_at,
                    configuration: ObservedConfiguration {
                        cli: session.cli,
                        execution_platform: ExecutionPlatform::Interactive,
                        model: ObservedValue::Missing,
                        identity: Default::default(),
                    },
                });
            }
            EventKind::ConfigurationObserved {
                session_id,
                boundary: ConfigurationBoundary::Start,
                configuration,
            } => {
                let Some(task_id) = event.task_id else {
                    return Err(ObservationError::InvalidHistory);
                };
                if task_sessions.get(&task_id) != Some(session_id)
                    || configuration.cli
                        != state
                            .sessions
                            .get(session_id)
                            .ok_or(ObservationError::InvalidHistory)?
                            .cli
                    || configuration.execution_platform != ExecutionPlatform::Interactive
                {
                    return Err(ObservationError::InvalidHistory);
                }
                if let Some(active) = state
                    .sessions
                    .get_mut(session_id)
                    .and_then(|session| session.active_task.as_mut())
                    && task_id == active.task_id
                {
                    active.configuration = configuration.clone();
                } else {
                    return Err(ObservationError::InvalidHistory);
                }
            }
            EventKind::ObservedTaskEnded { session_id, .. } => {
                let Some(task_id) = event.task_id else {
                    return Err(ObservationError::InvalidHistory);
                };
                let Some(session) = state.sessions.get_mut(session_id) else {
                    return Err(ObservationError::InvalidHistory);
                };
                if event.provenance != Provenance::ManualMark
                    || session.active_task.as_ref().map(|task| task.task_id) != Some(task_id)
                    || !ended_tasks.insert(task_id)
                {
                    return Err(ObservationError::InvalidHistory);
                }
                session.active_task = None;
            }
            EventKind::ConfigurationObserved {
                session_id,
                boundary: ConfigurationBoundary::End,
                configuration,
            } => {
                let Some(task_id) = event.task_id else {
                    return Err(ObservationError::InvalidHistory);
                };
                let session = state
                    .sessions
                    .get(session_id)
                    .ok_or(ObservationError::InvalidHistory)?;
                if task_sessions.get(&task_id) != Some(session_id)
                    || !ended_tasks.contains(&task_id)
                    || configuration.cli != session.cli
                    || configuration.execution_platform != ExecutionPlatform::Interactive
                {
                    return Err(ObservationError::InvalidHistory);
                }
            }
            EventKind::ProfileAttributed { session_id, .. }
            | EventKind::MeasurementObserved { session_id, .. } => {
                let Some(task_id) = event.task_id else {
                    return Err(ObservationError::InvalidHistory);
                };
                if task_sessions.get(&task_id) != Some(session_id)
                    || !ended_tasks.contains(&task_id)
                {
                    return Err(ObservationError::InvalidHistory);
                }
            }
            _ => {}
        }
    }
    state.tasks = task_sessions
        .into_iter()
        .map(|(task_id, session_id)| {
            (
                task_id,
                ObservedTaskState {
                    session_id,
                    ended: ended_tasks.contains(&task_id),
                },
            )
        })
        .collect();
    Ok(state)
}

pub fn configuration_for_profile(profile: &ProfileDeclaration) -> ObservedConfiguration {
    ObservedConfiguration {
        cli: profile.cli,
        execution_platform: profile.execution_platform,
        model: ObservedValue::Observed(profile.model.clone()),
        identity: profile
            .identity
            .iter()
            .map(|(key, value)| (key.clone(), ObservedValue::Observed(value.clone())))
            .collect(),
    }
}

fn missing_configuration(cli: CliKind) -> ObservedConfiguration {
    ObservedConfiguration {
        cli,
        execution_platform: ExecutionPlatform::Interactive,
        model: ObservedValue::Missing,
        identity: Default::default(),
    }
}

fn attribute(
    started: &ObservedConfiguration,
    ended: &ObservedConfiguration,
    profiles: &[ProfileDeclaration],
) -> Result<ProfileAttribution, ObservationError> {
    let relevant: Vec<_> = profiles
        .iter()
        .filter(|profile| {
            profile.cli == started.cli && profile.execution_platform == started.execution_platform
        })
        .collect();
    if relevant.iter().any(|profile| {
        !configuration_complete_for_profile(started, profile)
            || !configuration_complete_for_profile(ended, profile)
    }) {
        return Ok(ProfileAttribution::Unattributed {
            reason: AttributionFailure::MissingConfiguration,
        });
    }
    let configuration_changed = started.model != ended.model
        || relevant.iter().any(|profile| {
            profile
                .identity
                .keys()
                .any(|key| started.identity.get(key) != ended.identity.get(key))
        });
    if configuration_changed {
        return Ok(ProfileAttribution::Unattributed {
            reason: AttributionFailure::ConfigurationChanged,
        });
    }
    let matches: Vec<_> = profiles
        .iter()
        .filter(|profile| configuration_matches(started, profile))
        .collect();
    match matches.as_slice() {
        [] => Ok(ProfileAttribution::Unattributed {
            reason: AttributionFailure::NoMatchingProfile,
        }),
        [profile] => Ok(ProfileAttribution::Matched {
            profile: ProfileSnapshot::observed(profile)?,
        }),
        _ => Ok(ProfileAttribution::Unattributed {
            reason: AttributionFailure::AmbiguousProfile,
        }),
    }
}

fn configuration_complete_for_profile(
    configuration: &ObservedConfiguration,
    profile: &ProfileDeclaration,
) -> bool {
    matches!(configuration.model, ObservedValue::Observed(_))
        && profile.identity.keys().all(|key| {
            matches!(
                configuration.identity.get(key),
                Some(ObservedValue::Observed(_))
            )
        })
}

fn configuration_matches(
    configuration: &ObservedConfiguration,
    profile: &ProfileDeclaration,
) -> bool {
    if configuration.cli != profile.cli
        || configuration.execution_platform != profile.execution_platform
    {
        return false;
    }
    let ObservedValue::Observed(model) = &configuration.model else {
        return false;
    };
    if model != &profile.model {
        return false;
    }
    profile.identity.iter().all(|(key, value)| {
        matches!(
            configuration.identity.get(key),
            Some(ObservedValue::Observed(observed)) if observed == value
        )
    })
}

fn elapsed_millis(started: Timestamp, ended: Timestamp) -> Option<u64> {
    let milliseconds = ended.duration_since(started).as_millis();
    u64::try_from(milliseconds).ok()
}

fn at(mut event: Event, occurred_at: Timestamp) -> Event {
    event.occurred_at = occurred_at;
    event
}

#[derive(Debug, Error)]
pub enum ObservationError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Profile(#[from] crate::adapter::ProfileValidationError),
    #[error("unknown observed session {0}")]
    UnknownSession(Uuid),
    #[error("observed session {0} has already ended")]
    EndedSession(Uuid),
    #[error("session {session_id} cannot accept phase {phase:?}")]
    InvalidSessionTransition {
        session_id: Uuid,
        phase: SessionPhase,
    },
    #[error("session {session_id} already has active task {task_id}")]
    TaskAlreadyActive { session_id: Uuid, task_id: Uuid },
    #[error("session {session_id} belongs to {expected}, not {found}")]
    SessionCliMismatch {
        session_id: Uuid,
        expected: CliKind,
        found: CliKind,
    },
    #[error("configuration does not describe interactive session {0}")]
    InvalidConfiguration(Uuid),
    #[error("unknown observed task {0}")]
    UnknownTask(Uuid),
    #[error("task {0} cannot receive an attribution correction")]
    InvalidAttributionCorrection(Uuid),
    #[error("event log kept changing while applying observation")]
    ConcurrentModification,
    #[error("event log contains invalid or unsupported observations")]
    InvalidHistory,
    #[error(
        "external session `{external_session_id}` is already mapped to {existing}, not {requested}"
    )]
    ExternalSessionConflict {
        external_session_id: String,
        existing: Uuid,
        requested: Uuid,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier};

    use super::*;

    fn profile(name: &str, model: &str) -> ProfileDeclaration {
        ProfileDeclaration {
            name: name.to_owned(),
            cli: CliKind::Codex,
            execution_platform: ExecutionPlatform::Interactive,
            executable: None,
            model: Some(model.to_owned()),
            args: Vec::new(),
            identity: BTreeMap::from([("permission_mode".to_owned(), "default".to_owned())]),
        }
    }

    fn open_session(log: &EventLog) -> Uuid {
        apply_observation(
            log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: None,
                cli: CliKind::Codex,
                phase: SessionPhase::Started,
                external_session_id: Some("vendor-session".to_owned()),
                working_directory: Some(std::path::PathBuf::from("/tmp/project")),
                configuration: None,
                provenance: Provenance::CliHook,
            },
        )
        .unwrap()
        .session_id
    }

    #[test]
    fn explicit_marks_form_sequential_tasks_and_unique_attribution() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = profile("Codex direct", "gpt-5");
        let configuration = configuration_for_profile(&profile);
        let session_id = open_session(&log);

        let first = apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: vec!["rust".to_owned()],
                prompt: None,
                prompt_chars: 12,
                configuration: Some(configuration.clone()),
            },
        )
        .unwrap();
        let ended = apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::EndTask {
                session_id,
                status: ObservedTaskStatus::Completed,
                configuration: Some(configuration.clone()),
            },
        )
        .unwrap();
        let second = apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: Vec::new(),
                prompt: None,
                prompt_chars: 0,
                configuration: Some(configuration),
            },
        )
        .unwrap();

        assert_eq!(first.task_id, ended.task_id);
        assert_ne!(first.task_id, second.task_id);
        assert!(log.read_all().unwrap().events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ProfileAttributed {
                attribution: ProfileAttribution::Matched { profile: matched },
                ..
            } if matched.name == "Codex direct"
        )));
    }

    #[test]
    fn rejects_second_active_task_and_records_orphan_end() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = profile("Codex direct", "gpt-5");
        let configuration = configuration_for_profile(&profile);
        let session_id = open_session(&log);
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: Vec::new(),
                prompt: None,
                prompt_chars: 0,
                configuration: Some(configuration.clone()),
            },
        )
        .unwrap();
        assert!(matches!(
            apply_observation(
                &log,
                std::slice::from_ref(&profile),
                ObservationCommand::StartTask {
                    session_id,
                    tags: Vec::new(),
                    prompt: None,
                    prompt_chars: 0,
                    configuration: Some(configuration.clone()),
                },
            ),
            Err(ObservationError::TaskAlreadyActive { .. })
        ));
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::EndTask {
                session_id,
                status: ObservedTaskStatus::Interrupted,
                configuration: Some(configuration.clone()),
            },
        )
        .unwrap();
        let orphan = apply_observation(
            &log,
            &[profile],
            ObservationCommand::EndTask {
                session_id,
                status: ObservedTaskStatus::Completed,
                configuration: Some(configuration),
            },
        )
        .unwrap();
        assert!(orphan.anomaly_recorded);
        assert!(log.read_all().unwrap().events.iter().any(|event| matches!(
            event.kind,
            EventKind::ObservationAnomaly {
                anomaly: ObservationAnomaly::OrphanTaskEnd,
                ..
            }
        )));
    }

    #[test]
    fn session_end_does_not_close_active_task() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = profile("Codex direct", "gpt-5");
        let session_id = open_session(&log);
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: Vec::new(),
                prompt: None,
                prompt_chars: 0,
                configuration: Some(configuration_for_profile(&profile)),
            },
        )
        .unwrap();
        apply_observation(
            &log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: Some(session_id),
                cli: CliKind::Codex,
                phase: SessionPhase::Ended,
                external_session_id: Some("vendor-session".to_owned()),
                working_directory: None,
                configuration: None,
                provenance: Provenance::CliHook,
            },
        )
        .unwrap();

        let history = log.read_all().unwrap();
        assert_eq!(
            history
                .events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ObservedTaskEnded { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn hook_configuration_must_be_observed_again_after_task_start() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = profile("Codex direct", "gpt-5");
        let configuration = configuration_for_profile(&profile);
        let session_id = apply_observation(
            &log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: None,
                cli: CliKind::Codex,
                phase: SessionPhase::Started,
                external_session_id: Some("captured-session".to_owned()),
                working_directory: None,
                configuration: Some(configuration.clone()),
                provenance: Provenance::CliHook,
            },
        )
        .unwrap()
        .session_id;
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: Vec::new(),
                prompt: None,
                prompt_chars: 0,
                configuration: None,
            },
        )
        .unwrap();
        apply_observation(
            &log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: Some(session_id),
                cli: CliKind::Codex,
                phase: SessionPhase::Started,
                external_session_id: Some("captured-session".to_owned()),
                working_directory: None,
                configuration: Some(configuration),
                provenance: Provenance::CliHook,
            },
        )
        .unwrap();
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::EndTask {
                session_id,
                status: ObservedTaskStatus::Completed,
                configuration: None,
            },
        )
        .unwrap();

        assert!(log.read_all().unwrap().events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ProfileAttributed {
                attribution: ProfileAttribution::Matched { profile: matched },
                ..
            } if matched.name == "Codex direct"
        )));
    }

    #[test]
    fn stale_session_configuration_is_missing_at_task_end() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = profile("Codex direct", "gpt-5");
        let session_id = apply_observation(
            &log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: None,
                cli: CliKind::Codex,
                phase: SessionPhase::Started,
                external_session_id: Some("stale-configuration".to_owned()),
                working_directory: None,
                configuration: Some(configuration_for_profile(&profile)),
                provenance: Provenance::CliHook,
            },
        )
        .unwrap()
        .session_id;
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: Vec::new(),
                prompt: None,
                prompt_chars: 0,
                configuration: None,
            },
        )
        .unwrap();
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::EndTask {
                session_id,
                status: ObservedTaskStatus::Completed,
                configuration: None,
            },
        )
        .unwrap();

        assert!(log.read_all().unwrap().events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ProfileAttributed {
                attribution: ProfileAttribution::Unattributed {
                    reason: AttributionFailure::MissingConfiguration
                },
                ..
            }
        )));
    }

    #[test]
    fn stale_start_cannot_reopen_an_ended_session() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let session_id = open_session(&log);
        apply_observation(
            &log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: Some(session_id),
                cli: CliKind::Codex,
                phase: SessionPhase::Ended,
                external_session_id: Some("vendor-session".to_owned()),
                working_directory: None,
                configuration: None,
                provenance: Provenance::CliHook,
            },
        )
        .unwrap();

        assert!(matches!(
            apply_observation(
                &log,
                &[],
                ObservationCommand::ObserveSession {
                    session_id: None,
                    cli: CliKind::Codex,
                    phase: SessionPhase::Started,
                    external_session_id: Some("vendor-session".to_owned()),
                    working_directory: None,
                    configuration: None,
                    provenance: Provenance::CliHook,
                },
            ),
            Err(ObservationError::InvalidSessionTransition { .. })
        ));
    }

    #[test]
    fn concurrent_task_starts_preserve_the_single_active_task_invariant() {
        let directory = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::new(directory.path().join("events.jsonl")));
        let profile = profile("Codex direct", "gpt-5");
        let configuration = configuration_for_profile(&profile);
        let session_id = open_session(&log);
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for tag in ["first", "second"] {
            let log = Arc::clone(&log);
            let barrier = Arc::clone(&barrier);
            let profile = profile.clone();
            let configuration = configuration.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                apply_observation(
                    &log,
                    &[profile],
                    ObservationCommand::StartTask {
                        session_id,
                        tags: vec![tag.to_owned()],
                        prompt: None,
                        prompt_chars: 0,
                        configuration: Some(configuration),
                    },
                )
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(ObservationError::TaskAlreadyActive { .. })))
                .count(),
            1
        );
        assert_eq!(
            log.read_all()
                .unwrap()
                .events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ObservedTaskStarted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn external_session_cannot_be_reassigned_to_another_local_session() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let original = open_session(&log);
        let requested = Uuid::now_v7();
        let result = apply_observation(
            &log,
            &[],
            ObservationCommand::ObserveSession {
                session_id: Some(requested),
                cli: CliKind::Codex,
                phase: SessionPhase::Started,
                external_session_id: Some("vendor-session".to_owned()),
                working_directory: None,
                configuration: None,
                provenance: Provenance::CliHook,
            },
        );
        assert!(matches!(
            result,
            Err(ObservationError::ExternalSessionConflict {
                existing,
                requested: found,
                ..
            }) if existing == original && found == requested
        ));
        assert_eq!(
            apply_observation(
                &log,
                &[],
                ObservationCommand::ObserveSession {
                    session_id: None,
                    cli: CliKind::Codex,
                    phase: SessionPhase::Resumed,
                    external_session_id: Some("vendor-session".to_owned()),
                    working_directory: None,
                    configuration: None,
                    provenance: Provenance::CliHook,
                },
            )
            .unwrap()
            .session_id,
            original
        );
    }

    #[test]
    fn refuses_to_extend_a_log_with_skipped_current_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        std::fs::write(&path, b"{not json}\n").unwrap();
        let log = EventLog::new(path);
        assert!(matches!(
            apply_observation(
                &log,
                &[],
                ObservationCommand::ObserveSession {
                    session_id: None,
                    cli: CliKind::Codex,
                    phase: SessionPhase::Started,
                    external_session_id: None,
                    working_directory: None,
                    configuration: None,
                    provenance: Provenance::ManualMark,
                },
            ),
            Err(ObservationError::InvalidHistory)
        ));
    }

    #[test]
    fn missing_or_changed_configuration_never_attributes() {
        let profile = profile("Codex direct", "gpt-5");
        let complete = configuration_for_profile(&profile);
        let mut missing = complete.clone();
        missing.model = ObservedValue::Missing;
        assert_eq!(
            attribute(&missing, &complete, std::slice::from_ref(&profile)).unwrap(),
            ProfileAttribution::Unattributed {
                reason: AttributionFailure::MissingConfiguration
            }
        );
        let mut changed = complete.clone();
        changed.model = ObservedValue::Observed(Some("gpt-5.1".to_owned()));
        assert_eq!(
            attribute(&complete, &changed, std::slice::from_ref(&profile)).unwrap(),
            ProfileAttribution::Unattributed {
                reason: AttributionFailure::ConfigurationChanged
            }
        );

        let mut profile_without_permission = profile.clone();
        profile_without_permission.name = "Codex model only".to_owned();
        profile_without_permission.identity.clear();
        let mut permission_changed = complete.clone();
        permission_changed.identity.insert(
            "permission_mode".to_owned(),
            ObservedValue::Observed("dontAsk".to_owned()),
        );
        assert!(matches!(
            attribute(
                &complete,
                &permission_changed,
                &[profile_without_permission]
            )
            .unwrap(),
            ProfileAttribution::Matched { .. }
        ));
    }

    #[test]
    fn attribution_corrections_append_and_latest_event_is_current() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = profile("Codex direct", "gpt-5");
        let configuration = configuration_for_profile(&profile);
        let session_id = open_session(&log);
        let task_id = apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::StartTask {
                session_id,
                tags: Vec::new(),
                prompt: None,
                prompt_chars: 0,
                configuration: Some(configuration.clone()),
            },
        )
        .unwrap()
        .task_id
        .unwrap();
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::EndTask {
                session_id,
                status: ObservedTaskStatus::Completed,
                configuration: Some(configuration),
            },
        )
        .unwrap();
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::CorrectAttribution {
                session_id,
                task_id,
                attribution: ProfileAttribution::Unattributed {
                    reason: AttributionFailure::ConfigurationChanged,
                },
            },
        )
        .unwrap();
        apply_observation(
            &log,
            std::slice::from_ref(&profile),
            ObservationCommand::CorrectAttribution {
                session_id,
                task_id,
                attribution: ProfileAttribution::Matched {
                    profile: ProfileSnapshot::observed(&profile).unwrap(),
                },
            },
        )
        .unwrap();

        let attributions: Vec<_> = log
            .read_all()
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::ProfileAttributed { attribution, .. } => Some(attribution),
                _ => None,
            })
            .collect();
        assert_eq!(attributions.len(), 3);
        assert!(matches!(
            attributions.last(),
            Some(ProfileAttribution::Matched { .. })
        ));
    }
}

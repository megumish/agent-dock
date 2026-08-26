use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use uuid::Uuid;

use crate::{
    Event, EventKind, ObservedTaskStatus, ProfileAttribution, ProfileSnapshot,
    RecordedExecutionOutcome, Verdict,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgementCandidate {
    pub target_event_id: Uuid,
    pub task_id: Uuid,
    pub started_at: Timestamp,
    pub profile: ProfileSnapshot,
    pub outcome: RecordedExecutionOutcome,
    pub tags: Vec<String>,
    pub current_verdict: Option<Verdict>,
}

pub fn judgement_candidates(events: &[Event]) -> Vec<JudgementCandidate> {
    let mut task_tags = HashMap::new();
    for event in events {
        if let EventKind::TaskReceived { tags, .. } | EventKind::ObservedTaskStarted { tags, .. } =
            &event.kind
            && let Some(task_id) = event.task_id
        {
            task_tags.insert(task_id, tags.clone());
        }
    }
    let verdicts = latest_verdicts(events);
    let mut candidates: Vec<_> = events
        .iter()
        .filter_map(|event| {
            let EventKind::Executed {
                origin: crate::ExecutionOrigin::Brokered,
                profile,
                started_at,
                outcome,
                ..
            } = &event.kind
            else {
                return None;
            };
            let task_id = event.task_id?;
            matches!(
                outcome,
                RecordedExecutionOutcome::Completed { .. } | RecordedExecutionOutcome::Cancelled
            )
            .then(|| JudgementCandidate {
                target_event_id: event.event_id,
                task_id,
                started_at: *started_at,
                profile: profile.clone(),
                outcome: outcome.clone(),
                tags: task_tags.get(&task_id).cloned().unwrap_or_default(),
                current_verdict: verdicts.get(&task_id).copied(),
            })
        })
        .collect();
    let mut direct_started = HashMap::new();
    let mut direct_ended = HashMap::new();
    let mut direct_profiles = HashMap::new();
    for event in events {
        let Some(task_id) = event.task_id else {
            continue;
        };
        match &event.kind {
            EventKind::ObservedTaskStarted {
                origin: crate::ExecutionOrigin::DirectObservation,
                ..
            } => {
                direct_started.insert(task_id, event.occurred_at);
            }
            EventKind::ObservedTaskEnded {
                status: ObservedTaskStatus::Completed,
                ..
            } => {
                direct_ended.insert(task_id, event.event_id);
            }
            EventKind::ProfileAttributed {
                attribution: ProfileAttribution::Matched { profile },
                ..
            } => {
                direct_profiles.insert(task_id, profile.clone());
            }
            EventKind::ProfileAttributed {
                attribution: ProfileAttribution::Unattributed { .. },
                ..
            } => {
                direct_profiles.remove(&task_id);
            }
            _ => {}
        }
    }
    candidates.extend(
        direct_ended
            .into_iter()
            .filter_map(|(task_id, target_event_id)| {
                Some(JudgementCandidate {
                    target_event_id,
                    task_id,
                    started_at: *direct_started.get(&task_id)?,
                    profile: direct_profiles.get(&task_id)?.clone(),
                    outcome: RecordedExecutionOutcome::Completed { exit_code: None },
                    tags: task_tags.get(&task_id).cloned().unwrap_or_default(),
                    current_verdict: verdicts.get(&task_id).copied(),
                })
            }),
    );
    let positions: HashMap<_, _> = events
        .iter()
        .enumerate()
        .map(|(index, event)| (event.event_id, index))
        .collect();
    candidates.sort_by_key(|candidate| positions.get(&candidate.target_event_id).copied());
    let mut by_task = HashMap::new();
    let mut deduplicated = Vec::new();
    for candidate in candidates {
        if let Some(index) = by_task.get(&candidate.task_id).copied() {
            deduplicated[index] = candidate;
        } else {
            by_task.insert(candidate.task_id, deduplicated.len());
            deduplicated.push(candidate);
        }
    }
    let mut candidates = deduplicated;
    candidates.reverse();
    candidates
}

pub(crate) fn latest_verdicts(events: &[Event]) -> HashMap<Uuid, Verdict> {
    let judgeable_tasks: HashSet<_> = events
        .iter()
        .filter_map(|event| {
            matches!(
                event.kind,
                EventKind::Executed {
                    origin: crate::ExecutionOrigin::Brokered,
                    ..
                } | EventKind::ObservedTaskEnded {
                    status: ObservedTaskStatus::Completed,
                    ..
                }
            )
            .then_some(event.task_id)
            .flatten()
        })
        .collect();
    let mut verdicts = HashMap::new();
    for event in events {
        if let EventKind::Judged { verdict } = event.kind
            && let Some(task_id) = event.task_id
            && judgeable_tasks.contains(&task_id)
        {
            verdicts.insert(task_id, verdict);
        }
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{CliKind, ExecutionOrigin, ExecutionPlatform, FailureKind};

    fn snapshot(name: &str) -> ProfileSnapshot {
        ProfileSnapshot {
            id: format!("profile-{name}"),
            name: name.to_owned(),
            cli: CliKind::Test,
            execution_platform: ExecutionPlatform::Headless,
            executable_override: None,
            model: None,
            args: Vec::new(),
            identity: Default::default(),
            resolved_executable: Some(PathBuf::from("/bin/true")),
        }
    }

    fn executed(task_id: Uuid, outcome: RecordedExecutionOutcome) -> Event {
        Event::new(
            task_id,
            EventKind::Executed {
                origin: ExecutionOrigin::Brokered,
                profile: snapshot("historical"),
                working_directory: PathBuf::from("/tmp/project"),
                started_at: "2026-08-01T00:00:00Z".parse().unwrap(),
                elapsed_ms: 100,
                outcome,
                cost: None,
                estimated_cost: None,
                usage: None,
                report_failure: None,
                observed_cli_version: None,
                observed_models: Vec::new(),
            },
        )
    }

    #[test]
    fn lists_completed_and_cancelled_executions_newest_first_with_current_verdict() {
        let older_task = Uuid::now_v7();
        let newer_task = Uuid::now_v7();
        let mut older = executed(
            older_task,
            RecordedExecutionOutcome::Completed { exit_code: Some(0) },
        );
        let mut newer = executed(
            newer_task,
            RecordedExecutionOutcome::Completed { exit_code: None },
        );
        older.event_id = Uuid::now_v7();
        newer.event_id = Uuid::now_v7();
        let cancelled = executed(Uuid::now_v7(), RecordedExecutionOutcome::Cancelled);
        let cancelled_id = cancelled.event_id;
        let failed = executed(
            older_task,
            RecordedExecutionOutcome::Failed {
                failure_kind: FailureKind::Runtime,
                message: "failed".to_owned(),
            },
        );
        let events = vec![
            Event::new(
                older_task,
                EventKind::TaskReceived {
                    tags: vec!["rust".to_owned()],
                    prompt: None,
                    prompt_chars: 4,
                },
            ),
            older.clone(),
            Event::new(
                older_task,
                EventKind::Judged {
                    verdict: Verdict::Rejected,
                },
            ),
            Event::new(
                older_task,
                EventKind::Judged {
                    verdict: Verdict::Accepted,
                },
            ),
            cancelled,
            failed,
            newer.clone(),
        ];

        let candidates = judgement_candidates(&events);

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].target_event_id, newer.event_id);
        assert_eq!(
            candidates[0].outcome,
            RecordedExecutionOutcome::Completed { exit_code: None }
        );
        assert_eq!(candidates[0].current_verdict, None);
        assert_eq!(candidates[1].target_event_id, cancelled_id);
        assert_eq!(candidates[1].outcome, RecordedExecutionOutcome::Cancelled);
        assert_eq!(candidates[1].current_verdict, None);
        assert_eq!(candidates[2].target_event_id, older.event_id);
        assert_eq!(candidates[2].tags, ["rust"]);
        assert_eq!(candidates[2].current_verdict, Some(Verdict::Accepted));
    }

    #[test]
    fn ignores_judgements_for_unknown_executions() {
        let event = Event::new(
            Uuid::now_v7(),
            EventKind::Judged {
                verdict: Verdict::Accepted,
            },
        );

        assert!(latest_verdicts(&[event]).is_empty());
    }

    #[test]
    fn lists_only_currently_attributed_completed_direct_tasks_and_uses_latest_task_verdict() {
        let task_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let profile = snapshot("direct");
        let started = Event::for_task(
            task_id,
            crate::Provenance::ManualMark,
            EventKind::ObservedTaskStarted {
                session_id,
                origin: crate::ExecutionOrigin::DirectObservation,
                tags: vec!["review".to_owned()],
                prompt: None,
                prompt_chars: 0,
            },
        );
        let ended = Event::for_task(
            task_id,
            crate::Provenance::ManualMark,
            EventKind::ObservedTaskEnded {
                session_id,
                status: ObservedTaskStatus::Completed,
            },
        );
        let events = vec![
            started,
            ended.clone(),
            Event::for_task(
                task_id,
                crate::Provenance::Dock,
                EventKind::ProfileAttributed {
                    session_id,
                    attribution: ProfileAttribution::Matched {
                        profile: profile.clone(),
                    },
                },
            ),
            Event::new(
                task_id,
                EventKind::Judged {
                    verdict: Verdict::Rejected,
                },
            ),
            Event::new(
                task_id,
                EventKind::Judged {
                    verdict: Verdict::Accepted,
                },
            ),
        ];

        let candidates = judgement_candidates(&events);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].target_event_id, ended.event_id);
        assert_eq!(candidates[0].profile, profile);
        assert_eq!(candidates[0].tags, ["review"]);
        assert_eq!(candidates[0].current_verdict, Some(Verdict::Accepted));

        let mut corrected = events;
        corrected.push(Event::for_task(
            task_id,
            crate::Provenance::Dock,
            EventKind::ProfileAttributed {
                session_id,
                attribution: ProfileAttribution::Unattributed {
                    reason: crate::AttributionFailure::ConfigurationChanged,
                },
            },
        ));
        assert!(judgement_candidates(&corrected).is_empty());
    }
}

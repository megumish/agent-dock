use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use uuid::Uuid;

use crate::{Event, EventKind, ProfileSnapshot, RecordedExecutionOutcome, Verdict};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgementCandidate {
    pub execution_event_id: Uuid,
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
        if let EventKind::TaskReceived { tags, .. } = &event.kind {
            task_tags.insert(event.task_id, tags.clone());
        }
    }
    let verdicts = latest_verdicts(events);
    let mut candidates: Vec<_> = events
        .iter()
        .filter_map(|event| {
            let EventKind::Executed {
                profile,
                started_at,
                outcome,
                ..
            } = &event.kind
            else {
                return None;
            };
            matches!(
                outcome,
                RecordedExecutionOutcome::Completed { .. } | RecordedExecutionOutcome::Cancelled
            )
            .then(|| JudgementCandidate {
                execution_event_id: event.event_id,
                task_id: event.task_id,
                started_at: *started_at,
                profile: profile.clone(),
                outcome: outcome.clone(),
                tags: task_tags.get(&event.task_id).cloned().unwrap_or_default(),
                current_verdict: verdicts.get(&event.event_id).copied(),
            })
        })
        .collect();
    candidates.reverse();
    candidates
}

pub(crate) fn latest_verdicts(events: &[Event]) -> HashMap<Uuid, Verdict> {
    let execution_ids: HashSet<_> = events
        .iter()
        .filter_map(|event| {
            matches!(event.kind, EventKind::Executed { .. }).then_some(event.event_id)
        })
        .collect();
    let mut verdicts = HashMap::new();
    for event in events {
        if let EventKind::Judged {
            execution_event_id,
            verdict,
        } = event.kind
            && execution_ids.contains(&execution_event_id)
        {
            verdicts.insert(execution_event_id, verdict);
        }
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{CliKind, FailureKind};

    fn snapshot(name: &str) -> ProfileSnapshot {
        ProfileSnapshot {
            id: format!("profile-{name}"),
            name: name.to_owned(),
            cli: CliKind::Test,
            executable_override: None,
            model: None,
            args: Vec::new(),
            resolved_executable: PathBuf::from("/bin/true"),
        }
    }

    fn executed(task_id: Uuid, outcome: RecordedExecutionOutcome) -> Event {
        Event::new(
            task_id,
            EventKind::Executed {
                profile: snapshot("historical"),
                working_directory: PathBuf::from("/tmp/project"),
                started_at: "2026-08-01T00:00:00Z".parse().unwrap(),
                elapsed_ms: 100,
                outcome,
                cost: None,
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
            RecordedExecutionOutcome::Completed { exit_code: Some(2) },
        );
        older.event_id = Uuid::now_v7();
        newer.event_id = Uuid::now_v7();
        let cancelled = executed(older_task, RecordedExecutionOutcome::Cancelled);
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
                    execution_event_id: older.event_id,
                    verdict: Verdict::Rejected,
                },
            ),
            Event::new(
                older_task,
                EventKind::Judged {
                    execution_event_id: older.event_id,
                    verdict: Verdict::Accepted,
                },
            ),
            cancelled,
            failed,
            newer.clone(),
        ];

        let candidates = judgement_candidates(&events);

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].execution_event_id, newer.event_id);
        assert_eq!(candidates[0].current_verdict, None);
        assert_eq!(candidates[1].execution_event_id, cancelled_id);
        assert_eq!(candidates[1].outcome, RecordedExecutionOutcome::Cancelled);
        assert_eq!(candidates[1].current_verdict, None);
        assert_eq!(candidates[2].execution_event_id, older.event_id);
        assert_eq!(candidates[2].tags, ["rust"]);
        assert_eq!(candidates[2].current_verdict, Some(Verdict::Accepted));
    }

    #[test]
    fn ignores_judgements_for_unknown_executions() {
        let event = Event::new(
            Uuid::now_v7(),
            EventKind::Judged {
                execution_event_id: Uuid::now_v7(),
                verdict: Verdict::Accepted,
            },
        );

        assert!(latest_verdicts(&[event]).is_empty());
    }
}

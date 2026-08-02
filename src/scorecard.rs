use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use uuid::Uuid;

use crate::{Event, EventKind, ExecutionProfile, RecordedExecutionOutcome, Verdict};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Segment {
    Overall,
    Untagged,
    Tag(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AxisSummary {
    pub count: usize,
    pub latest_started_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcceptanceAxis {
    pub summary: AxisSummary,
    pub accepted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DurationAxis {
    pub summary: AxisSummary,
    pub median_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CostAxis {
    pub summary: AxisSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentScore {
    pub segment: Segment,
    pub execution_count: usize,
    pub excluded_count: usize,
    pub acceptance: AcceptanceAxis,
    pub duration: DurationAxis,
    pub cost: CostAxis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileScorecard {
    pub profile_id: String,
    pub scores: Vec<SegmentScore>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Projection {
    pub scorecards: Vec<ProfileScorecard>,
    pub warnings: Vec<String>,
}

struct Execution<'a> {
    event_id: Uuid,
    started_at: Timestamp,
    elapsed_ms: u64,
    outcome: &'a RecordedExecutionOutcome,
    tags: &'a [String],
}

pub fn segments_for_tags(tags: &[String]) -> Vec<Segment> {
    let mut result = vec![Segment::Overall];
    let mut seen = HashSet::new();
    if tags.is_empty() {
        result.push(Segment::Untagged);
    } else {
        result.extend(
            tags.iter()
                .filter(|tag| seen.insert((*tag).clone()))
                .cloned()
                .map(Segment::Tag),
        );
    }
    result
}

pub fn project(
    events: &[Event],
    profiles: &[ExecutionProfile],
    requested: &[Segment],
) -> Projection {
    let mut projection = Projection::default();
    let mut task_tags: HashMap<Uuid, &[String]> = HashMap::new();
    let execution_ids: HashSet<Uuid> = events
        .iter()
        .filter_map(|event| {
            matches!(event.kind, EventKind::Executed { .. }).then_some(event.event_id)
        })
        .collect();
    let mut verdicts = HashMap::new();
    for event in events {
        match &event.kind {
            EventKind::TaskReceived { tags, .. } => {
                task_tags.insert(event.task_id, tags);
            }
            EventKind::Judged {
                execution_event_id,
                verdict,
            } if execution_ids.contains(execution_event_id) => {
                verdicts.insert(*execution_event_id, *verdict);
            }
            _ => {}
        }
    }

    let mut current = HashMap::new();
    for profile in profiles {
        current.entry(profile.id.as_str()).or_insert(profile);
    }
    let mut executions: HashMap<&str, Vec<Execution<'_>>> = HashMap::new();
    let empty_tags = Vec::new();
    for event in events {
        let EventKind::Executed {
            profile,
            started_at,
            elapsed_ms,
            outcome,
            ..
        } = &event.kind
        else {
            continue;
        };
        let Some(current_profile) = current.get(profile.id.as_str()) else {
            continue;
        };
        let declared_id = profile.declaration().id();
        let same_identity = profile.declaration().canonical_identity().ok()
            == current_profile.declaration.canonical_identity().ok();
        if declared_id.as_deref() != Ok(profile.id.as_str()) || !same_identity {
            projection.warnings.push(format!(
                "execution event {} has inconsistent profile identity {}; excluded",
                event.event_id, profile.id
            ));
            continue;
        }
        executions
            .entry(profile.id.as_str())
            .or_default()
            .push(Execution {
                event_id: event.event_id,
                started_at: *started_at,
                elapsed_ms: *elapsed_ms,
                outcome,
                tags: task_tags
                    .get(&event.task_id)
                    .copied()
                    .unwrap_or(&empty_tags),
            });
    }

    let mut seen_profiles = HashSet::new();
    let mut seen_segments = HashSet::new();
    let segments: Vec<_> = requested
        .iter()
        .filter(|segment| seen_segments.insert((*segment).clone()))
        .cloned()
        .collect();
    for profile in profiles
        .iter()
        .filter(|profile| seen_profiles.insert(profile.id.as_str()))
    {
        let all = executions
            .get(profile.id.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let scores = segments
            .iter()
            .map(|segment| {
                let selected: Vec<_> = all
                    .iter()
                    .filter(|execution| matches_segment(segment, execution.tags))
                    .collect();
                score_segment(segment.clone(), &selected, &verdicts)
            })
            .collect();
        projection.scorecards.push(ProfileScorecard {
            profile_id: profile.id.clone(),
            scores,
        });
    }
    projection
}

fn matches_segment(segment: &Segment, tags: &[String]) -> bool {
    match segment {
        Segment::Overall => true,
        Segment::Untagged => tags.is_empty(),
        Segment::Tag(tag) => tags.contains(tag),
    }
}

fn score_segment(
    segment: Segment,
    executions: &[&Execution<'_>],
    verdicts: &HashMap<Uuid, Verdict>,
) -> SegmentScore {
    let mut acceptance = AcceptanceAxis::default();
    let mut durations = Vec::new();
    let mut duration_latest = None;
    let mut included = HashSet::new();
    for execution in executions {
        if let Some(verdict) = verdicts.get(&execution.event_id) {
            acceptance.summary.count += 1;
            acceptance.accepted += usize::from(*verdict == Verdict::Accepted);
            acceptance.summary.latest_started_at =
                latest(acceptance.summary.latest_started_at, execution.started_at);
            included.insert(execution.event_id);
            if *verdict == Verdict::Accepted
                && matches!(
                    execution.outcome,
                    RecordedExecutionOutcome::Completed { .. }
                )
            {
                durations.push(execution.elapsed_ms);
                duration_latest = latest(duration_latest, execution.started_at);
            }
        }
    }
    durations.sort_unstable();
    let median_ms = median(&durations);
    let duration = DurationAxis {
        summary: AxisSummary {
            count: durations.len(),
            latest_started_at: duration_latest,
        },
        median_ms,
    };
    let excluded_count = executions
        .iter()
        .filter(|execution| !included.contains(&execution.event_id))
        .count();
    SegmentScore {
        segment,
        execution_count: executions.len(),
        excluded_count,
        acceptance,
        duration,
        cost: CostAxis::default(),
    }
}

fn latest(current: Option<Timestamp>, value: Timestamp) -> Option<Timestamp> {
    Some(current.map_or(value, |current| current.max(value)))
}

fn median(values: &[u64]) -> Option<u64> {
    let upper = *values.get(values.len() / 2)?;
    if values.len() % 2 == 1 {
        Some(upper)
    } else {
        let lower = values[values.len() / 2 - 1];
        Some(lower + (upper - lower) / 2)
    }
}

pub fn escape_terminal(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{{{:x}}}", character as u32))
            }
            character => escaped.push(character),
        }
    }
    escaped
}

pub fn format_acceptance(axis: &AcceptanceAxis) -> String {
    if axis.summary.count == 0 {
        return "- (0/0)".to_owned();
    }
    let tenths = ((axis.accepted as u128 * 1_000) + axis.summary.count as u128 / 2)
        / axis.summary.count as u128;
    format!(
        "{}.{:01}% ({}/{})",
        tenths / 10,
        tenths % 10,
        axis.accepted,
        axis.summary.count
    )
}

pub fn format_duration(axis: &DurationAxis) -> String {
    let Some(ms) = axis.median_ms else {
        return "- (0)".to_owned();
    };
    let value = if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{:01}s", ms / 1_000, (ms % 1_000) / 100)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms / 1_000) % 60)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms / 60_000) % 60)
    };
    format!("{value} ({})", axis.summary.count)
}

pub fn format_recency(now: Timestamp, latest: Option<Timestamp>) -> Option<String> {
    let elapsed = now.duration_since(latest?).as_secs();
    if elapsed <= 0 {
        Some("just now".to_owned())
    } else if elapsed < 60 {
        Some(format!("{elapsed}s ago"))
    } else if elapsed < 3_600 {
        Some(format!("{}m ago", elapsed / 60))
    } else if elapsed < 86_400 {
        Some(format!("{}h ago", elapsed / 3_600))
    } else {
        Some(format!("{}d ago", elapsed / 86_400))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Event, EventKind, ProfileSnapshot, RecordedExecutionOutcome, safe_test_profile};

    fn timestamp(value: &str) -> Timestamp {
        value.parse().unwrap()
    }

    fn task(task_id: Uuid, at: &str, tags: &[&str]) -> Event {
        let mut event = Event::new(
            task_id,
            EventKind::TaskReceived {
                tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                prompt: None,
                prompt_chars: 1,
            },
        );
        event.occurred_at = timestamp(at);
        event
    }

    fn execution(
        task_id: Uuid,
        event_id: Uuid,
        at: &str,
        elapsed_ms: u64,
        completed: bool,
    ) -> Event {
        let profile = safe_test_profile();
        Event {
            format_version: crate::EVENT_FORMAT_VERSION.to_owned(),
            event_id,
            task_id,
            occurred_at: timestamp(at),
            kind: EventKind::Executed {
                profile: ProfileSnapshot::from(&profile),
                working_directory: PathBuf::from("/tmp"),
                started_at: timestamp(at),
                elapsed_ms,
                outcome: if completed {
                    RecordedExecutionOutcome::Completed { exit_code: Some(0) }
                } else {
                    RecordedExecutionOutcome::Cancelled
                },
                cost: None,
            },
        }
    }

    fn judged(task_id: Uuid, execution_event_id: Uuid, verdict: Verdict) -> Event {
        Event::new(
            task_id,
            EventKind::Judged {
                execution_event_id,
                verdict,
            },
        )
    }

    #[test]
    fn projects_per_execution_with_last_verdict_and_tag_membership() {
        let task_id = Uuid::now_v7();
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let events = vec![
            task(task_id, "2026-01-01T00:00:00Z", &["rust", "docs"]),
            execution(task_id, first, "2026-01-01T00:00:01Z", 1_000, true),
            judged(task_id, first, Verdict::Accepted),
            judged(Uuid::now_v7(), first, Verdict::Rejected),
            execution(task_id, second, "2026-01-01T00:00:03Z", 1_000, true),
            judged(task_id, second, Verdict::Accepted),
            judged(task_id, second, Verdict::Rejected),
        ];
        let profile = safe_test_profile();
        let projection = project(
            &events,
            std::slice::from_ref(&profile),
            &segments_for_tags(&["rust".to_owned()]),
        );
        let overall = &projection.scorecards[0].scores[0];
        assert_eq!(overall.execution_count, 2);
        assert_eq!(
            (
                overall.acceptance.accepted,
                overall.acceptance.summary.count
            ),
            (0, 2)
        );
        assert_eq!(
            (overall.duration.median_ms, overall.duration.summary.count),
            (None, 0)
        );
        assert_eq!(overall.excluded_count, 0);
        assert_eq!(projection.scorecards[0].scores[1].execution_count, 2);
    }

    #[test]
    fn duration_uses_only_completed_crew_member_runtime() {
        let task_id = Uuid::now_v7();
        let cancelled = Uuid::now_v7();
        let missing_task_id = Uuid::now_v7();
        let accepted_without_task = Uuid::now_v7();
        let events = vec![
            task(task_id, "2026-01-01T00:00:00Z", &[]),
            execution(task_id, cancelled, "2026-01-01T00:00:01Z", 10, false),
            execution(
                missing_task_id,
                accepted_without_task,
                "2026-01-01T00:00:02Z",
                10,
                true,
            ),
            judged(missing_task_id, accepted_without_task, Verdict::Accepted),
        ];
        let projection = project(&events, &[safe_test_profile()], &[Segment::Overall]);
        let score = &projection.scorecards[0].scores[0];
        assert_eq!(score.acceptance.summary.count, 1);
        assert_eq!(score.duration.median_ms, Some(10));
        assert_eq!(score.duration.summary.count, 1);
        assert_eq!(score.excluded_count, 1);
        assert_eq!(score.cost.summary.count, 0);
    }

    #[test]
    fn rejects_snapshots_whose_declared_identity_does_not_match_id() {
        let task_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let mut event = execution(task_id, event_id, "2026-01-01T00:00:00Z", 1, true);
        let EventKind::Executed { profile, .. } = &mut event.kind else {
            unreachable!()
        };
        profile.model = Some("changed".to_owned());
        let projection = project(&[event], &[safe_test_profile()], &[Segment::Overall]);
        assert_eq!(projection.scorecards[0].scores[0].execution_count, 0);
        assert_eq!(projection.warnings.len(), 1);
    }

    #[test]
    fn duplicate_current_profile_ids_consistently_use_the_first_profile() {
        let first = safe_test_profile();
        let mut duplicate = first.clone();
        duplicate.declaration.model = Some("different".to_owned());
        let task_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let event = execution(task_id, event_id, "2026-01-01T00:00:00Z", 1, true);

        let projection = project(&[event], &[first, duplicate], &[Segment::Overall]);

        assert_eq!(projection.scorecards.len(), 1);
        assert_eq!(projection.scorecards[0].scores[0].execution_count, 1);
        assert!(projection.warnings.is_empty());
    }

    #[test]
    fn formats_boundaries_without_floating_point() {
        let acceptance = |accepted, count| AcceptanceAxis {
            summary: AxisSummary {
                count,
                latest_started_at: None,
            },
            accepted,
        };
        assert_eq!(format_acceptance(&acceptance(1, 16)), "6.3% (1/16)");
        assert_eq!(format_acceptance(&acceptance(0, 0)), "- (0/0)");
        for (ms, expected) in [
            (999, "999ms (1)"),
            (1_000, "1.0s (1)"),
            (59_999, "59.9s (1)"),
            (125_000, "2m05s (1)"),
            (3_720_000, "1h02m (1)"),
        ] {
            let axis = DurationAxis {
                summary: AxisSummary {
                    count: 1,
                    latest_started_at: None,
                },
                median_ms: Some(ms),
            };
            assert_eq!(format_duration(&axis), expected);
        }
        assert_eq!(median(&[1, 2]), Some(1));
        assert_eq!(median(&[u64::MAX - 1, u64::MAX]), Some(u64::MAX - 1));
    }

    #[test]
    fn escapes_terminal_controls_and_limits_visible_tags() {
        assert_eq!(
            escape_terminal("a\\\n\r\t\u{1b}雪"),
            "a\\\\\\n\\r\\t\\u{1b}雪"
        );
        let tags: Vec<_> = (0..7).map(|index| format!("t{index}")).collect();
        assert_eq!(segments_for_tags(&tags).len(), 8);
    }

    #[test]
    fn formats_relative_time_by_largest_unit() {
        let now = timestamp("2026-01-03T00:00:00Z");
        for (latest, expected) in [
            ("2026-01-03T00:00:01Z", "just now"),
            ("2026-01-02T23:59:15Z", "45s ago"),
            ("2026-01-02T19:00:00Z", "5h ago"),
            ("2026-01-01T00:00:00Z", "2d ago"),
        ] {
            assert_eq!(
                format_recency(now, Some(timestamp(latest))).as_deref(),
                Some(expected)
            );
        }
    }
}

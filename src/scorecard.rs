use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use uuid::Uuid;

use crate::{
    ActualCost, AttributionFailure, EstimatedCost, Event, EventKind, ExecutionOrigin,
    ExecutionProfile, ObservedTaskStatus, ProfileAttribution, RecordedExecutionOutcome,
    ReportFailure, TokenUsage, Verdict, judgement::latest_verdicts,
};

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
    pub currency: Option<String>,
    pub total_minor_units: Option<u64>,
    pub missing_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReportFailureCounts {
    pub parse: usize,
    pub limit: usize,
    pub model: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EstimatedCostSummary {
    #[default]
    None,
    Total {
        currency: String,
        amount_micros: u64,
        count: usize,
    },
    MixedCurrencies {
        count: usize,
    },
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReportedDisclosure {
    pub brokered_count: usize,
    pub reported_count: usize,
    pub missing_count: usize,
    pub failures: ReportFailureCounts,
    pub estimated_cost: EstimatedCostSummary,
    pub latest_started_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentScore {
    pub segment: Segment,
    pub execution_count: usize,
    pub excluded_count: usize,
    pub acceptance: AcceptanceAxis,
    pub duration: DurationAxis,
    pub cost: CostAxis,
    pub reported: ReportedDisclosure,
    pub origins: OriginCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OriginCounts {
    pub brokered: usize,
    pub direct_observation: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileScorecard {
    pub profile_id: String,
    pub scores: Vec<SegmentScore>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Projection {
    pub scorecards: Vec<ProfileScorecard>,
    pub exclusions: Vec<ExclusionSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExclusionReason {
    Ongoing,
    Interrupted,
    InvalidTiming,
    MissingConfiguration,
    ConfigurationChanged,
    NoMatchingProfile,
    AmbiguousProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusionSummary {
    pub reason: ExclusionReason,
    pub count: usize,
}

struct Execution<'a> {
    task_id: Uuid,
    started_at: Timestamp,
    elapsed_ms: u64,
    completed: bool,
    actual_cost: Option<&'a ActualCost>,
    estimated_cost: Option<&'a EstimatedCost>,
    usage: Option<&'a TokenUsage>,
    report_failure: Option<ReportFailure>,
    origin: ExecutionOrigin,
    tags: &'a [String],
}

#[derive(Default)]
struct DirectTask<'a> {
    started_at: Option<Timestamp>,
    status: Option<ObservedTaskStatus>,
    attribution: Option<&'a ProfileAttribution>,
    elapsed_ms: Option<u64>,
    actual_cost: Option<&'a ActualCost>,
    origin: Option<ExecutionOrigin>,
}

pub fn segments_for_tags(tags: &[String]) -> Vec<Segment> {
    let mut result = vec![Segment::Overall];
    let mut seen = HashSet::new();
    for tag in tags {
        let tag = tag.trim();
        if !tag.is_empty() && seen.insert(tag.to_owned()) {
            result.push(Segment::Tag(tag.to_owned()));
        }
    }
    if result.len() == 1 {
        result.push(Segment::Untagged);
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
    let verdicts = latest_verdicts(events);
    for event in events {
        if let EventKind::TaskReceived { tags, .. } = &event.kind {
            if let Some(task_id) = event.task_id {
                task_tags.insert(task_id, tags);
            }
        } else if let EventKind::ObservedTaskStarted { tags, .. } = &event.kind
            && let Some(task_id) = event.task_id
        {
            task_tags.insert(task_id, tags);
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
            origin,
            profile,
            started_at,
            elapsed_ms,
            outcome,
            cost,
            estimated_cost,
            usage,
            report_failure,
            ..
        } = &event.kind
        else {
            continue;
        };
        let Some(task_id) = event.task_id else {
            projection.warnings.push(format!(
                "execution event {} is missing task identity; excluded",
                event.event_id
            ));
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
                task_id,
                started_at: *started_at,
                elapsed_ms: *elapsed_ms,
                completed: matches!(outcome, RecordedExecutionOutcome::Completed { .. }),
                actual_cost: cost.as_ref(),
                estimated_cost: estimated_cost.as_ref(),
                usage: usage.as_ref(),
                report_failure: *report_failure,
                origin: *origin,
                tags: task_tags.get(&task_id).copied().unwrap_or(&empty_tags),
            });
    }

    let mut direct: HashMap<Uuid, DirectTask<'_>> = HashMap::new();
    for event in events {
        let Some(task_id) = event.task_id else {
            continue;
        };
        match &event.kind {
            EventKind::ObservedTaskStarted { origin, .. } => {
                let task = direct.entry(task_id).or_default();
                task.started_at = Some(event.occurred_at);
                task.origin = Some(*origin);
            }
            EventKind::ObservedTaskEnded { status, .. } => {
                direct.entry(task_id).or_default().status = Some(*status);
            }
            EventKind::ProfileAttributed { attribution, .. } => {
                direct.entry(task_id).or_default().attribution = Some(attribution);
            }
            EventKind::MeasurementObserved {
                elapsed_ms,
                actual_cost,
                ..
            } => {
                let task = direct.entry(task_id).or_default();
                task.elapsed_ms = *elapsed_ms;
                task.actual_cost = actual_cost.as_ref();
            }
            _ => {}
        }
    }
    let mut exclusion_counts: HashMap<ExclusionReason, usize> = HashMap::new();
    for (task_id, task) in direct {
        let reason = match task.status {
            None => Some(ExclusionReason::Ongoing),
            Some(ObservedTaskStatus::Interrupted) => Some(ExclusionReason::Interrupted),
            Some(ObservedTaskStatus::Completed)
                if task.started_at.is_none()
                    || task.elapsed_ms.is_none()
                    || task.origin != Some(ExecutionOrigin::DirectObservation) =>
            {
                Some(ExclusionReason::InvalidTiming)
            }
            Some(ObservedTaskStatus::Completed) => match task.attribution {
                Some(ProfileAttribution::Matched { .. }) => None,
                Some(ProfileAttribution::Unattributed { reason }) => Some(match reason {
                    AttributionFailure::MissingConfiguration => {
                        ExclusionReason::MissingConfiguration
                    }
                    AttributionFailure::ConfigurationChanged => {
                        ExclusionReason::ConfigurationChanged
                    }
                    AttributionFailure::NoMatchingProfile => ExclusionReason::NoMatchingProfile,
                    AttributionFailure::AmbiguousProfile => ExclusionReason::AmbiguousProfile,
                }),
                None => Some(ExclusionReason::MissingConfiguration),
            },
        };
        if let Some(reason) = reason {
            *exclusion_counts.entry(reason).or_default() += 1;
            continue;
        }
        let Some(ProfileAttribution::Matched { profile }) = task.attribution else {
            continue;
        };
        let Some(current_profile) = current.get(profile.id.as_str()) else {
            *exclusion_counts
                .entry(ExclusionReason::NoMatchingProfile)
                .or_default() += 1;
            continue;
        };
        let same_identity = profile.declaration().canonical_identity().ok()
            == current_profile.declaration.canonical_identity().ok();
        if !same_identity {
            *exclusion_counts
                .entry(ExclusionReason::NoMatchingProfile)
                .or_default() += 1;
            continue;
        }
        executions
            .entry(profile.id.as_str())
            .or_default()
            .push(Execution {
                task_id,
                started_at: task.started_at.expect("validated above"),
                elapsed_ms: task.elapsed_ms.expect("validated above"),
                completed: true,
                actual_cost: task.actual_cost,
                estimated_cost: None,
                usage: None,
                report_failure: None,
                origin: ExecutionOrigin::DirectObservation,
                tags: task_tags.get(&task_id).copied().unwrap_or(&empty_tags),
            });
    }
    projection.exclusions = exclusion_counts
        .into_iter()
        .map(|(reason, count)| ExclusionSummary { reason, count })
        .collect();
    projection
        .exclusions
        .sort_by_key(|summary| summary.reason as u8);

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
        Segment::Untagged => tags.iter().all(|tag| tag.trim().is_empty()),
        Segment::Tag(tag) => tags.iter().any(|candidate| candidate.trim() == tag),
    }
}

fn score_segment(
    segment: Segment,
    executions: &[&Execution<'_>],
    verdicts: &HashMap<Uuid, Verdict>,
) -> SegmentScore {
    let mut acceptance = AcceptanceAxis::default();
    let mut origins = OriginCounts::default();
    let mut durations = Vec::new();
    let mut duration_latest = None;
    let mut included = HashSet::new();
    for execution in executions {
        match execution.origin {
            ExecutionOrigin::Brokered => origins.brokered += 1,
            ExecutionOrigin::DirectObservation => origins.direct_observation += 1,
        }
        if let Some(verdict) = verdicts.get(&execution.task_id) {
            acceptance.summary.count += 1;
            acceptance.accepted += usize::from(*verdict == Verdict::Accepted);
            acceptance.summary.latest_started_at =
                latest(acceptance.summary.latest_started_at, execution.started_at);
            included.insert(execution.task_id);
            if *verdict == Verdict::Accepted && execution.completed {
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
        .filter(|execution| !included.contains(&execution.task_id))
        .count();
    let costs: Vec<_> = executions
        .iter()
        .filter_map(|execution| execution.actual_cost)
        .collect();
    let currency = costs.first().map(|cost| cost.currency.clone());
    let same_currency = currency
        .as_ref()
        .is_some_and(|currency| costs.iter().all(|cost| &cost.currency == currency));
    let total_minor_units = same_currency.then(|| {
        costs
            .iter()
            .fold(0_u64, |total, cost| total.saturating_add(cost.minor_units))
    });
    let reported = reported_disclosure(executions);
    SegmentScore {
        segment,
        execution_count: executions.len(),
        excluded_count,
        acceptance,
        duration,
        cost: CostAxis {
            summary: AxisSummary {
                count: costs.len(),
                latest_started_at: executions
                    .iter()
                    .filter(|execution| execution.actual_cost.is_some())
                    .fold(None, |latest_at, execution| {
                        latest(latest_at, execution.started_at)
                    }),
            },
            currency: same_currency.then_some(currency).flatten(),
            total_minor_units,
            missing_count: executions.len().saturating_sub(costs.len()),
        },
        reported,
        origins,
    }
}

fn reported_disclosure(executions: &[&Execution<'_>]) -> ReportedDisclosure {
    let brokered = executions
        .iter()
        .filter(|execution| execution.origin == ExecutionOrigin::Brokered);
    let mut disclosure = ReportedDisclosure {
        brokered_count: brokered.clone().count(),
        ..ReportedDisclosure::default()
    };
    let mut estimated_costs = Vec::new();
    for execution in brokered {
        match execution.report_failure {
            Some(ReportFailure::ParseFailure) => disclosure.failures.parse += 1,
            Some(ReportFailure::OutputLimitExceeded) => disclosure.failures.limit += 1,
            Some(ReportFailure::ModelMismatch) => disclosure.failures.model += 1,
            None => {}
        }
        let has_usage = execution.usage.is_some_and(usage_has_reported_value);
        let has_reported_value = execution.estimated_cost.is_some() || has_usage;
        if has_reported_value {
            disclosure.reported_count += 1;
            disclosure.latest_started_at =
                latest(disclosure.latest_started_at, execution.started_at);
        }
        if let Some(estimated_cost) = execution.estimated_cost {
            estimated_costs.push(estimated_cost);
        }
    }
    disclosure.missing_count = disclosure
        .brokered_count
        .saturating_sub(disclosure.reported_count);
    disclosure.estimated_cost = summarize_estimated_cost(&estimated_costs);
    disclosure
}

fn usage_has_reported_value(usage: &TokenUsage) -> bool {
    usage.input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.cached_input_tokens.is_some()
        || usage.reasoning_tokens.is_some()
}

fn summarize_estimated_cost(costs: &[&EstimatedCost]) -> EstimatedCostSummary {
    let Some(first) = costs.first() else {
        return EstimatedCostSummary::None;
    };
    if costs.iter().any(|cost| cost.currency != first.currency) {
        return EstimatedCostSummary::MixedCurrencies { count: costs.len() };
    }
    let mut amount_micros = 0_u64;
    for cost in costs {
        let Some(total) = amount_micros.checked_add(cost.amount_micros) else {
            return EstimatedCostSummary::Overflow;
        };
        amount_micros = total;
    }
    EstimatedCostSummary::Total {
        currency: first.currency.clone(),
        amount_micros,
        count: costs.len(),
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::{Event, EventKind, ProfileSnapshot, RecordedExecutionOutcome, safe_test_profile};

    fn timestamp(value: &str) -> Timestamp {
        value.parse().unwrap()
    }

    fn interactive_profile() -> (crate::ProfileDeclaration, ExecutionProfile) {
        let declaration = crate::ProfileDeclaration {
            name: "Codex interactive".to_owned(),
            cli: crate::CliKind::Codex,
            execution_platform: crate::ExecutionPlatform::Interactive,
            executable: None,
            model: Some("gpt-5".to_owned()),
            args: Vec::new(),
            identity: BTreeMap::from([("permission_mode".to_owned(), "default".to_owned())]),
        };
        let resolved = declaration
            .resolve(PathBuf::from("codex"))
            .expect("profile is valid");
        (declaration, resolved)
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
            task_id: Some(task_id),
            occurred_at: timestamp(at),
            provenance: crate::Provenance::Dock,
            kind: EventKind::Executed {
                origin: crate::ExecutionOrigin::Brokered,
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
                estimated_cost: None,
                usage: None,
                report_failure: None,
            },
        }
    }

    fn reported_execution(
        profile: &ExecutionProfile,
        task_id: Uuid,
        at: &str,
        estimated_cost: Option<crate::EstimatedCost>,
        usage: Option<crate::TokenUsage>,
        report_failure: Option<crate::ReportFailure>,
    ) -> Event {
        let mut event = Event::new(
            task_id,
            EventKind::Executed {
                origin: crate::ExecutionOrigin::Brokered,
                profile: ProfileSnapshot::from(profile),
                working_directory: PathBuf::from("/tmp"),
                started_at: timestamp(at),
                elapsed_ms: 100,
                outcome: RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                cost: None,
                estimated_cost,
                usage,
                report_failure,
            },
        );
        event.occurred_at = timestamp(at);
        event
    }

    fn judged_execution(
        profile: &ExecutionProfile,
        tags: &[&str],
        at: &str,
        accepted: bool,
    ) -> Vec<Event> {
        let task_id = Uuid::now_v7();
        vec![
            task(task_id, at, tags),
            reported_execution(profile, task_id, at, None, None, None),
            judged(
                task_id,
                Uuid::now_v7(),
                if accepted {
                    Verdict::Accepted
                } else {
                    Verdict::Rejected
                },
            ),
        ]
    }

    fn judged(task_id: Uuid, _execution_event_id: Uuid, verdict: Verdict) -> Event {
        Event::new(task_id, EventKind::Judged { verdict })
    }

    #[test]
    fn ignores_advice_events_when_projecting_the_scorecard() {
        let profile = safe_test_profile();
        let task_id = Uuid::now_v7();
        let advice_event_id = Uuid::now_v7();
        let events = vec![
            Event::new(
                task_id,
                EventKind::Advised {
                    referenced_segments: vec![crate::EvidenceSegment::Untagged],
                    outcome: crate::AdviceOutcome::Abstained {
                        reason: crate::AdviceReason::NoEvidence {
                            max_judged: vec![crate::AdviceSegmentCount {
                                segment: crate::EvidenceSegment::Untagged,
                                max_judged: 0,
                            }],
                        },
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
        ];
        let expected = project(&[], std::slice::from_ref(&profile), &[Segment::Untagged]);
        let actual = project(
            &events,
            std::slice::from_ref(&profile),
            &[Segment::Untagged],
        );
        assert_eq!(actual, expected);
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
            &segments_for_tags(&["rust".to_owned(), "docs".to_owned()]),
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
            overall.acceptance.summary.latest_started_at,
            Some(timestamp("2026-01-01T00:00:03Z"))
        );
        assert_eq!(
            (overall.duration.median_ms, overall.duration.summary.count),
            (None, 0)
        );
        assert_eq!(overall.excluded_count, 0);
        assert_eq!(projection.scorecards[0].scores[1].execution_count, 2);
        assert_eq!(projection.scorecards[0].scores[2].execution_count, 2);
    }

    #[test]
    fn duration_uses_only_completed_crew_member_runtime() {
        let task_id = Uuid::now_v7();
        let cancelled = Uuid::now_v7();
        let accepted_earlier_task = Uuid::now_v7();
        let accepted_without_task = Uuid::now_v7();
        let rejected_task = Uuid::now_v7();
        let accepted_earlier = Uuid::now_v7();
        let accepted_without_task_event = Uuid::now_v7();
        let later_rejected = Uuid::now_v7();
        let events = vec![
            task(task_id, "2026-01-01T00:00:00Z", &[]),
            execution(task_id, cancelled, "2026-01-01T00:00:01Z", 10, false),
            execution(
                accepted_earlier_task,
                accepted_earlier,
                "2026-01-01T00:00:00Z",
                6,
                true,
            ),
            judged(accepted_earlier_task, accepted_earlier, Verdict::Accepted),
            execution(
                accepted_without_task,
                accepted_without_task_event,
                "2026-01-01T00:00:02Z",
                14,
                true,
            ),
            judged(
                accepted_without_task,
                accepted_without_task_event,
                Verdict::Accepted,
            ),
            execution(
                rejected_task,
                later_rejected,
                "2026-01-01T00:00:03Z",
                100,
                true,
            ),
            judged(rejected_task, later_rejected, Verdict::Rejected),
        ];
        let projection = project(&events, &[safe_test_profile()], &[Segment::Overall]);
        let score = &projection.scorecards[0].scores[0];
        assert_eq!(score.acceptance.summary.count, 3);
        assert_eq!(
            score.acceptance.summary.latest_started_at,
            Some(timestamp("2026-01-01T00:00:03Z"))
        );
        assert_eq!(score.duration.median_ms, Some(10));
        assert_eq!(score.duration.summary.count, 2);
        assert_eq!(
            score.duration.summary.latest_started_at,
            Some(timestamp("2026-01-01T00:00:02Z"))
        );
        assert_eq!(score.excluded_count, 1);
        assert_eq!(score.cost.summary.count, 0);
        assert_eq!(score.cost.summary.latest_started_at, None);
    }

    #[test]
    fn judged_cancelled_executions_affect_acceptance_but_not_duration() {
        let task_id = Uuid::now_v7();
        let completed = Uuid::now_v7();
        let cancelled = Uuid::now_v7();
        let events = vec![
            task(task_id, "2026-01-01T00:00:00Z", &[]),
            execution(task_id, completed, "2026-01-01T00:00:01Z", 20, true),
            judged(task_id, completed, Verdict::Accepted),
            execution(task_id, cancelled, "2026-01-01T00:00:02Z", 30, false),
            judged(task_id, cancelled, Verdict::Accepted),
        ];

        let projection = project(&events, &[safe_test_profile()], &[Segment::Overall]);
        let score = &projection.scorecards[0].scores[0];

        assert_eq!(score.execution_count, 2);
        assert_eq!(score.acceptance.summary.count, 2);
        assert_eq!(score.acceptance.accepted, 2);
        assert_eq!(
            score.acceptance.summary.latest_started_at,
            Some(timestamp("2026-01-01T00:00:02Z"))
        );
        assert_eq!(score.duration.summary.count, 1);
        assert_eq!(score.duration.median_ms, Some(20));
        assert_eq!(
            score.duration.summary.latest_started_at,
            Some(timestamp("2026-01-01T00:00:01Z"))
        );
        assert_eq!(score.excluded_count, 0);
    }

    #[test]
    fn connects_an_actual_execution_cost_to_the_cost_axis() {
        let task_id = Uuid::now_v7();
        let mut event = execution(task_id, Uuid::now_v7(), "2026-01-01T00:00:00Z", 20, true);
        let EventKind::Executed { cost, .. } = &mut event.kind else {
            unreachable!()
        };
        *cost = Some(ActualCost {
            currency: "USD".to_owned(),
            minor_units: 42,
        });

        let projection = project(&[event], &[safe_test_profile()], &[Segment::Overall]);
        let score = &projection.scorecards[0].scores[0];
        assert_eq!(score.cost.summary.count, 1);
        assert_eq!(score.cost.total_minor_units, Some(42));
        assert_eq!(score.cost.currency.as_deref(), Some("USD"));
        assert_eq!(score.cost.missing_count, 0);
    }

    #[test]
    fn reported_values_do_not_change_axes_or_advice() {
        let task_id = Uuid::now_v7();
        let baseline = execution(task_id, Uuid::now_v7(), "2026-01-01T00:00:00Z", 20, true);
        let mut reported = baseline.clone();
        let EventKind::Executed {
            estimated_cost,
            usage,
            report_failure,
            ..
        } = &mut reported.kind
        else {
            unreachable!()
        };
        *estimated_cost = Some(crate::EstimatedCost {
            currency: "USD".to_owned(),
            amount_micros: 1,
        });
        *usage = Some(crate::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(20),
            cached_input_tokens: Some(3),
            reasoning_tokens: Some(4),
        });
        *report_failure = Some(crate::ReportFailure::ParseFailure);
        let profile = safe_test_profile();
        let baseline_projection = project(
            std::slice::from_ref(&baseline),
            std::slice::from_ref(&profile),
            &[Segment::Overall],
        );
        let reported_projection = project(
            std::slice::from_ref(&reported),
            std::slice::from_ref(&profile),
            &[Segment::Overall],
        );
        let baseline_score = &baseline_projection.scorecards[0].scores[0];
        let reported_score = &reported_projection.scorecards[0].scores[0];
        assert_eq!(
            reported_score.execution_count,
            baseline_score.execution_count
        );
        assert_eq!(reported_score.excluded_count, baseline_score.excluded_count);
        assert_eq!(reported_score.acceptance, baseline_score.acceptance);
        assert_eq!(reported_score.duration, baseline_score.duration);
        assert_eq!(reported_score.cost, baseline_score.cost);
        assert_eq!(reported_score.origins, baseline_score.origins);
        assert_ne!(reported_score.reported, baseline_score.reported);
        assert_eq!(
            crate::advise(&reported_projection, &[], std::slice::from_ref(&profile)),
            crate::advise(&baseline_projection, &[], std::slice::from_ref(&profile))
        );
    }

    #[test]
    fn aggregates_reported_values_by_profile_and_segment() {
        let first = safe_test_profile();
        let mut declaration = crate::safe_test_declaration();
        declaration.name = "Second".to_owned();
        declaration.model = Some("second".to_owned());
        let second = declaration.resolve(PathBuf::from("/usr/bin/true")).unwrap();
        let first_task = Uuid::now_v7();
        let second_task = Uuid::now_v7();
        let third_task = Uuid::now_v7();
        let events = vec![
            task(first_task, "2026-01-01T00:00:00Z", &["rust"]),
            reported_execution(
                &first,
                first_task,
                "2026-01-01T00:00:01Z",
                Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 1_000_001,
                }),
                None,
                None,
            ),
            task(second_task, "2026-01-02T00:00:00Z", &["docs"]),
            reported_execution(
                &first,
                second_task,
                "2026-01-02T00:00:01Z",
                None,
                Some(crate::TokenUsage {
                    output_tokens: Some(4),
                    ..crate::TokenUsage::default()
                }),
                None,
            ),
            task(third_task, "2026-01-03T00:00:00Z", &["rust"]),
            reported_execution(
                &second,
                third_task,
                "2026-01-03T00:00:01Z",
                Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 2_000_000,
                }),
                None,
                None,
            ),
        ];
        let segments = [
            Segment::Overall,
            Segment::Tag("rust".to_owned()),
            Segment::Tag("docs".to_owned()),
        ];
        let projection = project(&events, &[first.clone(), second.clone()], &segments);
        let first_scores = &projection.scorecards[0].scores;
        assert_eq!(projection.scorecards[0].profile_id, first.id);
        assert_eq!(first_scores[0].reported.brokered_count, 2);
        assert_eq!(first_scores[0].reported.reported_count, 2);
        assert_eq!(first_scores[0].reported.missing_count, 0);
        assert_eq!(
            first_scores[0].reported.latest_started_at,
            Some(timestamp("2026-01-02T00:00:01Z"))
        );
        assert_eq!(first_scores[1].reported.brokered_count, 1);
        assert_eq!(first_scores[1].reported.reported_count, 1);
        assert_eq!(first_scores[2].reported.brokered_count, 1);
        assert_eq!(first_scores[2].reported.reported_count, 1);
        assert_eq!(
            first_scores[0].reported.estimated_cost,
            EstimatedCostSummary::Total {
                currency: "USD".to_owned(),
                amount_micros: 1_000_001,
                count: 1,
            }
        );
        let second_scores = &projection.scorecards[1].scores;
        assert_eq!(projection.scorecards[1].profile_id, second.id);
        assert_eq!(second_scores[0].reported.brokered_count, 1);
        assert_eq!(second_scores[0].reported.reported_count, 1);
        assert_eq!(second_scores[1].reported.brokered_count, 1);
        assert_eq!(second_scores[2].reported.brokered_count, 0);
        assert_eq!(second_scores[2].reported, ReportedDisclosure::default());
    }

    #[test]
    fn counts_only_brokered_executions_and_marks_direct_only_as_not_applicable() {
        let profile = safe_test_profile();
        let task_id = Uuid::now_v7();
        let mut direct = reported_execution(
            &profile,
            task_id,
            "2026-01-01T00:00:00Z",
            Some(crate::EstimatedCost {
                currency: "USD".to_owned(),
                amount_micros: 10,
            }),
            Some(crate::TokenUsage {
                input_tokens: Some(1),
                ..crate::TokenUsage::default()
            }),
            Some(crate::ReportFailure::ParseFailure),
        );
        let EventKind::Executed { origin, .. } = &mut direct.kind else {
            unreachable!()
        };
        *origin = ExecutionOrigin::DirectObservation;
        let events = vec![task(task_id, "2026-01-01T00:00:00Z", &[]), direct];
        let projection = project(&events, std::slice::from_ref(&profile), &[Segment::Overall]);
        let reported = &projection.scorecards[0].scores[0].reported;
        assert_eq!(reported, &ReportedDisclosure::default());
    }

    #[test]
    fn reported_disclosure_counts_only_brokered_execution_with_attributed_direct_observation() {
        let (_, profile) = interactive_profile();
        let brokered_task_id = Uuid::now_v7();
        let direct_task_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let direct_event = |kind, at: &str| {
            let mut event = Event::for_task(direct_task_id, crate::Provenance::CliHook, kind);
            event.occurred_at = timestamp(at);
            event
        };
        let events = vec![
            task(brokered_task_id, "2026-01-01T00:00:00Z", &["rust"]),
            reported_execution(
                &profile,
                brokered_task_id,
                "2026-01-01T00:00:01Z",
                Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 7,
                }),
                None,
                Some(crate::ReportFailure::ParseFailure),
            ),
            direct_event(
                EventKind::ObservedTaskStarted {
                    session_id,
                    origin: ExecutionOrigin::DirectObservation,
                    tags: vec!["rust".to_owned()],
                    prompt: None,
                    prompt_chars: 0,
                },
                "2026-01-01T00:00:02Z",
            ),
            direct_event(
                EventKind::ObservedTaskEnded {
                    session_id,
                    status: crate::ObservedTaskStatus::Completed,
                },
                "2026-01-01T00:00:03Z",
            ),
            direct_event(
                EventKind::MeasurementObserved {
                    session_id,
                    elapsed_ms: Some(200),
                    actual_cost: None,
                },
                "2026-01-01T00:00:04Z",
            ),
            direct_event(
                EventKind::ProfileAttributed {
                    session_id,
                    attribution: crate::ProfileAttribution::Matched {
                        profile: ProfileSnapshot::from(&profile),
                    },
                },
                "2026-01-01T00:00:05Z",
            ),
            judged(direct_task_id, Uuid::now_v7(), Verdict::Accepted),
        ];

        let projection = project(
            &events,
            std::slice::from_ref(&profile),
            &[Segment::Tag("rust".to_owned())],
        );
        let score = &projection.scorecards[0].scores[0];

        assert_eq!(score.execution_count, 2);
        assert_eq!(
            score.origins,
            OriginCounts {
                brokered: 1,
                direct_observation: 1,
            }
        );
        assert_eq!(score.reported.brokered_count, 1);
        assert_eq!(score.reported.reported_count, 1);
        assert_eq!(score.reported.missing_count, 0);
        assert_eq!(
            score.reported.failures,
            ReportFailureCounts {
                parse: 1,
                limit: 0,
                model: 0,
            }
        );
    }

    #[test]
    fn counts_usage_without_estimated_cost_but_not_an_empty_usage_object() {
        let profile = safe_test_profile();
        let valued_task = Uuid::now_v7();
        let empty_usage_task = Uuid::now_v7();
        let missing_task = Uuid::now_v7();
        let events = vec![
            task(valued_task, "2026-01-01T00:00:00Z", &[]),
            reported_execution(
                &profile,
                valued_task,
                "2026-01-01T00:00:01Z",
                None,
                Some(crate::TokenUsage {
                    input_tokens: Some(0),
                    ..crate::TokenUsage::default()
                }),
                None,
            ),
            task(empty_usage_task, "2026-01-02T00:00:00Z", &[]),
            reported_execution(
                &profile,
                empty_usage_task,
                "2026-01-02T00:00:01Z",
                None,
                Some(crate::TokenUsage::default()),
                None,
            ),
            task(missing_task, "2026-01-03T00:00:00Z", &[]),
            reported_execution(
                &profile,
                missing_task,
                "2026-01-03T00:00:01Z",
                None,
                None,
                None,
            ),
        ];
        let projection = project(&events, std::slice::from_ref(&profile), &[Segment::Overall]);
        let reported = &projection.scorecards[0].scores[0].reported;
        assert_eq!(reported.brokered_count, 3);
        assert_eq!(reported.reported_count, 1);
        assert_eq!(reported.missing_count, 2);
        assert_eq!(reported.estimated_cost, EstimatedCostSummary::None);
        assert_eq!(
            reported.latest_started_at,
            Some(timestamp("2026-01-01T00:00:01Z"))
        );
    }

    #[test]
    fn summarizes_zero_mixed_and_overflowing_estimated_costs() {
        let profile = safe_test_profile();
        let cases: [(
            &str,
            Vec<Option<crate::EstimatedCost>>,
            EstimatedCostSummary,
        ); 5] = [
            ("none", vec![None], EstimatedCostSummary::None),
            (
                "zero",
                vec![Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 0,
                })],
                EstimatedCostSummary::Total {
                    currency: "USD".to_owned(),
                    amount_micros: 0,
                    count: 1,
                },
            ),
            (
                "same currency nonzero",
                vec![
                    Some(crate::EstimatedCost {
                        currency: "USD".to_owned(),
                        amount_micros: 1_000_001,
                    }),
                    Some(crate::EstimatedCost {
                        currency: "USD".to_owned(),
                        amount_micros: 2_000_002,
                    }),
                ],
                EstimatedCostSummary::Total {
                    currency: "USD".to_owned(),
                    amount_micros: 3_000_003,
                    count: 2,
                },
            ),
            (
                "mixed",
                vec![
                    Some(crate::EstimatedCost {
                        currency: "USD".to_owned(),
                        amount_micros: 1,
                    }),
                    Some(crate::EstimatedCost {
                        currency: "EUR".to_owned(),
                        amount_micros: 2,
                    }),
                ],
                EstimatedCostSummary::MixedCurrencies { count: 2 },
            ),
            (
                "overflow",
                vec![
                    Some(crate::EstimatedCost {
                        currency: "USD".to_owned(),
                        amount_micros: u64::MAX,
                    }),
                    Some(crate::EstimatedCost {
                        currency: "USD".to_owned(),
                        amount_micros: 1,
                    }),
                ],
                EstimatedCostSummary::Overflow,
            ),
        ];
        for (name, costs, expected) in cases {
            let mut events = Vec::new();
            for (index, estimated_cost) in costs.into_iter().enumerate() {
                let task_id = Uuid::now_v7();
                let at = format!("2026-01-0{}T00:00:00Z", index + 1);
                events.push(task(task_id, &at, &[]));
                events.push(reported_execution(
                    &profile,
                    task_id,
                    &at,
                    estimated_cost,
                    None,
                    None,
                ));
            }
            let projection = project(&events, std::slice::from_ref(&profile), &[Segment::Overall]);
            assert_eq!(
                projection.scorecards[0].scores[0].reported.estimated_cost, expected,
                "case={name}"
            );
        }
    }

    #[test]
    fn advice_candidate_and_evidence_ignore_reported_execution_values() {
        let leader = {
            let mut declaration = crate::safe_test_declaration();
            declaration.name = "leader".to_owned();
            declaration.model = Some("leader".to_owned());
            declaration.resolve(PathBuf::from("/usr/bin/true")).unwrap()
        };
        let other = {
            let mut declaration = crate::safe_test_declaration();
            declaration.name = "other".to_owned();
            declaration.model = Some("other".to_owned());
            declaration.resolve(PathBuf::from("/usr/bin/true")).unwrap()
        };
        let profiles = [leader.clone(), other.clone()];
        assert!(
            profiles
                .iter()
                .all(|profile| profile.execution_platform() == crate::ExecutionPlatform::Headless)
        );

        let mut baseline_events = Vec::new();
        for (tag, month) in [("rust", "01"), ("docs", "02")] {
            for index in 0..3 {
                baseline_events.extend(judged_execution(
                    &leader,
                    &[tag],
                    &format!("2026-{month}-{:02}T00:00:00Z", index + 1),
                    true,
                ));
                baseline_events.extend(judged_execution(
                    &other,
                    &[tag],
                    &format!("2026-{month}-{:02}T01:00:00Z", index + 1),
                    true,
                ));
            }
            baseline_events.extend(judged_execution(
                &other,
                &[tag],
                &format!("2026-{month}-04T01:00:00Z"),
                false,
            ));
        }

        let mut reported_events = baseline_events.clone();
        let mut changed_executions = 0;
        for event in &mut reported_events {
            let EventKind::Executed {
                estimated_cost,
                usage,
                report_failure,
                ..
            } = &mut event.kind
            else {
                continue;
            };
            *estimated_cost = Some(crate::EstimatedCost {
                currency: "USD".to_owned(),
                amount_micros: 1,
            });
            *usage = Some(crate::TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                cached_input_tokens: None,
                reasoning_tokens: None,
            });
            *report_failure = Some(crate::ReportFailure::ParseFailure);
            changed_executions += 1;
        }
        assert_eq!(changed_executions, 14);

        let segments = [
            Segment::Tag("rust".to_owned()),
            Segment::Tag("docs".to_owned()),
        ];
        let tags = ["rust".to_owned(), "docs".to_owned()];
        let baseline_projection = project(&baseline_events, &profiles, &segments);
        let reported_projection = project(&reported_events, &profiles, &segments);
        let baseline_advice = crate::advise(&baseline_projection, &tags, &profiles);
        let reported_advice = crate::advise(&reported_projection, &tags, &profiles);

        let (baseline_profile_id, baseline_evidence) = match &baseline_advice.outcome {
            crate::AdviceOutcome::Proposed {
                profile_id,
                evidence,
            } => (profile_id, evidence),
            outcome => panic!("expected Proposed advice, got {outcome:?}"),
        };
        let (reported_profile_id, reported_evidence) = match &reported_advice.outcome {
            crate::AdviceOutcome::Proposed {
                profile_id,
                evidence,
            } => (profile_id, evidence),
            outcome => panic!("expected Proposed advice, got {outcome:?}"),
        };
        assert_eq!(baseline_profile_id, &leader.id);
        assert_eq!(reported_profile_id, baseline_profile_id);
        assert_eq!(reported_evidence, baseline_evidence);
    }

    #[test]
    fn counts_report_failures_independently_from_missing_reported_values() {
        let profile = safe_test_profile();
        let parse_task = Uuid::now_v7();
        let limit_task = Uuid::now_v7();
        let model_task = Uuid::now_v7();
        let events = vec![
            task(parse_task, "2026-01-01T00:00:00Z", &[]),
            reported_execution(
                &profile,
                parse_task,
                "2026-01-01T00:00:01Z",
                None,
                Some(crate::TokenUsage {
                    output_tokens: Some(5),
                    ..crate::TokenUsage::default()
                }),
                Some(crate::ReportFailure::ParseFailure),
            ),
            task(limit_task, "2026-01-02T00:00:00Z", &[]),
            reported_execution(
                &profile,
                limit_task,
                "2026-01-02T00:00:01Z",
                None,
                None,
                Some(crate::ReportFailure::OutputLimitExceeded),
            ),
            task(model_task, "2026-01-03T00:00:00Z", &[]),
            reported_execution(
                &profile,
                model_task,
                "2026-01-03T00:00:01Z",
                Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 0,
                }),
                None,
                Some(crate::ReportFailure::ModelMismatch),
            ),
        ];
        let projection = project(&events, std::slice::from_ref(&profile), &[Segment::Overall]);
        let reported = &projection.scorecards[0].scores[0].reported;
        assert_eq!(reported.brokered_count, 3);
        assert_eq!(reported.reported_count, 2);
        assert_eq!(reported.missing_count, 1);
        assert_eq!(reported.failures.parse, 1);
        assert_eq!(reported.failures.limit, 1);
        assert_eq!(reported.failures.model, 1);
        assert_eq!(
            reported.latest_started_at,
            Some(timestamp("2026-01-03T00:00:01Z"))
        );
    }

    #[test]
    fn completed_attributed_direct_task_joins_the_profile_scorecard() {
        let directory = tempfile::tempdir().unwrap();
        let log = crate::EventLog::new(directory.path().join("events.jsonl"));
        let (declaration, profile) = interactive_profile();
        let session = crate::apply_observation(
            &log,
            &[],
            crate::ObservationCommand::ObserveSession {
                session_id: None,
                cli: crate::CliKind::Codex,
                phase: crate::SessionPhase::Started,
                external_session_id: Some("codex-session".to_owned()),
                working_directory: Some(PathBuf::from("/tmp/project")),
                configuration: None,
                provenance: crate::Provenance::CliHook,
            },
        )
        .unwrap()
        .session_id;
        let configuration = crate::configuration_for_profile(&declaration);
        let task = crate::apply_observation(
            &log,
            std::slice::from_ref(&declaration),
            crate::ObservationCommand::StartTask {
                session_id: session,
                tags: vec!["rust".to_owned()],
                prompt: None,
                prompt_chars: 0,
                configuration: Some(configuration.clone()),
            },
        )
        .unwrap()
        .task_id
        .unwrap();
        crate::apply_observation(
            &log,
            std::slice::from_ref(&declaration),
            crate::ObservationCommand::EndTask {
                session_id: session,
                status: crate::ObservedTaskStatus::Completed,
                configuration: Some(configuration),
            },
        )
        .unwrap();
        log.append_judgement_if_current(task, None, Verdict::Accepted)
            .unwrap();

        let projection = project(
            &log.read_all().unwrap().events,
            &[profile],
            &[Segment::Overall, Segment::Tag("rust".to_owned())],
        );
        assert!(projection.exclusions.is_empty());
        for score in &projection.scorecards[0].scores {
            assert_eq!(score.execution_count, 1);
            assert_eq!(score.acceptance.accepted, 1);
            assert_eq!(score.duration.summary.count, 1);
            assert_eq!(score.cost.missing_count, 1);
            assert_eq!(score.reported, ReportedDisclosure::default());
        }
    }

    #[test]
    fn direct_task_exclusions_keep_distinct_reasons() {
        let task_ids: Vec<_> = (0..7).map(|_| Uuid::now_v7()).collect();
        let session = Uuid::now_v7();
        let configuration = crate::ObservedConfiguration {
            cli: crate::CliKind::Codex,
            execution_platform: crate::ExecutionPlatform::Interactive,
            model: crate::ObservedValue::Missing,
            identity: Default::default(),
        };
        let mut events = Vec::new();
        for task_id in &task_ids {
            events.push(Event::for_task(
                *task_id,
                crate::Provenance::ManualMark,
                EventKind::ObservedTaskStarted {
                    session_id: session,
                    origin: crate::ExecutionOrigin::DirectObservation,
                    tags: Vec::new(),
                    prompt: None,
                    prompt_chars: 0,
                },
            ));
        }
        events.extend([
            Event::for_task(
                task_ids[1],
                crate::Provenance::ManualMark,
                EventKind::ObservedTaskEnded {
                    session_id: session,
                    status: crate::ObservedTaskStatus::Interrupted,
                },
            ),
            Event::for_task(
                task_ids[2],
                crate::Provenance::ManualMark,
                EventKind::ObservedTaskEnded {
                    session_id: session,
                    status: crate::ObservedTaskStatus::Completed,
                },
            ),
            Event::for_task(
                task_ids[2],
                crate::Provenance::Dock,
                EventKind::MeasurementObserved {
                    session_id: session,
                    elapsed_ms: None,
                    actual_cost: None,
                },
            ),
            Event::for_task(
                task_ids[3],
                crate::Provenance::ManualMark,
                EventKind::ObservedTaskEnded {
                    session_id: session,
                    status: crate::ObservedTaskStatus::Completed,
                },
            ),
            Event::for_task(
                task_ids[3],
                crate::Provenance::Dock,
                EventKind::MeasurementObserved {
                    session_id: session,
                    elapsed_ms: Some(10),
                    actual_cost: None,
                },
            ),
            Event::for_task(
                task_ids[3],
                crate::Provenance::Dock,
                EventKind::ProfileAttributed {
                    session_id: session,
                    attribution: crate::ProfileAttribution::Unattributed {
                        reason: crate::AttributionFailure::MissingConfiguration,
                    },
                },
            ),
            Event::for_task(
                task_ids[3],
                crate::Provenance::ManualMark,
                EventKind::ConfigurationObserved {
                    session_id: session,
                    boundary: crate::ConfigurationBoundary::Start,
                    configuration,
                },
            ),
        ]);
        for (task_id, reason) in task_ids[4..].iter().zip([
            crate::AttributionFailure::ConfigurationChanged,
            crate::AttributionFailure::NoMatchingProfile,
            crate::AttributionFailure::AmbiguousProfile,
        ]) {
            events.extend([
                Event::for_task(
                    *task_id,
                    crate::Provenance::ManualMark,
                    EventKind::ObservedTaskEnded {
                        session_id: session,
                        status: crate::ObservedTaskStatus::Completed,
                    },
                ),
                Event::for_task(
                    *task_id,
                    crate::Provenance::Dock,
                    EventKind::MeasurementObserved {
                        session_id: session,
                        elapsed_ms: Some(10),
                        actual_cost: None,
                    },
                ),
                Event::for_task(
                    *task_id,
                    crate::Provenance::Dock,
                    EventKind::ProfileAttributed {
                        session_id: session,
                        attribution: crate::ProfileAttribution::Unattributed { reason },
                    },
                ),
            ]);
        }
        let profile = safe_test_profile();
        let projection = project(&events, std::slice::from_ref(&profile), &[Segment::Overall]);
        assert_eq!(
            projection.exclusions,
            vec![
                ExclusionSummary {
                    reason: ExclusionReason::Ongoing,
                    count: 1
                },
                ExclusionSummary {
                    reason: ExclusionReason::Interrupted,
                    count: 1
                },
                ExclusionSummary {
                    reason: ExclusionReason::InvalidTiming,
                    count: 1
                },
                ExclusionSummary {
                    reason: ExclusionReason::MissingConfiguration,
                    count: 1
                },
                ExclusionSummary {
                    reason: ExclusionReason::ConfigurationChanged,
                    count: 1
                },
                ExclusionSummary {
                    reason: ExclusionReason::NoMatchingProfile,
                    count: 1
                },
                ExclusionSummary {
                    reason: ExclusionReason::AmbiguousProfile,
                    count: 1
                },
            ]
        );
        assert_eq!(
            projection.scorecards[0].scores[0].reported,
            ReportedDisclosure::default()
        );
    }

    #[test]
    fn rejects_inconsistent_snapshot_or_current_profile_identity() {
        let task_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let event = execution(task_id, event_id, "2026-01-01T00:00:00Z", 1, true);
        let mut inconsistent_snapshot = event.clone();
        let EventKind::Executed { profile, .. } = &mut inconsistent_snapshot.kind else {
            unreachable!()
        };
        profile.model = Some("changed".to_owned());
        let mut inconsistent_current = safe_test_profile();
        inconsistent_current.declaration.model = Some("changed".to_owned());

        for (event, current) in [
            (inconsistent_snapshot, safe_test_profile()),
            (event, inconsistent_current),
        ] {
            let projection = project(&[event], &[current], &[Segment::Overall]);
            assert_eq!(projection.scorecards[0].scores[0].execution_count, 0);
            assert_eq!(projection.warnings.len(), 1);
        }
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
    fn escapes_terminal_controls() {
        assert_eq!(
            escape_terminal("a\\\n\r\t\u{1b}雪"),
            "a\\\\\\n\\r\\t\\u{1b}雪"
        );
    }

    #[test]
    fn builds_and_projects_tag_segments() {
        for (tags, expected) in [
            (
                Vec::<String>::new(),
                vec![Segment::Overall, Segment::Untagged],
            ),
            (
                vec!["rust".to_owned(), "docs".to_owned(), "rust".to_owned()],
                vec![
                    Segment::Overall,
                    Segment::Tag("rust".to_owned()),
                    Segment::Tag("docs".to_owned()),
                ],
            ),
            (
                vec![" rust ".to_owned()],
                vec![Segment::Overall, Segment::Tag("rust".to_owned())],
            ),
            (
                vec!["  ".to_owned(), "\t".to_owned()],
                vec![Segment::Overall, Segment::Untagged],
            ),
            (
                vec!["Rust".to_owned()],
                vec![Segment::Overall, Segment::Tag("Rust".to_owned())],
            ),
        ] {
            assert_eq!(segments_for_tags(&tags), expected);
        }

        for (segment, tags, expected) in [
            (
                Segment::Tag("rust".to_owned()),
                vec![" rust ".to_owned()],
                true,
            ),
            (
                Segment::Tag("rust".to_owned()),
                vec!["Rust".to_owned()],
                false,
            ),
            (
                Segment::Untagged,
                vec![" ".to_owned(), "\t".to_owned()],
                true,
            ),
            (
                Segment::Untagged,
                vec!["rust".to_owned(), " ".to_owned()],
                false,
            ),
        ] {
            assert_eq!(matches_segment(&segment, &tags), expected);
        }

        let task_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let events = [
            task(task_id, "2026-01-01T00:00:00Z", &[]),
            execution(task_id, event_id, "2026-01-01T00:00:01Z", 1, true),
        ];
        let segments = [
            Segment::Overall,
            Segment::Untagged,
            Segment::Tag("rust".to_owned()),
        ];
        let projection = project(&events, &[safe_test_profile()], &segments);
        let counts: Vec<_> = projection.scorecards[0]
            .scores
            .iter()
            .map(|score| score.execution_count)
            .collect();
        assert_eq!(counts, [1, 1, 0]);
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

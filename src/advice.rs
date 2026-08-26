use std::{cmp::Ordering, collections::HashSet};

use crate::{
    AdviceEvidence, AdviceOutcome, AdviceReason, AdviceSegmentCount, EvidenceAcceptance,
    EvidenceAxisSummary, EvidenceCost, EvidenceDuration, EvidenceSegment, ExecutionPlatform,
    ExecutionProfile, Projection, Segment, SegmentScore,
};

pub const ADVICE_MIN_JUDGED: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub referenced_segments: Vec<Segment>,
    pub outcome: AdviceOutcome,
    pub threshold: usize,
}

impl Advice {
    pub fn event_kind(&self) -> crate::EventKind {
        crate::EventKind::Advised {
            referenced_segments: self
                .referenced_segments
                .iter()
                .map(evidence_segment)
                .collect(),
            outcome: self.outcome.clone(),
            threshold: count(self.threshold),
        }
    }

    pub fn proposed_profile_id(&self) -> Option<&str> {
        match &self.outcome {
            AdviceOutcome::Proposed { profile_id, .. } => Some(profile_id),
            AdviceOutcome::Abstained { .. } => None,
        }
    }

    pub fn evidence(&self) -> Option<&[AdviceEvidence]> {
        match &self.outcome {
            AdviceOutcome::Proposed { evidence, .. } => Some(evidence),
            AdviceOutcome::Abstained { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&AdviceReason> {
        match &self.outcome {
            AdviceOutcome::Proposed { .. } => None,
            AdviceOutcome::Abstained { reason } => Some(reason),
        }
    }
}

pub fn advise(
    projection: &Projection,
    tags: &[String],
    selectable_profiles: &[ExecutionProfile],
) -> Advice {
    let referenced_segments = reference_segments(tags);
    let mut seen_profiles = HashSet::new();
    let profiles = selectable_profiles
        .iter()
        .filter(|profile| profile.execution_platform() == ExecutionPlatform::Headless)
        .filter_map(|profile| {
            seen_profiles
                .insert(profile.id.as_str())
                .then_some(profile.id.as_str())
        })
        .collect::<Vec<_>>();

    let scores = profiles
        .iter()
        .map(|profile_id| {
            referenced_segments
                .iter()
                .map(|segment| {
                    projection
                        .scorecards
                        .iter()
                        .find(|card| card.profile_id == *profile_id)
                        .and_then(|card| card.scores.iter().find(|score| score.segment == *segment))
                        .cloned()
                        .unwrap_or_else(|| empty_score(segment.clone()))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let max_judged = referenced_segments
        .iter()
        .enumerate()
        .map(|(index, segment)| AdviceSegmentCount {
            segment: evidence_segment(segment),
            max_judged: scores
                .iter()
                .map(|profile_scores| profile_scores[index].acceptance.summary.count)
                .max()
                .map(count)
                .unwrap_or(0),
        })
        .collect::<Vec<_>>();

    let base = |outcome| Advice {
        referenced_segments: referenced_segments.clone(),
        outcome,
        threshold: ADVICE_MIN_JUDGED,
    };

    if max_judged.iter().any(|summary| summary.max_judged == 0) {
        return base(AdviceOutcome::Abstained {
            reason: AdviceReason::NoEvidence { max_judged },
        });
    }

    let qualified: Vec<_> = profiles
        .iter()
        .enumerate()
        .filter(|(profile_index, _)| {
            scores[*profile_index]
                .iter()
                .all(|score| score.acceptance.summary.count >= ADVICE_MIN_JUDGED)
        })
        .collect();

    if qualified.is_empty() {
        return base(AdviceOutcome::Abstained {
            reason: AdviceReason::InsufficientEvidence { max_judged },
        });
    }

    let mut leaders = Vec::new();
    for (segment_index, _) in scores[qualified[0].0].iter().enumerate() {
        let mut segment_leaders: Vec<usize> = Vec::new();
        for &(profile_index, _) in &qualified {
            let score = &scores[profile_index][segment_index];
            let Some(&leader_index) = segment_leaders.first() else {
                segment_leaders.push(profile_index);
                continue;
            };
            match compare_acceptance(
                &score.acceptance,
                &scores[leader_index][segment_index].acceptance,
            ) {
                Ordering::Greater => {
                    segment_leaders.clear();
                    segment_leaders.push(profile_index);
                }
                Ordering::Equal => segment_leaders.push(profile_index),
                Ordering::Less => {}
            }
        }
        leaders.push(segment_leaders);
    }

    let mut unique_leaders = leaders
        .iter()
        .flat_map(|leaders| leaders.iter())
        .copied()
        .collect::<Vec<_>>();
    unique_leaders.sort_unstable();
    unique_leaders.dedup();
    let unique_leader = (unique_leaders.len() == 1).then_some(unique_leaders[0]);
    let Some(leader_index) = unique_leader else {
        let mut profile_ids = leaders
            .iter()
            .flat_map(|leaders| leaders.iter())
            .map(|index| profiles[*index].to_owned())
            .collect::<Vec<_>>();
        profile_ids.sort();
        profile_ids.dedup();
        return base(AdviceOutcome::Abstained {
            reason: AdviceReason::NoUniqueLeader { profile_ids },
        });
    };

    let evidence = scores[leader_index]
        .iter()
        .map(to_evidence)
        .collect::<Vec<_>>();
    base(AdviceOutcome::Proposed {
        profile_id: profiles[leader_index].to_owned(),
        evidence,
    })
}

fn reference_segments(tags: &[String]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut seen = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if !tag.is_empty() && !seen.contains(&tag) {
            seen.push(tag);
            segments.push(Segment::Tag(tag.to_owned()));
        }
    }
    if segments.is_empty() {
        vec![Segment::Untagged]
    } else {
        segments
    }
}

fn compare_acceptance(left: &crate::AcceptanceAxis, right: &crate::AcceptanceAxis) -> Ordering {
    ((left.accepted as u128) * (right.summary.count as u128))
        .cmp(&((right.accepted as u128) * (left.summary.count as u128)))
}

fn to_evidence(score: &SegmentScore) -> AdviceEvidence {
    AdviceEvidence {
        segment: evidence_segment(&score.segment),
        acceptance: EvidenceAcceptance {
            summary: evidence_summary(&score.acceptance.summary),
            accepted: count(score.acceptance.accepted),
        },
        duration: EvidenceDuration {
            summary: evidence_summary(&score.duration.summary),
            median_ms: score.duration.median_ms,
        },
        cost: EvidenceCost {
            summary: evidence_summary(&score.cost.summary),
            currency: score.cost.currency.clone(),
            total_minor_units: score.cost.total_minor_units,
            missing_count: count(score.cost.missing_count),
        },
    }
}

fn evidence_summary(summary: &crate::AxisSummary) -> EvidenceAxisSummary {
    EvidenceAxisSummary {
        count: count(summary.count),
        latest_started_at: summary.latest_started_at,
    }
}

fn evidence_segment(segment: &Segment) -> EvidenceSegment {
    match segment {
        Segment::Overall => EvidenceSegment::Overall,
        Segment::Untagged => EvidenceSegment::Untagged,
        Segment::Tag(tag) => EvidenceSegment::Tag(tag.clone()),
    }
}

fn empty_score(segment: Segment) -> SegmentScore {
    SegmentScore {
        segment,
        execution_count: 0,
        excluded_count: 0,
        acceptance: Default::default(),
        duration: Default::default(),
        cost: Default::default(),
        reported: Default::default(),
        origins: Default::default(),
    }
}

fn count(value: usize) -> u64 {
    u64::try_from(value).expect("scorecard counts fit in u64")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use jiff::Timestamp;
    use uuid::Uuid;

    use super::*;
    use crate::{
        AcceptanceAxis, AxisSummary, CostAxis, DurationAxis, Event, EventKind, ExecutionOrigin,
        ProfileScorecard, ProfileSnapshot, RecordedExecutionOutcome, Verdict, project,
        safe_test_declaration,
    };

    fn profile(name: &str, model: &str) -> ExecutionProfile {
        let mut declaration = safe_test_declaration();
        declaration.name = name.to_owned();
        declaration.model = Some(model.to_owned());
        declaration.resolve(PathBuf::from("/usr/bin/true")).unwrap()
    }

    fn interactive_profile() -> ExecutionProfile {
        let declaration = crate::ProfileDeclaration {
            name: "Interactive".to_owned(),
            cli: crate::CliKind::Codex,
            execution_platform: ExecutionPlatform::Interactive,
            executable: None,
            model: Some("interactive".to_owned()),
            args: Vec::new(),
            identity: BTreeMap::new(),
        };
        declaration
            .resolve(PathBuf::from("codex"))
            .expect("interactive profile is valid")
    }

    fn run(profile: &ExecutionProfile, tags: &[&str], at: &str, accepted: bool) -> Vec<Event> {
        let task_id = Uuid::now_v7();
        let mut task = Event::new(
            task_id,
            EventKind::TaskReceived {
                tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                prompt: None,
                prompt_chars: 1,
            },
        );
        task.occurred_at = at.parse::<Timestamp>().unwrap();
        let mut execution = Event::new(
            task_id,
            EventKind::Executed {
                origin: ExecutionOrigin::Brokered,
                profile: ProfileSnapshot::from(profile),
                working_directory: PathBuf::from("/tmp"),
                started_at: at.parse().unwrap(),
                elapsed_ms: 100,
                outcome: RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                cost: None,
                estimated_cost: None,
                usage: None,
                report_failure: None,
            },
        );
        execution.occurred_at = at.parse().unwrap();
        vec![
            task,
            execution,
            Event::new(
                task_id,
                EventKind::Judged {
                    verdict: if accepted {
                        Verdict::Accepted
                    } else {
                        Verdict::Rejected
                    },
                },
            ),
        ]
    }

    fn score_with_metrics(
        profile: &ExecutionProfile,
        accepted: usize,
        duration_ms: u64,
        total_minor_units: u64,
    ) -> ProfileScorecard {
        ProfileScorecard {
            profile_id: profile.id.clone(),
            scores: vec![SegmentScore {
                segment: Segment::Tag("rust".to_owned()),
                execution_count: 3,
                excluded_count: 0,
                acceptance: AcceptanceAxis {
                    summary: AxisSummary {
                        count: 3,
                        latest_started_at: None,
                    },
                    accepted,
                },
                duration: DurationAxis {
                    summary: AxisSummary {
                        count: 3,
                        latest_started_at: None,
                    },
                    median_ms: Some(duration_ms),
                },
                cost: CostAxis {
                    summary: AxisSummary {
                        count: 3,
                        latest_started_at: None,
                    },
                    currency: Some("USD".to_owned()),
                    total_minor_units: Some(total_minor_units),
                    missing_count: 0,
                },
                reported: Default::default(),
                origins: Default::default(),
            }],
        }
    }

    #[test]
    fn untagged_instructions_reference_the_untagged_segment() {
        let profile = profile("one", "one");
        let events = (0..3)
            .flat_map(|index| {
                run(
                    &profile,
                    &[],
                    &format!("2026-01-0{}T00:00:00Z", index + 1),
                    true,
                )
            })
            .collect::<Vec<_>>();
        let projection = project(
            &events,
            std::slice::from_ref(&profile),
            &[Segment::Untagged],
        );
        let advice = advise(&projection, &[], std::slice::from_ref(&profile));
        assert_eq!(advice.referenced_segments, [Segment::Untagged]);
        assert_eq!(advice.proposed_profile_id(), Some(profile.id.as_str()));
    }

    #[test]
    fn proposes_a_unique_leader_with_enough_tagged_judgements() {
        let leader = profile("leader", "leader");
        let other = profile("other", "other");
        let mut events = Vec::new();
        for index in 0..3 {
            events.extend(run(
                &leader,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                true,
            ));
            events.extend(run(
                &other,
                &["rust"],
                &format!("2026-02-0{}T00:00:00Z", index + 1),
                false,
            ));
        }
        let profiles = [leader.clone(), other.clone()];
        let projection = project(&events, &profiles, &[Segment::Tag("rust".to_owned())]);
        let advice = advise(&projection, &["rust".to_owned()], &profiles);
        assert_eq!(advice.proposed_profile_id(), Some(leader.id.as_str()));
        assert_eq!(advice.evidence().unwrap()[0].acceptance.accepted, 3);
    }

    #[test]
    fn proposes_the_same_unique_leader_across_multiple_tags() {
        let leader = profile("leader", "leader");
        let other = profile("other", "other");
        let mut events = Vec::new();
        for (tag, month) in [("rust", "01"), ("docs", "02")] {
            for index in 0..3 {
                events.extend(run(
                    &leader,
                    &[tag],
                    &format!("2026-{month}-{:02}T00:00:00Z", index + 1),
                    true,
                ));
                events.extend(run(
                    &other,
                    &[tag],
                    &format!("2026-{month}-{:02}T01:00:00Z", index + 1),
                    false,
                ));
            }
        }
        let profiles = [leader.clone(), other.clone()];
        let segments = [
            Segment::Tag("rust".to_owned()),
            Segment::Tag("docs".to_owned()),
        ];
        let projection = project(&events, &profiles, &segments);
        let advice = advise(
            &projection,
            &["rust".to_owned(), "docs".to_owned()],
            &profiles,
        );

        let AdviceOutcome::Proposed {
            profile_id,
            evidence,
        } = advice.outcome
        else {
            panic!("the same profile should lead on every referenced segment");
        };
        assert_eq!(profile_id, leader.id);
        assert_eq!(
            evidence
                .iter()
                .map(|evidence| evidence.segment.clone())
                .collect::<Vec<_>>(),
            [
                crate::EvidenceSegment::Tag("rust".to_owned()),
                crate::EvidenceSegment::Tag("docs".to_owned()),
            ]
        );
    }

    #[test]
    fn distinguishes_the_two_and_three_judgement_boundaries() {
        let profile = profile("one", "one");
        for (count, expected) in [(2, "insufficient"), (3, "proposed")] {
            let events = (0..count)
                .flat_map(|index| {
                    run(
                        &profile,
                        &["rust"],
                        &format!("2026-01-0{}T00:00:00Z", index + 1),
                        true,
                    )
                })
                .collect::<Vec<_>>();
            let projection = project(
                &events,
                std::slice::from_ref(&profile),
                &[Segment::Tag("rust".to_owned())],
            );
            let advice = advise(
                &projection,
                &["rust".to_owned()],
                std::slice::from_ref(&profile),
            );
            match (expected, advice.outcome) {
                (
                    "insufficient",
                    AdviceOutcome::Abstained {
                        reason: AdviceReason::InsufficientEvidence { .. },
                    },
                )
                | ("proposed", AdviceOutcome::Proposed { .. }) => {}
                _ => panic!("unexpected advice outcome"),
            }
        }
    }

    #[test]
    fn reports_no_evidence_and_tied_leaders() {
        let first = profile("first", "first");
        let second = profile("second", "second");
        let projection = project(
            &[],
            &[first.clone(), second.clone()],
            &[Segment::Tag("rust".to_owned())],
        );
        let advice = advise(
            &projection,
            &["rust".to_owned()],
            &[first.clone(), second.clone()],
        );
        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoEvidence { .. }
            }
        ));

        let mut events = Vec::new();
        for index in 0..3 {
            events.extend(run(
                &first,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                true,
            ));
            events.extend(run(
                &second,
                &["rust"],
                &format!("2026-02-0{}T00:00:00Z", index + 1),
                true,
            ));
        }
        let profiles = [first.clone(), second.clone()];
        let projection = project(&events, &profiles, &[Segment::Tag("rust".to_owned())]);
        let advice = advise(&projection, &["rust".to_owned()], &profiles);
        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained { reason: AdviceReason::NoUniqueLeader { ref profile_ids } }
                if profile_ids == &vec![first.id, second.id]
        ));
    }

    #[test]
    fn compares_equal_fractions_with_different_denominators() {
        let first = profile("first", "first");
        let second = profile("second", "second");
        let mut events = Vec::new();
        for index in 0..6 {
            events.extend(run(
                &first,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                index < 3,
            ));
        }
        for index in 0..4 {
            events.extend(run(
                &second,
                &["rust"],
                &format!("2026-02-0{}T00:00:00Z", index + 1),
                index < 2,
            ));
        }
        let profiles = [first.clone(), second.clone()];
        let projection = project(&events, &profiles, &[Segment::Tag("rust".to_owned())]);
        let advice = advise(&projection, &["rust".to_owned()], &profiles);
        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoUniqueLeader { .. }
            }
        ));
    }

    #[test]
    fn does_not_break_acceptance_ties_with_duration_or_cost() {
        let first = profile("first", "first");
        let second = profile("second", "second");
        let projection = Projection {
            scorecards: vec![
                score_with_metrics(&first, 2, 100, 10),
                score_with_metrics(&second, 2, 200, 20),
            ],
            ..Projection::default()
        };
        let profiles = [first.clone(), second.clone()];
        let advice = advise(&projection, &["rust".to_owned()], &profiles);

        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoUniqueLeader { ref profile_ids }
            } if profile_ids == &vec![first.id, second.id]
        ));
    }

    #[test]
    fn prefers_acceptance_over_better_duration_or_cost() {
        let higher_acceptance = profile("higher-acceptance", "higher-acceptance");
        let lower_acceptance = profile("lower-acceptance", "lower-acceptance");
        let projection = Projection {
            scorecards: vec![
                score_with_metrics(&higher_acceptance, 3, 2_000, 200),
                score_with_metrics(&lower_acceptance, 2, 100, 10),
            ],
            ..Projection::default()
        };
        let profiles = [higher_acceptance.clone(), lower_acceptance];
        let advice = advise(&projection, &["rust".to_owned()], &profiles);

        assert_eq!(
            advice.proposed_profile_id(),
            Some(higher_acceptance.id.as_str())
        );
    }

    #[test]
    fn requires_the_same_unique_leader_for_every_tag() {
        let first = profile("first", "first");
        let second = profile("second", "second");
        let mut events = Vec::new();
        for index in 0..3 {
            events.extend(run(
                &first,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                true,
            ));
            events.extend(run(
                &second,
                &["rust"],
                &format!("2026-02-0{}T00:00:00Z", index + 1),
                false,
            ));
            events.extend(run(
                &first,
                &["docs"],
                &format!("2026-03-0{}T00:00:00Z", index + 1),
                false,
            ));
            events.extend(run(
                &second,
                &["docs"],
                &format!("2026-04-0{}T00:00:00Z", index + 1),
                true,
            ));
        }
        let profiles = [first.clone(), second.clone()];
        let projection = project(
            &events,
            &profiles,
            &[
                Segment::Tag("rust".to_owned()),
                Segment::Tag("docs".to_owned()),
            ],
        );
        let advice = advise(
            &projection,
            &["rust".to_owned(), "docs".to_owned()],
            &profiles,
        );
        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoUniqueLeader { .. }
            }
        ));
    }

    #[test]
    fn applies_and_condition_when_one_tag_is_below_the_threshold() {
        let first = profile("first", "first");
        let mut events = Vec::new();
        for index in 0..3 {
            events.extend(run(
                &first,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                true,
            ));
        }
        events.extend(run(&first, &["docs"], "2026-02-01T00:00:00Z", true));
        let projection = project(
            &events,
            std::slice::from_ref(&first),
            &[
                Segment::Tag("rust".to_owned()),
                Segment::Tag("docs".to_owned()),
            ],
        );
        let advice = advise(
            &projection,
            &["rust".to_owned(), "docs".to_owned()],
            std::slice::from_ref(&first),
        );
        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained {
                reason: AdviceReason::InsufficientEvidence { .. }
            }
        ));
    }

    #[test]
    fn uses_all_tags_and_keeps_case_sensitive_tag_segments() {
        let profile = profile("one", "one");
        let tags: Vec<_> = (0..6).map(|index| format!("tag-{index}")).collect();
        let projection = project(
            &[],
            std::slice::from_ref(&profile),
            &tags.iter().cloned().map(Segment::Tag).collect::<Vec<_>>(),
        );
        let advice = advise(&projection, &tags, std::slice::from_ref(&profile));
        assert_eq!(advice.referenced_segments.len(), 6);

        let projection = project(
            &[],
            std::slice::from_ref(&profile),
            &[
                Segment::Tag("Rust".to_owned()),
                Segment::Tag("rust".to_owned()),
            ],
        );
        let advice = advise(
            &projection,
            &[" Rust ".to_owned(), "rust".to_owned()],
            std::slice::from_ref(&profile),
        );
        assert_eq!(
            advice.referenced_segments,
            [
                Segment::Tag("Rust".to_owned()),
                Segment::Tag("rust".to_owned())
            ]
        );
    }

    #[test]
    fn excludes_interactive_profiles_even_when_their_evidence_is_best() {
        let headless = profile("headless", "headless");
        let interactive = interactive_profile();
        let mut events = Vec::new();
        for index in 0..3 {
            events.extend(run(
                &interactive,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                true,
            ));
        }
        let profiles = [headless.clone(), interactive.clone()];
        let projection = project(&events, &profiles, &[Segment::Tag("rust".to_owned())]);
        let advice = advise(&projection, &["rust".to_owned()], &profiles);
        assert!(matches!(
            advice.outcome,
            AdviceOutcome::Abstained {
                reason: AdviceReason::NoEvidence { .. }
            }
        ));
    }

    #[test]
    fn proposes_the_only_qualified_profile_regardless_of_rate() {
        let qualified = profile("qualified", "qualified");
        let other = profile("other", "other");
        let mut events = Vec::new();
        for index in 0..3 {
            events.extend(run(
                &qualified,
                &["rust"],
                &format!("2026-01-0{}T00:00:00Z", index + 1),
                false,
            ));
            events.extend(run(
                &other,
                &["rust"],
                &format!("2026-02-0{}T00:00:00Z", index + 1),
                true,
            ));
        }
        let profiles = [qualified.clone(), other.clone()];
        let projection = project(&events, &profiles, &[Segment::Tag("rust".to_owned())]);
        let advice = advise(&projection, &["rust".to_owned()], &profiles);
        assert_eq!(advice.proposed_profile_id(), Some(other.id.as_str()));

        let qualified_events = (0..3)
            .flat_map(|index| {
                run(
                    &qualified,
                    &["rust"],
                    &format!("2026-03-0{}T00:00:00Z", index + 1),
                    false,
                )
            })
            .collect::<Vec<_>>();
        let projection = project(
            &qualified_events,
            &profiles,
            &[Segment::Tag("rust".to_owned())],
        );
        let advice = advise(&projection, &["rust".to_owned()], &profiles);
        assert_eq!(advice.proposed_profile_id(), Some(qualified.id.as_str()));
    }

    #[test]
    fn copies_axis_values_without_recomputing_them() {
        let profile = profile("one", "one");
        let events = (0..3)
            .flat_map(|index| {
                run(
                    &profile,
                    &["rust"],
                    &format!("2026-01-0{}T00:00:00Z", index + 1),
                    true,
                )
            })
            .collect::<Vec<_>>();
        let segment = Segment::Tag("rust".to_owned());
        let projection = project(
            &events,
            std::slice::from_ref(&profile),
            std::slice::from_ref(&segment),
        );
        let score = &projection.scorecards[0].scores[0];
        let advice = advise(
            &projection,
            &["rust".to_owned()],
            std::slice::from_ref(&profile),
        );
        let evidence = &advice.evidence().unwrap()[0];
        assert_eq!(
            evidence.acceptance.summary.count,
            score.acceptance.summary.count as u64
        );
        assert_eq!(
            evidence.acceptance.accepted,
            score.acceptance.accepted as u64
        );
        assert_eq!(
            evidence.duration.summary.count,
            score.duration.summary.count as u64
        );
        assert_eq!(evidence.duration.median_ms, score.duration.median_ms);
        assert_eq!(evidence.cost.summary.count, score.cost.summary.count as u64);
        assert_eq!(evidence.cost.missing_count, score.cost.missing_count as u64);
    }

    #[test]
    fn copies_independent_axis_summaries_and_all_cost_states() {
        let profile = profile("one", "one");
        let acceptance_latest = "2026-08-01T00:00:00Z".parse().unwrap();
        let cost_latest = "2026-08-02T00:00:00Z".parse().unwrap();
        let score = |segment: &str, cost: crate::CostAxis| SegmentScore {
            segment: Segment::Tag(segment.to_owned()),
            execution_count: 3,
            excluded_count: 1,
            acceptance: crate::AcceptanceAxis {
                summary: crate::AxisSummary {
                    count: 3,
                    latest_started_at: Some(acceptance_latest),
                },
                accepted: 2,
            },
            duration: crate::DurationAxis {
                summary: crate::AxisSummary {
                    count: 0,
                    latest_started_at: None,
                },
                median_ms: None,
            },
            cost,
            reported: Default::default(),
            origins: Default::default(),
        };
        let scores = vec![
            score(
                "single",
                crate::CostAxis {
                    summary: crate::AxisSummary {
                        count: 2,
                        latest_started_at: Some(cost_latest),
                    },
                    currency: Some("USD".to_owned()),
                    total_minor_units: Some(12),
                    missing_count: 1,
                },
            ),
            score(
                "mixed",
                crate::CostAxis {
                    summary: crate::AxisSummary {
                        count: 2,
                        latest_started_at: Some(cost_latest),
                    },
                    currency: None,
                    total_minor_units: None,
                    missing_count: 1,
                },
            ),
            score(
                "zero",
                crate::CostAxis {
                    summary: crate::AxisSummary {
                        count: 0,
                        latest_started_at: None,
                    },
                    currency: None,
                    total_minor_units: None,
                    missing_count: 3,
                },
            ),
        ];
        let projection = Projection {
            scorecards: vec![crate::ProfileScorecard {
                profile_id: profile.id.clone(),
                scores,
            }],
            ..Projection::default()
        };
        let advice = advise(
            &projection,
            &["single".to_owned(), "mixed".to_owned(), "zero".to_owned()],
            std::slice::from_ref(&profile),
        );
        let AdviceOutcome::Proposed { evidence, .. } = advice.outcome else {
            panic!("the only qualified profile should be proposed");
        };
        assert_eq!(
            evidence[0].acceptance.summary.latest_started_at,
            Some(acceptance_latest)
        );
        assert_eq!(evidence[0].cost.currency.as_deref(), Some("USD"));
        assert_eq!(evidence[0].cost.total_minor_units, Some(12));
        assert_eq!(
            evidence[0].cost.summary.latest_started_at,
            Some(cost_latest)
        );
        assert_eq!(evidence[1].cost.summary.count, 2);
        assert_eq!(evidence[1].cost.currency, None);
        assert_eq!(evidence[1].cost.total_minor_units, None);
        assert_eq!(evidence[2].cost.summary.count, 0);
        assert_eq!(evidence[2].cost.missing_count, 3);
        assert_eq!(evidence[0].duration.summary.count, 0);
        assert_eq!(evidence[0].duration.median_ms, None);
    }
}

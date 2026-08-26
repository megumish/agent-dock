use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use agent_dock::{
    Advice, AdviceOutcome, BackupEventLogOutcome, Config, Event, EventKind, EventLog,
    ExecutionProfile, ProfileSnapshot, RecordedExecutionOutcome, Verdict, advise,
    default_events_path, project, safe_test_profile, segments_for_tags,
};
use jiff::Timestamp;
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (target, scenario) = match sample_arguments(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("Usage: sample_events <event-log-path> [scenario]");
            return Err(message.into());
        }
    };
    if is_default_events_path(&target) {
        let default_path = default_events_path()?;
        let message = format!(
            "refusing to replace the default event log `{}`; choose an explicit development path",
            default_path.display()
        );
        eprintln!("{message}");
        return Err(message.into());
    }
    eprintln!(
        "WARNING: this replaces the explicitly selected event log with synthetic data for {}.",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("Stop every agent-dock process before continuing.");
    let backup = replace_with_scenario(&target, &scenario)?;
    println!("Installed `{scenario}` at `{}`.", target.display());
    if let Some(backup) = backup {
        println!("Previous log backup: `{}`.", backup.display());
    }
    Ok(())
}

fn sample_arguments(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(PathBuf, String), String> {
    let target = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "an event log path is required".to_owned())?;
    let scenario = arguments.next().unwrap_or_else(|| "showcase".to_owned());
    if arguments.next().is_some() {
        return Err("expected an event log path followed by at most one scenario".to_owned());
    }
    Ok((target, scenario))
}

fn is_default_events_path(target: &Path) -> bool {
    let Some(default_path) = default_events_path().ok() else {
        return false;
    };
    is_default_events_path_at(target, &default_path)
}

fn is_default_events_path_at(target: &Path, default_path: &Path) -> bool {
    let target = comparable_path(target);
    let default = comparable_path(default_path);
    paths_match(&target, &default)
}

fn comparable_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    let mut ancestor = absolute.clone();
    let mut missing = Vec::new();
    let canonical_ancestor = loop {
        if let Ok(canonical) = fs::canonicalize(&ancestor) {
            break canonical;
        }
        let Some(name) = ancestor.file_name() else {
            return lexical_normalize(&absolute);
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return lexical_normalize(&absolute);
        };
        ancestor = parent.to_path_buf();
    };

    let mut comparable = canonical_ancestor;
    for component in missing.iter().rev() {
        comparable.push(component);
    }
    comparable
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut comparable = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                comparable.pop();
            }
            component => comparable.push(component.as_os_str()),
        }
    }
    comparable
}

fn paths_match(left: &Path, right: &Path) -> bool {
    left == right || same_file_system_object(left, right) || paths_match_ignoring_case(left, right)
}

fn same_file_system_object(left: &Path, right: &Path) -> bool {
    let Ok(left) = fs::metadata(left) else {
        return false;
    };
    let Ok(right) = fs::metadata(right) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn paths_match_ignoring_case(left: &Path, right: &Path) -> bool {
    let (Some(left), Some(right)) = (left.to_str(), right.to_str()) else {
        return false;
    };
    left.to_lowercase() == right.to_lowercase()
}

fn replace_with_scenario(
    target_path: &Path,
    scenario: &str,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    replace_with_scenario_before_install(target_path, scenario, |_| Ok(()))
}

fn replace_with_scenario_before_install(
    target_path: &Path,
    scenario: &str,
    before_install: impl FnOnce(&Path) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let parent = target_path.parent().ok_or("event log path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary_path = parent.join(format!(".sample-events.{}.jsonl", Uuid::now_v7()));
    let temporary_log = EventLog::new(&temporary_path);
    let result = (|| {
        write_scenario(&temporary_log, scenario)?;
        let backup = if target_path.exists() {
            let target = EventLog::new(target_path);
            let current = target.read_all()?;
            match target.backup_and_delete_if_unchanged(&current)? {
                BackupEventLogOutcome::BackedUpAndDeleted { backup_path } => Some(backup_path),
                BackupEventLogOutcome::Changed => {
                    return Err(
                        "event log changed while it was being backed up; nothing was installed"
                            .into(),
                    );
                }
            }
        } else {
            None
        };
        before_install(target_path)?;
        fs::hard_link(&temporary_path, target_path).map_err(|error| {
            let recovery = backup.as_ref().map_or(String::new(), |path| {
                format!("; previous log remains at `{}`", path.display())
            });
            format!(
                "could not install sample log without overwriting `{}`: {error}{recovery}",
                target_path.display(),
            )
        })?;
        Ok(backup)
    })();
    let _ = fs::remove_file(&temporary_path);
    let _ = fs::remove_file(temporary_log.lock_path());
    result
}

fn write_scenario(log: &EventLog, scenario: &str) -> Result<(), Box<dyn std::error::Error>> {
    if scenario == "empty" {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log.path())?;
        file.sync_all()?;
        return Ok(());
    }
    if scenario == "old-schema" {
        let mut event = task("2026-07-01T00:00:00Z", &[]);
        event.format_version = "agent-dock/events/v1alpha1".to_owned();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log.path())?;
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        return Ok(());
    }

    if scenario == "showcase" {
        let profiles = sample_profiles();
        for (index, at) in [
            "2026-06-30T00:00:00Z",
            "2026-06-30T00:01:00Z",
            "2026-06-30T00:02:00Z",
            "2026-06-30T00:03:00Z",
        ]
        .into_iter()
        .enumerate()
        {
            append_run(
                log,
                &profiles[1],
                at,
                &["review"],
                400 + index as u64 * 10,
                Outcome::Accepted,
            )?;
        }
        append_run(
            log,
            &profiles[2],
            "2026-06-30T00:04:00Z",
            &["review"],
            500,
            Outcome::Rejected,
        )?;

        let scenarios = [
            "few",
            "many-tags",
            "rejections",
            "rejudged",
            "mixed-outcomes",
        ];
        for (scenario, profile) in scenarios.into_iter().zip(sample_profiles()) {
            write_current_scenario(log, scenario, &profile)?;
        }
        return Ok(());
    }

    write_current_scenario(log, scenario, &safe_test_profile())
}

fn write_current_scenario(
    log: &EventLog,
    scenario: &str,
    profile: &ExecutionProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    match scenario {
        "few" => {
            for (at, elapsed_ms) in [
                ("2026-07-01T00:00:00Z", 1_500),
                ("2026-07-01T00:01:00Z", 1_500),
                ("2026-07-01T00:02:00Z", 1_500),
                ("2026-07-01T00:03:00Z", 1_500),
            ] {
                append_run(log, profile, at, &[], elapsed_ms, Outcome::Accepted)?;
            }
        }
        "many-tags" => append_run(
            log,
            profile,
            "2026-07-01T00:00:00Z",
            &["one", "two", "three", "four", "five", "six", "seven"],
            2_500,
            Outcome::Accepted,
        )?,
        "rejections" => {
            append_run(
                log,
                profile,
                "2026-07-01T00:00:00Z",
                &["review"],
                500,
                Outcome::Rejected,
            )?;
            append_run(
                log,
                profile,
                "2026-07-01T00:01:00Z",
                &["review"],
                700,
                Outcome::Rejected,
            )?;
        }
        "rejudged" => append_run(
            log,
            profile,
            "2026-07-01T00:00:00Z",
            &["review"],
            900,
            Outcome::RejudgedRejected,
        )?,
        "mixed-outcomes" => {
            for (at, outcome) in [
                ("2026-07-01T00:00:00Z", Outcome::Accepted),
                ("2026-07-01T00:01:00Z", Outcome::Rejected),
                ("2026-07-01T00:02:00Z", Outcome::Unjudged),
                ("2026-07-01T00:03:00Z", Outcome::Cancelled),
                ("2026-07-01T00:04:00Z", Outcome::Failed),
            ] {
                append_run(log, profile, at, &[], 500, outcome)?;
            }
        }
        _ => return Err(format!("unknown scenario `{scenario}`").into()),
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Outcome {
    Accepted,
    Rejected,
    RejudgedRejected,
    Unjudged,
    Cancelled,
    Failed,
}

fn append_run(
    log: &EventLog,
    profile: &ExecutionProfile,
    at: &str,
    tags: &[&str],
    elapsed_ms: u64,
    outcome: Outcome,
) -> Result<(), Box<dyn std::error::Error>> {
    let task = task(at, tags);
    let task_id = task.task_id.expect("task event has an id");
    let tags = tags.iter().map(|tag| (*tag).to_owned()).collect::<Vec<_>>();
    let advice = advice_for_task(log, &tags)?;
    log.append(&task)?;
    append_advice(log, task_id, &advice, profile)?;
    let execution = execution(task_id, profile, at, elapsed_ms, outcome);
    log.append(&execution)?;
    match outcome {
        Outcome::Accepted | Outcome::RejudgedRejected => log.append(&Event::new(
            task_id,
            EventKind::Judged {
                verdict: Verdict::Accepted,
            },
        ))?,
        Outcome::Rejected => log.append(&Event::new(
            task_id,
            EventKind::Judged {
                verdict: Verdict::Rejected,
            },
        ))?,
        Outcome::Unjudged | Outcome::Cancelled | Outcome::Failed => {}
    }
    if matches!(outcome, Outcome::RejudgedRejected) {
        log.append(&Event::new(
            task_id,
            EventKind::Judged {
                verdict: Verdict::Rejected,
            },
        ))?;
    }
    Ok(())
}

fn advice_for_task(log: &EventLog, tags: &[String]) -> Result<Advice, Box<dyn std::error::Error>> {
    let history = log.read_all()?;
    let profiles = sample_profiles();
    let segments = segments_for_tags(tags);
    let projection = project(&history.events, &profiles, &segments);
    Ok(advise(&projection, tags, &profiles))
}

fn append_advice(
    log: &EventLog,
    task_id: Uuid,
    advice: &Advice,
    selected_profile: &ExecutionProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    let advised = Event::new(task_id, advice.event_kind());
    let advice_event_id = advised.event_id;
    log.append(&advised)?;
    match &advice.outcome {
        AdviceOutcome::Proposed { profile_id, .. } if profile_id == &selected_profile.id => {
            log.append(&Event::new(
                task_id,
                EventKind::Approved {
                    advice_event_id,
                    profile_id: selected_profile.id.clone(),
                },
            ))?;
        }
        AdviceOutcome::Proposed { .. } => {
            log.append(&Event::new(
                task_id,
                EventKind::Declined { advice_event_id },
            ))?;
            log.append(&Event::new(
                task_id,
                EventKind::Designated {
                    advice_event_id,
                    profile_id: selected_profile.id.clone(),
                },
            ))?;
        }
        AdviceOutcome::Abstained { .. } => {
            log.append(&Event::new(
                task_id,
                EventKind::Designated {
                    advice_event_id,
                    profile_id: selected_profile.id.clone(),
                },
            ))?;
        }
    }
    Ok(())
}

fn task(at: &str, tags: &[&str]) -> Event {
    let mut event = Event::new(
        Uuid::now_v7(),
        EventKind::TaskReceived {
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            prompt: None,
            prompt_chars: 16,
        },
    );
    event.occurred_at = parse(at);
    event
}

fn execution(
    task_id: Uuid,
    profile: &ExecutionProfile,
    at: &str,
    elapsed_ms: u64,
    outcome: Outcome,
) -> Event {
    let (estimated_cost, usage) = match profile.cli() {
        agent_dock::CliKind::Claude => (
            Some(agent_dock::EstimatedCost {
                currency: "USD".to_owned(),
                amount_micros: 1_234_567,
            }),
            Some(agent_dock::TokenUsage {
                input_tokens: Some(1_200),
                output_tokens: Some(340),
                cached_input_tokens: Some(80),
                reasoning_tokens: None,
            }),
        ),
        agent_dock::CliKind::Codex => (
            None,
            Some(agent_dock::TokenUsage {
                input_tokens: Some(900),
                output_tokens: Some(250),
                cached_input_tokens: Some(40),
                reasoning_tokens: Some(120),
            }),
        ),
        _ => (None, None),
    };
    Event::new(
        task_id,
        EventKind::Executed {
            origin: agent_dock::ExecutionOrigin::Brokered,
            profile: ProfileSnapshot::from(profile),
            working_directory: PathBuf::from("/tmp/agent-dock-sample"),
            started_at: parse(at)
                .checked_add(jiff::SignedDuration::from_secs(2))
                .unwrap(),
            elapsed_ms,
            outcome: match outcome {
                Outcome::Cancelled => RecordedExecutionOutcome::Cancelled,
                Outcome::Failed => RecordedExecutionOutcome::Failed {
                    failure_kind: agent_dock::FailureKind::Runtime,
                    message: "synthetic failure".to_owned(),
                },
                _ => RecordedExecutionOutcome::Completed { exit_code: Some(0) },
            },
            cost: None,
            estimated_cost,
            usage,
            report_failure: None,
            observed_cli_version: None,
            observed_models: Vec::new(),
        },
    )
}

fn sample_profiles() -> Vec<ExecutionProfile> {
    let mut profiles = vec![safe_test_profile()];
    profiles.extend(
        Config::defaults(false)
            .profiles
            .into_iter()
            .filter(|profile| profile.execution_platform == agent_dock::ExecutionPlatform::Headless)
            .map(|declaration| {
                let resolved_executable = declaration.executable();
                declaration
                    .resolve(resolved_executable)
                    .expect("default profiles are valid")
            }),
    );
    profiles
}

fn parse(value: &str) -> Timestamp {
    value.parse().expect("fixed sample timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_dock::{Segment, advise, project, segments_for_tags};

    #[test]
    fn requires_an_explicit_event_log_path_and_defaults_only_the_scenario() {
        assert!(sample_arguments(std::iter::empty()).is_err());
        assert_eq!(
            sample_arguments(["/tmp/events.jsonl".to_owned()].into_iter()).unwrap(),
            (PathBuf::from("/tmp/events.jsonl"), "showcase".to_owned())
        );
        assert!(
            sample_arguments(
                ["/tmp/events.jsonl", "empty", "few"]
                    .map(str::to_owned)
                    .into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_the_default_event_log_path() {
        let default_path = default_events_path().unwrap();
        assert!(is_default_events_path(&default_path));
        assert!(!is_default_events_path(Path::new(
            "/tmp/agent-dock-sample.jsonl"
        )));
    }

    #[test]
    fn refuses_a_missing_default_log_through_a_symlinked_parent() {
        let directory = tempfile::tempdir().unwrap();
        let default_parent = directory.path().join("default-parent");
        fs::create_dir(&default_parent).unwrap();
        let default_path = default_parent.join("events.jsonl");
        let symlink_parent = directory.path().join("parent-alias");
        std::os::unix::fs::symlink(&default_parent, &symlink_parent).unwrap();
        let target = symlink_parent.join("events.jsonl");

        assert!(!default_path.exists());
        assert!(is_default_events_path_at(&target, &default_path));
    }

    #[test]
    fn case_insensitive_path_comparison_rejects_a_case_only_alias() {
        let default_path = Path::new("/tmp/Agent-Dock/events.jsonl");
        let target = Path::new("/tmp/agent-dock/EVENTS.JSONL");

        assert!(paths_match_ignoring_case(
            &comparable_path(target),
            &comparable_path(default_path)
        ));
        assert!(is_default_events_path_at(target, default_path));
    }

    #[test]
    fn allows_an_unrelated_explicit_event_log_path() {
        let directory = tempfile::tempdir().unwrap();
        let default_path = directory.path().join("default/events.jsonl");
        let target = directory.path().join("other/events.jsonl");

        assert!(!is_default_events_path_at(&target, &default_path));
    }

    #[test]
    fn installs_empty_log_and_removes_temporary_lock() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("events.jsonl");
        assert_eq!(replace_with_scenario(&target, "empty").unwrap(), None);
        assert_eq!(fs::read(&target).unwrap(), b"");
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".sample-events")
        }));
    }

    #[test]
    fn installs_an_old_schema_log_for_migration_checks() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("events.jsonl");
        replace_with_scenario(&target, "old-schema").unwrap();

        let history = EventLog::new(target).read_all().unwrap();
        assert!(history.events.is_empty());
        assert!(matches!(
            &history.skipped_lines[..],
            [agent_dock::SkippedLine {
                reason: agent_dock::SkippedLineReason::UnsupportedFormat { found, .. },
                ..
            }] if found == "agent-dock/events/v1alpha1"
        ));
    }

    #[test]
    fn backs_up_existing_log_before_installing() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("events.jsonl");
        fs::write(&target, b"previous\n").unwrap();
        let backup = replace_with_scenario(&target, "few").unwrap().unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"previous\n");
        assert!(!fs::read(&target).unwrap().is_empty());
    }

    #[test]
    fn showcase_keeps_representative_scenarios_separate_by_profile() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("events.jsonl");
        replace_with_scenario(&target, "showcase").unwrap();

        let history = EventLog::new(target).read_all().unwrap();
        assert_eq!(history.events.len(), 92);
        assert!(history.skipped_lines.is_empty());
        assert!(history.events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Executed {
                profile,
                estimated_cost: Some(estimated_cost),
                usage: Some(usage),
                ..
            } if profile.cli == agent_dock::CliKind::Claude
                && estimated_cost.currency == "USD"
                && estimated_cost.amount_micros == 1_234_567
                && usage.input_tokens == Some(1_200)
        )));
        assert!(history.events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Executed {
                profile,
                estimated_cost: None,
                usage: Some(usage),
                ..
            } if profile.cli == agent_dock::CliKind::Codex
                && usage.reasoning_tokens == Some(120)
        )));

        let profiles = sample_profiles();
        let projection = project(&history.events, &profiles, &[Segment::Overall]);
        let actual: Vec<_> = projection
            .scorecards
            .iter()
            .map(|card| {
                let score = &card.scores[0];
                (
                    card.profile_id.as_str(),
                    score.execution_count,
                    score.acceptance.accepted,
                    score.acceptance.summary.count,
                    score.duration.median_ms,
                    score.excluded_count,
                )
            })
            .collect();
        let expected = [
            (profiles[0].id.as_str(), 4, 4, 4, Some(1_500), 0),
            (profiles[1].id.as_str(), 5, 5, 5, Some(420), 0),
            (profiles[2].id.as_str(), 3, 0, 3, None, 0),
            (profiles[3].id.as_str(), 1, 0, 1, None, 0),
            (profiles[4].id.as_str(), 5, 1, 2, Some(500), 3),
        ];
        assert_eq!(actual, expected);
        assert!(projection.warnings.is_empty());

        let mut advised_count = 0;
        let mut approved_count = 0;
        let mut declined_count = 0;
        let mut abstained_count = 0;
        for (index, event) in history.events.iter().enumerate() {
            let EventKind::Advised { outcome, .. } = &event.kind else {
                continue;
            };
            advised_count += 1;
            let task_id = event.task_id.expect("advice event has a task identity");
            let task_index = history.events[..index]
                .iter()
                .rposition(|candidate| {
                    candidate.task_id == Some(task_id)
                        && matches!(&candidate.kind, EventKind::TaskReceived { .. })
                })
                .expect("advice event has a preceding task event");
            assert_eq!(index, task_index + 1);
            let tags = match &history.events[task_index].kind {
                EventKind::TaskReceived { tags, .. } => tags.clone(),
                _ => unreachable!("the preceding task event was found by kind"),
            };
            let segments = segments_for_tags(&tags);
            let expected_kind = advise(
                &project(&history.events[..task_index], &profiles, &segments),
                &tags,
                &profiles,
            )
            .event_kind();
            assert_eq!(&event.kind, &expected_kind);
            let advice_event_id = event.event_id;
            match outcome {
                AdviceOutcome::Proposed { profile_id, .. } => match &history.events[index + 1].kind
                {
                    EventKind::Approved {
                        advice_event_id: approved_advice_event_id,
                        profile_id: approved_profile_id,
                    } => {
                        approved_count += 1;
                        assert_eq!(*approved_advice_event_id, advice_event_id);
                        assert_eq!(approved_profile_id, profile_id);
                    }
                    EventKind::Declined {
                        advice_event_id: declined_advice_event_id,
                    } => {
                        declined_count += 1;
                        assert_eq!(*declined_advice_event_id, advice_event_id);
                        let EventKind::Designated {
                            advice_event_id: designated_advice_event_id,
                            profile_id: designated_profile_id,
                        } = &history.events[index + 2].kind
                        else {
                            panic!("a declined proposal must be followed by a designation");
                        };
                        assert_eq!(*designated_advice_event_id, advice_event_id);
                        assert_ne!(designated_profile_id, profile_id);
                    }
                    _ => panic!("a proposal must be approved or declined"),
                },
                AdviceOutcome::Abstained { .. } => {
                    abstained_count += 1;
                    let EventKind::Designated {
                        advice_event_id: designated_advice_event_id,
                        ..
                    } = &history.events[index + 1].kind
                    else {
                        panic!("an abstained advice must be followed by a designation");
                    };
                    assert_eq!(*designated_advice_event_id, advice_event_id);
                }
            }
        }
        assert!(advised_count > 0);
        assert!(approved_count > 0);
        assert!(declined_count > 0);
        assert!(abstained_count > 0);
    }

    #[test]
    fn install_never_overwrites_a_racing_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("events.jsonl");
        let result = replace_with_scenario_before_install(&target, "few", |target| {
            fs::write(target, b"newcomer\n")?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"newcomer\n");
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".sample-events")
        }));
    }
}

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
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
    let scenario = scenario_argument(std::env::args().skip(1))?;
    let target = default_events_path()?;
    eprintln!(
        "WARNING: this replaces the active {} event log with synthetic data.",
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

fn scenario_argument(mut arguments: impl Iterator<Item = String>) -> Result<String, &'static str> {
    let scenario = arguments.next().unwrap_or_else(|| "showcase".to_owned());
    if arguments.next().is_some() {
        return Err("expected at most one scenario");
    }
    Ok(scenario)
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
    fn defaults_to_showcase_when_the_scenario_is_omitted() {
        assert_eq!(scenario_argument(std::iter::empty()).unwrap(), "showcase");
        assert_eq!(
            scenario_argument(["empty".to_owned()].into_iter()).unwrap(),
            "empty"
        );
        assert!(scenario_argument(["empty".to_owned(), "few".to_owned()].into_iter()).is_err());
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
        assert_eq!(history.events.len(), 68);
        assert!(history.skipped_lines.is_empty());

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
            (profiles[1].id.as_str(), 1, 1, 1, Some(2_500), 0),
            (profiles[2].id.as_str(), 2, 0, 2, None, 0),
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

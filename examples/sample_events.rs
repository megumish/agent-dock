use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use agent_dock::{
    BackupEventLogOutcome, Event, EventKind, EventLog, ProfileSnapshot, RecordedExecutionOutcome,
    Verdict, default_events_path, safe_test_profile,
};
use jiff::Timestamp;
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = std::env::args().nth(1).ok_or(
        "usage: cargo run --example sample_events -- <empty|few|many-tags|rejections|rejudged|mixed-outcomes|old-schema>",
    )?;
    if std::env::args().nth(2).is_some() {
        return Err("expected exactly one scenario".into());
    }
    let target = default_events_path()?;
    eprintln!("WARNING: this replaces the active v0.0.2 event log with synthetic data.");
    eprintln!("Stop every agent-dock process before continuing.");
    let backup = replace_with_scenario(&target, &scenario)?;
    println!("Installed `{scenario}` at `{}`.", target.display());
    if let Some(backup) = backup {
        println!("Previous log backup: `{}`.", backup.display());
    }
    Ok(())
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

    match scenario {
        "few" => append_run(log, "2026-07-01T00:00:00Z", &[], 1_500, Outcome::Accepted)?,
        "many-tags" => append_run(
            log,
            "2026-07-01T00:00:00Z",
            &["one", "two", "three", "four", "five", "six", "seven"],
            2_500,
            Outcome::Accepted,
        )?,
        "rejections" => {
            append_run(
                log,
                "2026-07-01T00:00:00Z",
                &["review"],
                500,
                Outcome::Rejected,
            )?;
            append_run(
                log,
                "2026-07-01T00:01:00Z",
                &["review"],
                700,
                Outcome::Rejected,
            )?;
        }
        "rejudged" => append_run(
            log,
            "2026-07-01T00:00:00Z",
            &["review"],
            900,
            Outcome::RejudgedRejected,
        )?,
        "mixed-outcomes" => {
            append_run(log, "2026-07-01T00:00:00Z", &[], 500, Outcome::Accepted)?;
            append_run(log, "2026-07-01T00:01:00Z", &[], 500, Outcome::Rejected)?;
            append_run(log, "2026-07-01T00:02:00Z", &[], 500, Outcome::Unjudged)?;
            append_run(log, "2026-07-01T00:03:00Z", &[], 500, Outcome::Cancelled)?;
            append_run(log, "2026-07-01T00:04:00Z", &[], 500, Outcome::Failed)?;
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
    at: &str,
    tags: &[&str],
    elapsed_ms: u64,
    outcome: Outcome,
) -> Result<(), Box<dyn std::error::Error>> {
    let task = task(at, tags);
    let task_id = task.task_id;
    log.append(&task)?;
    let execution = execution(task_id, at, elapsed_ms, outcome);
    let execution_id = execution.event_id;
    log.append(&execution)?;
    match outcome {
        Outcome::Accepted | Outcome::RejudgedRejected => log.append(&Event::new(
            task_id,
            EventKind::Judged {
                execution_event_id: execution_id,
                verdict: Verdict::Accepted,
            },
        ))?,
        Outcome::Rejected => log.append(&Event::new(
            task_id,
            EventKind::Judged {
                execution_event_id: execution_id,
                verdict: Verdict::Rejected,
            },
        ))?,
        Outcome::Unjudged | Outcome::Cancelled | Outcome::Failed => {}
    }
    if matches!(outcome, Outcome::RejudgedRejected) {
        log.append(&Event::new(
            task_id,
            EventKind::Judged {
                execution_event_id: execution_id,
                verdict: Verdict::Rejected,
            },
        ))?;
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

fn execution(task_id: Uuid, at: &str, elapsed_ms: u64, outcome: Outcome) -> Event {
    Event::new(
        task_id,
        EventKind::Executed {
            profile: ProfileSnapshot::from(&safe_test_profile()),
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

fn parse(value: &str) -> Timestamp {
    value.parse().expect("fixed sample timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backs_up_existing_log_before_installing() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("events.jsonl");
        fs::write(&target, b"previous\n").unwrap();
        let backup = replace_with_scenario(&target, "few").unwrap().unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"previous\n");
        assert!(!fs::read(&target).unwrap().is_empty());
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

use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

use agent_dock::{
    BackupEventLogOutcome, Config, ConfigError, EVENT_FORMAT_VERSION, Event, EventKind, EventLog,
    ExecutionError, ExecutionEvent, ExecutionOutcome, ExecutionProfile, ExecutionRequest,
    FailureKind, OutputSource, ProfileDeclaration, ProfileSnapshot, ReadEvents,
    RecordedExecutionOutcome, ReplaceConfigOutcome, Segment, SegmentScore, SkippedLineReason,
    Verdict, default_config_path, default_events_path, escape_terminal, execute, format_acceptance,
    format_duration, format_recency, project, safe_test_profile, segments_for_tags,
};
use jiff::Timestamp;
use rustyline::{DefaultEditor, error::ReadlineError};
use thiserror::Error;
use tokio::sync::oneshot;
use uuid::Uuid;

const ACCEPT_RESULT_DEFAULT: bool = true;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(normalize_exit_code(code)),
        Err(error) if error.is_interrupted() => {
            eprintln!("agent-dock: cancelled");
            ExitCode::from(130)
        }
        Err(error) => {
            eprintln!("agent-dock: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<i32, AppError> {
    let config_path = default_config_path()?;
    let Some(config) = load_or_create_config(&config_path)? else {
        return Ok(0);
    };

    let mut available = vec![safe_test_profile()];
    for declaration in &config.profiles {
        if let Some(executable) = resolve_executable(declaration) {
            available.push(
                declaration
                    .resolve(executable)
                    .expect("validated configuration"),
            );
        } else {
            eprintln!(
                "Unavailable profile '{}': executable '{}' was not found",
                escape_terminal(&declaration.name),
                escape_terminal(&declaration.executable().display().to_string())
            );
        }
    }
    if available.is_empty() {
        return Err(AppError::NoAvailableProfiles);
    }

    let working_directory = std::env::current_dir()?;
    let event_log = EventLog::new(default_events_path()?);
    let Some(history) = read_event_history(&event_log)? else {
        return Ok(0);
    };
    for skipped in &history.skipped_lines {
        eprintln!(
            "Warning: skipped event log line {}: {}",
            skipped.line_number, skipped.reason
        );
    }
    let tag_candidates = collect_tag_candidates(&config.tag_candidates, &history.events);
    let prompt = read_non_empty_prompt()?;
    let received_at = Timestamp::now();
    let tags = read_tags(&tag_candidates)?;
    let prompt_approved = if config.record_prompt {
        confirm("Record this task prompt?", true)?
    } else {
        false
    };
    let task_id = Uuid::now_v7();
    let all_segments = segments_for_tags(&tags);
    let score_segments = visible_segments(&all_segments);
    let hidden_tags = all_segments.len() - score_segments.len();
    event_log.append(&task_received_event(
        task_id,
        tags,
        &prompt,
        config.record_prompt,
        prompt_approved,
        received_at,
    ))?;

    let Some(fresh_history) = read_event_history(&event_log)? else {
        return Ok(0);
    };
    let projection = project(&fresh_history.events, &available, &score_segments);
    for warning in &projection.warnings {
        eprintln!("Warning: {warning}");
    }
    let choices = render_profile_choices(
        &available,
        &projection.scorecards,
        hidden_tags,
        Timestamp::now(),
    );
    let selected = choose_profile(&choices)?;
    let profile = &available[selected];
    print_execution_summary(profile, &prompt, &working_directory);
    if let Some(exit_code) = record_assignment(&event_log, task_id, &profile.id, confirm_run()?)? {
        println!("Task was not run.");
        return Ok(exit_code);
    }

    let (cancellation_sender, cancellation_receiver) = oneshot::channel();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancellation_sender.send(());
        }
    });
    let started_at = Timestamp::now();
    let attempt_started = Instant::now();
    let execution = execute(
        profile,
        ExecutionRequest {
            prompt: prompt.clone(),
            working_directory: working_directory.clone(),
        },
        cancellation_receiver,
        print_event,
    )
    .await;
    signal_task.abort();

    let outcome = match execution {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = error.to_string();
            eprintln!("Execution failed: {message}");
            event_log.append(&Event::new(
                task_id,
                EventKind::Executed {
                    profile: ProfileSnapshot::from(profile),
                    working_directory,
                    started_at,
                    elapsed_ms: elapsed_millis(attempt_started.elapsed()),
                    outcome: RecordedExecutionOutcome::Failed {
                        failure_kind: failure_kind(&error),
                        message,
                    },
                    cost: None,
                },
            ))?;
            return Ok(1);
        }
    };

    match &outcome {
        ExecutionOutcome::Completed { exit_code, elapsed } => {
            println!(
                "Finished in {:.2?} with exit code {:?}.",
                elapsed, exit_code
            );
        }
        ExecutionOutcome::Cancelled { elapsed } => {
            eprintln!("Cancelled after {:.2?}.", elapsed);
        }
    }
    let process_exit_code = outcome.process_exit_code();
    let recorded_outcome = match &outcome {
        ExecutionOutcome::Completed { exit_code, .. } => RecordedExecutionOutcome::Completed {
            exit_code: *exit_code,
        },
        ExecutionOutcome::Cancelled { .. } => RecordedExecutionOutcome::Cancelled,
    };
    let elapsed = match &outcome {
        ExecutionOutcome::Completed { elapsed, .. } | ExecutionOutcome::Cancelled { elapsed } => {
            *elapsed
        }
    };
    let executed = Event::new(
        task_id,
        EventKind::Executed {
            profile: ProfileSnapshot::from(profile),
            working_directory,
            started_at,
            elapsed_ms: elapsed_millis(elapsed),
            outcome: recorded_outcome,
            cost: None,
        },
    );
    let execution_event_id = executed.event_id;
    event_log.append(&executed)?;

    if matches!(outcome, ExecutionOutcome::Cancelled { .. }) {
        return Ok(process_exit_code);
    }

    let accepted = match confirm("Accept this result?", ACCEPT_RESULT_DEFAULT) {
        Ok(accepted) => accepted,
        Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
            return Ok(process_exit_code);
        }
        Err(source) => return Err(source.into()),
    };
    event_log.append(&Event::new(
        task_id,
        EventKind::Judged {
            execution_event_id,
            verdict: if accepted {
                Verdict::Accepted
            } else {
                Verdict::Rejected
            },
        },
    ))?;
    Ok(process_exit_code)
}

fn load_or_create_config(path: &Path) -> Result<Option<Config>, AppError> {
    load_or_create_config_with(path, &mut confirm)
}

fn load_or_create_config_with(
    path: &Path,
    ask: &mut impl FnMut(&str, bool) -> io::Result<bool>,
) -> Result<Option<Config>, AppError> {
    if path.exists() {
        match Config::load(path) {
            Ok(config) => return Ok(Some(config)),
            Err(ConfigError::UnsupportedApi {
                found,
                expected,
                revision: Some(revision),
            }) => {
                eprintln!(
                    "Configuration `{}` uses API version {found}; expected {expected}.",
                    path.display()
                );
                if !ask("Recreate the incompatible configuration?", false)? {
                    eprintln!("Configuration was not changed.");
                    return Ok(None);
                }
                match Config::backup_and_replace_default_if_unchanged(path, &revision, false)? {
                    ReplaceConfigOutcome::Replaced {
                        config,
                        backup_path,
                    } => {
                        eprintln!(
                            "Warning: backed up incompatible configuration to `{}` and replaced `{}`.",
                            backup_path.display(),
                            path.display()
                        );
                        return Ok(Some(config));
                    }
                    ReplaceConfigOutcome::Changed => {
                        eprintln!(
                            "Configuration changed while waiting for confirmation; reloading it."
                        );
                        return match Config::load(path) {
                            Ok(config) => Ok(Some(config)),
                            Err(ConfigError::UnsupportedApi { .. }) => {
                                eprintln!(
                                    "The changed configuration is still incompatible; it was not changed."
                                );
                                Ok(None)
                            }
                            Err(ConfigError::Read { source, .. })
                                if source.kind() == io::ErrorKind::NotFound =>
                            {
                                eprintln!("The configuration was removed; it was not recreated.");
                                Ok(None)
                            }
                            Err(error) => Err(error.into()),
                        };
                    }
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    println!("Configuration does not exist: {}", path.display());
    if !ask(
        "Create default safe test, Claude, Codex, Gemini, and Antigravity profiles?",
        true,
    )? {
        println!("No configuration was created.");
        return Ok(None);
    }
    let record_prompt = ask("Allow full task prompts to be recorded?", false)?;
    let config = Config::create_default(path, record_prompt)?;
    println!("Created {}", path.display());
    Ok(Some(config))
}

fn read_event_history(event_log: &EventLog) -> Result<Option<ReadEvents>, AppError> {
    read_event_history_with(event_log, &mut confirm)
}

fn read_event_history_with(
    event_log: &EventLog,
    ask: &mut impl FnMut(&str, bool) -> io::Result<bool>,
) -> Result<Option<ReadEvents>, AppError> {
    let history = event_log.read_all()?;
    let mut incompatible_count = 0;
    let mut incompatible_formats = Vec::new();
    for skipped in &history.skipped_lines {
        if let SkippedLineReason::UnsupportedFormat { found, .. } = &skipped.reason {
            incompatible_count += 1;
            if !incompatible_formats.contains(found) {
                incompatible_formats.push(found.clone());
            }
        }
    }
    if incompatible_count == 0 {
        return Ok(Some(history));
    }

    eprintln!(
        "Event log `{}` contains {incompatible_count} event(s) using format(s) {}; expected {EVENT_FORMAT_VERSION}.",
        event_log.path().display(),
        incompatible_formats.join(", ")
    );
    if !ask("Back up and clear the incompatible event log?", false)? {
        eprintln!("Event log was not changed.");
        return Ok(None);
    }
    match event_log.backup_and_delete_if_unchanged(&history)? {
        BackupEventLogOutcome::BackedUpAndDeleted { backup_path } => {
            eprintln!(
                "Warning: backed up incompatible event log to `{}` and cleared `{}`.",
                backup_path.display(),
                event_log.path().display()
            );
            Ok(Some(ReadEvents::default()))
        }
        BackupEventLogOutcome::Changed => {
            eprintln!("Event log changed while waiting for confirmation; it was not changed.");
            Ok(None)
        }
    }
}

fn read_non_empty_prompt() -> Result<String, ReadlineError> {
    let mut editor = DefaultEditor::new()?;
    loop {
        let prompt = editor.readline("Task: ")?;
        if prompt_is_valid(&prompt) {
            return Ok(prompt);
        }
        eprintln!("Task must not be empty.");
    }
}

fn read_line(prompt: &str) -> io::Result<String> {
    print!("{prompt} ");
    io::stdout().flush()?;

    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "standard input was closed",
        ));
    }
    Ok(input.trim_end_matches(['\r', '\n']).to_owned())
}

fn confirm(prompt: &str, default: bool) -> io::Result<bool> {
    let choices = if default { "Y/n" } else { "y/N" };
    loop {
        let input = read_line(&format!("{prompt} [{choices}]"))?;
        if let Some(confirmed) = parse_confirmation(&input, default) {
            return Ok(confirmed);
        }
        eprintln!("Please answer y or n.");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunConfirmation {
    Run,
    Exit,
}

fn confirm_run() -> io::Result<RunConfirmation> {
    confirm_run_with(&mut confirm)
}

fn confirm_run_with(
    ask: &mut impl FnMut(&str, bool) -> io::Result<bool>,
) -> io::Result<RunConfirmation> {
    Ok(if ask("Run this task?", true)? {
        RunConfirmation::Run
    } else {
        RunConfirmation::Exit
    })
}

fn parse_confirmation(input: &str, default: bool) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn choose_profile(labels: &[String]) -> io::Result<usize> {
    println!("Choose a crew member:");
    for (index, label) in labels.iter().enumerate() {
        println!("  {}. {label}", index + 1);
    }

    loop {
        let input = read_line("Selection [1]:")?;
        if let Some(index) = parse_selection(&input, labels.len()) {
            return Ok(index);
        }
        eprintln!("Enter a number from 1 to {}.", labels.len());
    }
}

fn parse_selection(input: &str, count: usize) -> Option<usize> {
    let input = input.trim();
    if input.is_empty() && count > 0 {
        return Some(0);
    }
    let selected = input.parse::<usize>().ok()?;
    selected.checked_sub(1).filter(|index| *index < count)
}

fn visible_segments(segments: &[Segment]) -> Vec<Segment> {
    let mut tag_count = 0;
    segments
        .iter()
        .filter(|segment| match segment {
            Segment::Tag(_) if tag_count == 5 => false,
            Segment::Tag(_) => {
                tag_count += 1;
                true
            }
            _ => true,
        })
        .cloned()
        .collect()
}

fn render_profile_choices(
    profiles: &[ExecutionProfile],
    scorecards: &[agent_dock::ProfileScorecard],
    hidden_tags: usize,
    now: Timestamp,
) -> Vec<String> {
    profiles
        .iter()
        .zip(scorecards)
        .map(|(profile, card)| {
            let model = profile
                .model()
                .map(escape_terminal)
                .unwrap_or_else(|| "default".to_owned());
            let mut lines = vec![format!(
                "{} [{}, {}]",
                escape_terminal(profile.name()),
                profile.cli(),
                model
            )];
            for score in &card.scores {
                lines.push(format!("     {}", render_segment(score, now)));
            }
            if hidden_tags > 0 {
                lines.push(format!("     ... {hidden_tags} more tags"));
            }
            lines.join("\n")
        })
        .collect()
}

fn render_segment(score: &SegmentScore, now: Timestamp) -> String {
    let label = match &score.segment {
        Segment::Overall => "Overall".to_owned(),
        Segment::Untagged => "Untagged".to_owned(),
        Segment::Tag(tag) => format!("Tag: {}", escape_terminal(tag)),
    };
    if score.execution_count == 0 {
        return format!("{label}: No history");
    }
    let acceptance_recency = format_recency(now, score.acceptance.summary.latest_started_at)
        .map(|value| format!(", {value}"))
        .unwrap_or_default();
    let duration_recency = format_recency(now, score.duration.summary.latest_started_at)
        .map(|value| format!(", {value}"))
        .unwrap_or_default();
    let excluded = if score.excluded_count > 0 {
        format!(" | Excluded {}", score.excluded_count)
    } else {
        String::new()
    };
    format!(
        "{label}: Acceptance {}{} | Duration {}{} | Cost missing (0){}",
        format_acceptance(&score.acceptance),
        acceptance_recency,
        format_duration(&score.duration),
        duration_recency,
        excluded,
    )
}

fn prompt_is_valid(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn resolve_executable(profile: &ProfileDeclaration) -> Option<PathBuf> {
    let executable = profile.executable();
    if executable.components().count() > 1 {
        executable_path(&executable)
    } else {
        std::env::var_os("PATH")
            .as_deref()
            .and_then(|path| executable_in_path(&executable, path))
    }
}

fn executable_in_path(executable: &Path, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(executable))
        .find_map(|candidate| executable_path(&candidate))
}

fn executable_path(path: &Path) -> Option<PathBuf> {
    is_executable_file(path)
        .then(|| fs::canonicalize(path).ok())
        .flatten()
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn read_tags(candidates: &[String]) -> io::Result<Vec<String>> {
    if !candidates.is_empty() {
        println!("Tag candidates:");
        for (index, candidate) in candidates.iter().enumerate() {
            println!("  {}. {}", index + 1, escape_terminal(candidate));
        }
    }
    let input = read_line("Tags (comma-separated, blank for unclassified):")?;
    Ok(parse_tags(&input, candidates))
}

fn parse_tags(input: &str, candidates: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    input
        .split(',')
        .filter_map(|item| {
            let item = item.trim();
            if item.is_empty() {
                return None;
            }
            let tag = if item.bytes().all(|byte| byte.is_ascii_digit()) {
                item.parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .and_then(|index| candidates.get(index))
                    .cloned()
            } else {
                Some(item.to_owned())
            }?;
            seen.insert(tag.clone()).then_some(tag)
        })
        .collect()
}

fn collect_tag_candidates(configured: &[String], events: &[Event]) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for tag in configured
        .iter()
        .chain(events.iter().rev().flat_map(|event| {
            if let EventKind::TaskReceived { tags, .. } = &event.kind {
                tags.iter()
            } else {
                [].iter()
            }
        }))
    {
        let tag = tag.trim();
        if !tag.is_empty() && seen.insert(tag.to_owned()) {
            candidates.push(tag.to_owned());
        }
    }
    candidates
}

fn elapsed_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn failure_kind(error: &ExecutionError) -> FailureKind {
    match error {
        ExecutionError::Spawn { .. } => FailureKind::Spawn,
        ExecutionError::MissingProcessId | ExecutionError::MissingPipe(_) => {
            FailureKind::ProcessSetup
        }
        ExecutionError::Io(_) => FailureKind::Io,
        ExecutionError::Signal { .. } => FailureKind::Signal,
        ExecutionError::Task(_) => FailureKind::Runtime,
    }
}

fn task_received_event(
    task_id: Uuid,
    tags: Vec<String>,
    prompt: &str,
    prompt_recording_enabled: bool,
    prompt_approved: bool,
    occurred_at: Timestamp,
) -> Event {
    let mut event = Event::new(
        task_id,
        EventKind::TaskReceived {
            tags,
            prompt: (prompt_recording_enabled && prompt_approved).then(|| prompt.to_owned()),
            prompt_chars: prompt.chars().count(),
        },
    );
    event.occurred_at = occurred_at;
    event
}

fn record_assignment(
    event_log: &EventLog,
    task_id: Uuid,
    profile_id: &str,
    confirmation: RunConfirmation,
) -> Result<Option<i32>, AppError> {
    if confirmation == RunConfirmation::Exit {
        return Ok(Some(0));
    }
    event_log.append(&Event::new(
        task_id,
        EventKind::Assigned {
            profile_id: profile_id.to_owned(),
        },
    ))?;
    Ok(None)
}

fn print_execution_summary(profile: &ExecutionProfile, prompt: &str, working_directory: &Path) {
    println!("\nTask: {}", escape_terminal(prompt));
    println!(
        "Profile: {} ({})",
        escape_terminal(profile.name()),
        profile.id
    );
    println!("CLI: {}", profile.cli());
    println!(
        "Model: {}",
        profile
            .model()
            .map(escape_terminal)
            .as_deref()
            .unwrap_or("CLI default")
    );
    println!(
        "Working directory: {}",
        escape_terminal(&working_directory.display().to_string())
    );
    println!("Permissions: inherited from the agent CLI");
    if !profile.args().is_empty() {
        println!(
            "Additional arguments: {:?}",
            profile
                .args()
                .iter()
                .map(|arg| escape_terminal(arg))
                .collect::<Vec<_>>()
        );
    }
}

fn print_event(event: ExecutionEvent) {
    match event {
        ExecutionEvent::Started { process_id } => println!("Started process {process_id}."),
        ExecutionEvent::Output {
            source: OutputSource::Stdout,
            line,
        } => println!("[stdout] {line}"),
        ExecutionEvent::Output {
            source: OutputSource::Stderr,
            line,
        } => eprintln!("[stderr] {line}"),
        ExecutionEvent::Finished => {}
    }
}

fn normalize_exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

#[derive(Debug, Error)]
enum AppError {
    #[error(transparent)]
    Config(#[from] agent_dock::ConfigError),
    #[error(transparent)]
    Execution(#[from] agent_dock::ExecutionError),
    #[error(transparent)]
    Record(#[from] agent_dock::RecordError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Readline(#[from] ReadlineError),
    #[error("no configured agent CLI is available")]
    NoAvailableProfiles,
}

impl AppError {
    fn is_interrupted(&self) -> bool {
        match self {
            Self::Io(source) => source.kind() == std::io::ErrorKind::Interrupted,
            Self::Readline(ReadlineError::Interrupted) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_event() -> Event {
        let mut event = Event::new(
            Uuid::now_v7(),
            EventKind::TaskReceived {
                tags: vec!["old".to_owned()],
                prompt: None,
                prompt_chars: 1,
            },
        );
        event.format_version = "old".to_owned();
        event
    }

    #[test]
    fn rejects_blank_prompts() {
        assert!(!prompt_is_valid(""));
        assert!(!prompt_is_valid("  \t"));
        assert!(prompt_is_valid("do the work"));
    }

    #[test]
    fn records_prompts_only_when_both_policies_allow_it() {
        let cases = [
            (false, false, None),
            (false, true, None),
            (true, false, None),
            (true, true, Some("秘密")),
        ];

        for (enabled, approved, expected) in cases {
            let event = task_received_event(
                Uuid::now_v7(),
                vec!["private".to_owned()],
                "秘密",
                enabled,
                approved,
                Timestamp::now(),
            );
            let EventKind::TaskReceived {
                prompt,
                prompt_chars,
                ..
            } = event.kind
            else {
                panic!("unexpected event kind");
            };
            assert_eq!(
                prompt.as_deref(),
                expected,
                "enabled={enabled}, approved={approved}"
            );
            assert_eq!(prompt_chars, 2, "enabled={enabled}, approved={approved}");
        }
    }

    #[test]
    fn records_only_task_received_when_run_is_declined() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let task_id = Uuid::now_v7();
        log.append(&task_received_event(
            task_id,
            Vec::new(),
            "do not run",
            false,
            false,
            Timestamp::now(),
        ))
        .unwrap();

        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Run this task?");
            assert!(default);
            Ok(false)
        };
        let confirmation = confirm_run_with(&mut ask).unwrap();
        let exit_code = record_assignment(&log, task_id, "test-default", confirmation).unwrap();
        let events = log.read_all().unwrap().events;

        assert_eq!(exit_code, Some(0));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventKind::TaskReceived { .. }));
    }

    #[test]
    fn parses_confirmation_answers_and_defaults() {
        assert_eq!(parse_confirmation("", ACCEPT_RESULT_DEFAULT), Some(true));
        assert_eq!(parse_confirmation("  ", false), Some(false));
        assert_eq!(parse_confirmation("Y", false), Some(true));
        assert_eq!(parse_confirmation("yes", false), Some(true));
        assert_eq!(parse_confirmation("N", true), Some(false));
        assert_eq!(parse_confirmation("no", true), Some(false));
        assert_eq!(parse_confirmation("maybe", true), None);
    }

    #[test]
    fn safe_test_is_a_fixed_system_profile() {
        let profile = safe_test_profile();
        assert_eq!(profile.cli(), agent_dock::CliKind::Test);
        assert_eq!(profile.id, "42d38cde-50a8-5024-9f12-d164e82adea6");
        assert_eq!(profile.resolved_executable, PathBuf::from("/usr/bin/true"));
    }

    #[test]
    fn recreates_an_incompatible_configuration_only_after_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = \"old\"\n").unwrap();
        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Recreate the incompatible configuration?");
            assert!(!default);
            Ok(true)
        };

        let config = load_or_create_config_with(&path, &mut ask)
            .unwrap()
            .unwrap();

        assert!(!config.record_prompt);
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn preserves_an_incompatible_configuration_if_confirmation_is_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "schema_version = \"old\"\n";
        fs::write(&path, original).unwrap();
        let mut ask = |_: &str, _: bool| {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "test interruption",
            ))
        };

        assert!(matches!(
            load_or_create_config_with(&path, &mut ask),
            Err(AppError::Io(source)) if source.kind() == io::ErrorKind::Interrupted
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn preserves_an_incompatible_configuration_if_recreation_is_declined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "schema_version = \"old\"\n";
        fs::write(&path, original).unwrap();
        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Recreate the incompatible configuration?");
            assert!(!default);
            Ok(false)
        };

        assert!(
            load_or_create_config_with(&path, &mut ask)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn reloads_a_configuration_changed_while_waiting_for_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = \"old\"\n").unwrap();
        let newer = toml::to_string_pretty(&Config::defaults(false)).unwrap();
        let newer_for_prompt = newer.clone();
        let path_for_prompt = path.clone();
        let mut ask = move |_: &str, _: bool| {
            fs::write(&path_for_prompt, &newer_for_prompt).unwrap();
            Ok(true)
        };

        let config = load_or_create_config_with(&path, &mut ask)
            .unwrap()
            .unwrap();

        assert!(!config.record_prompt);
        assert_eq!(fs::read_to_string(path).unwrap(), newer);
    }

    #[test]
    fn backs_up_a_mixed_incompatible_event_log_and_starts_with_empty_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let current = Event::new(
            Uuid::now_v7(),
            EventKind::TaskReceived {
                tags: vec!["current".to_owned()],
                prompt: None,
                prompt_chars: 1,
            },
        );
        let original = format!(
            "{}\n{}\n",
            serde_json::to_string(&current).unwrap(),
            serde_json::to_string(&old_event()).unwrap()
        );
        fs::write(&path, &original).unwrap();
        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Back up and clear the incompatible event log?");
            assert!(!default);
            Ok(true)
        };
        let history = read_event_history_with(&EventLog::new(&path), &mut ask)
            .unwrap()
            .unwrap();

        assert_eq!(history, ReadEvents::default());
        assert!(!path.exists());
        let backups: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("events.jsonl.backup.")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), original);
    }

    #[test]
    fn preserves_an_incompatible_event_log_if_recreation_is_declined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let original = serde_json::to_string(&old_event()).unwrap() + "\n";
        fs::write(&path, &original).unwrap();
        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Back up and clear the incompatible event log?");
            assert!(!default);
            Ok(false)
        };

        assert!(
            read_event_history_with(&EventLog::new(&path), &mut ask)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn preserves_an_incompatible_event_log_if_confirmation_is_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let original = serde_json::to_string(&old_event()).unwrap() + "\n";
        fs::write(&path, &original).unwrap();
        let mut ask = |_: &str, _: bool| {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "test interruption",
            ))
        };

        assert!(matches!(
            read_event_history_with(&EventLog::new(&path), &mut ask),
            Err(AppError::Io(source)) if source.kind() == io::ErrorKind::Interrupted
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn keeps_a_log_containing_only_an_invalid_current_format_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        fs::write(
            &path,
            format!("{{\"format_version\":{EVENT_FORMAT_VERSION:?}}}\n"),
        )
        .unwrap();
        let mut ask = |_: &str, _: bool| panic!("invalid events do not require confirmation");
        let history = read_event_history_with(&EventLog::new(&path), &mut ask)
            .unwrap()
            .unwrap();

        assert!(matches!(
            &history.skipped_lines[0].reason,
            SkippedLineReason::InvalidEvent { .. }
        ));
        assert!(path.exists());
    }

    #[test]
    fn parses_one_based_profile_selections() {
        assert_eq!(parse_selection("", 3), Some(0));
        assert_eq!(parse_selection("1", 3), Some(0));
        assert_eq!(parse_selection(" 3 ", 3), Some(2));
        assert_eq!(parse_selection("0", 3), None);
        assert_eq!(parse_selection("4", 3), None);
        assert_eq!(parse_selection("one", 3), None);
        assert_eq!(parse_selection("", 0), None);
    }

    #[test]
    fn limits_scorecard_tags_and_reports_the_hidden_count() {
        let tags: Vec<_> = (0..7).map(|index| format!("tag-{index}")).collect();
        let all = segments_for_tags(&tags);
        let visible = visible_segments(&all);
        assert_eq!(visible.len(), 6);
        assert_eq!(all.len() - visible.len(), 2);
        assert!(matches!(visible[0], Segment::Overall));
        assert!(matches!(&visible[5], Segment::Tag(tag) if tag == "tag-4"));
    }

    #[test]
    fn resolves_only_executable_files() {
        let profile = ProfileDeclaration {
            name: "Missing".to_owned(),
            cli: agent_dock::CliKind::Claude,
            executable: Some(std::path::PathBuf::from("/definitely/not/installed")),
            model: None,
            args: Vec::new(),
        };

        assert!(resolve_executable(&profile).is_none());
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-agent");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();

        assert_eq!(
            executable_in_path(Path::new("fake-agent"), &path),
            Some(fs::canonicalize(executable).unwrap())
        );
        assert!(executable_in_path(Path::new("missing"), &path).is_none());
        let non_executable = directory.path().join("not-executable");
        fs::write(&non_executable, "not executable\n").unwrap();
        let mut permissions = fs::metadata(&non_executable).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&non_executable, permissions).unwrap();

        assert!(executable_in_path(Path::new("not-executable"), &path).is_none());
    }

    #[test]
    fn parses_numbers_free_tags_and_duplicates() {
        let candidates = vec!["rust".to_owned(), "docs".to_owned()];
        assert_eq!(
            parse_tags(
                " 1, custom,2,rust,custom,0,9,999999999999999999999, ",
                &candidates
            ),
            ["rust", "custom", "docs"]
        );
    }

    #[test]
    fn merges_configured_and_recent_history_tags() {
        let old = Event::new(
            Uuid::now_v7(),
            EventKind::TaskReceived {
                tags: vec!["old".to_owned(), "shared".to_owned()],
                prompt: None,
                prompt_chars: 1,
            },
        );
        let new = Event::new(
            Uuid::now_v7(),
            EventKind::TaskReceived {
                tags: vec!["new".to_owned(), "shared".to_owned()],
                prompt: None,
                prompt_chars: 1,
            },
        );

        assert_eq!(
            collect_tag_candidates(&["configured".to_owned(), "shared".to_owned()], &[old, new]),
            ["configured", "shared", "new", "old"]
        );
    }
    #[test]
    fn preserves_a_new_incompatible_configuration_without_asking_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = 1\n").unwrap();
        let newer = "schema_version = 2\n";
        let path_for_prompt = path.clone();
        let mut prompt_count = 0;
        let mut ask = |prompt: &str, default: bool| {
            prompt_count += 1;
            assert_eq!(prompt_count, 1);
            assert_eq!(prompt, "Recreate the incompatible configuration?");
            assert!(!default);
            fs::write(&path_for_prompt, newer).unwrap();
            Ok(true)
        };

        assert!(
            load_or_create_config_with(&path, &mut ask)
                .unwrap()
                .is_none()
        );
        assert_eq!(prompt_count, 1);
        assert_eq!(fs::read_to_string(path).unwrap(), newer);
    }
}

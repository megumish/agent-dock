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
    Config, Event, EventKind, EventLog, ExecutionError, ExecutionEvent, ExecutionOutcome,
    ExecutionProfile, ExecutionRequest, FailureKind, OutputSource, ProfileSnapshot,
    RecordedExecutionOutcome, Verdict, default_config_path, default_events_path, execute,
};
use jiff::Timestamp;
use rustyline::{DefaultEditor, error::ReadlineError};
use thiserror::Error;
use tokio::sync::oneshot;
use uuid::Uuid;

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
    let config = if config_path.exists() {
        Config::load(&config_path)?
    } else {
        println!("Configuration does not exist: {}", config_path.display());
        let create = confirm(
            "Create default Claude, Codex, Gemini, and Antigravity profiles?",
            true,
        )?;
        if !create {
            println!("No configuration was created.");
            return Ok(0);
        }
        let record_prompt = confirm("Allow full task prompts to be recorded?", false)?;
        let config = Config::create_default(&config_path, record_prompt)?;
        println!("Created {}", config_path.display());
        config
    };

    let mut available = Vec::new();
    for profile in &config.profiles {
        if let Some(executable) = resolve_executable(profile) {
            let mut resolved = profile.clone();
            resolved.executable = Some(executable);
            available.push(resolved);
        } else {
            eprintln!(
                "Unavailable profile '{}': executable '{}' was not found",
                profile.name,
                profile.executable().display()
            );
        }
    }
    if available.is_empty() {
        return Err(AppError::NoAvailableProfiles);
    }

    let working_directory = std::env::current_dir()?;
    let event_log = EventLog::new(default_events_path()?);
    let history = event_log.read_all()?;
    for skipped in &history.skipped_lines {
        eprintln!(
            "Warning: skipped event log line {}: {}",
            skipped.line_number, skipped.reason
        );
    }
    let tag_candidates = collect_tag_candidates(&config.tag_candidates, &history.events);
    let prompt = read_non_empty_prompt()?;
    let tags = read_tags(&tag_candidates)?;
    let recorded_prompt = if config.record_prompt && confirm("Record this task prompt?", true)? {
        Some(prompt.clone())
    } else {
        None
    };
    let task_id = Uuid::now_v7();
    event_log.append(&Event::new(
        task_id,
        EventKind::TaskReceived {
            tags,
            prompt: recorded_prompt,
            prompt_chars: prompt.chars().count(),
        },
    ))?;

    let profile = loop {
        let labels: Vec<_> = available
            .iter()
            .map(|profile| format!("{} [{}]", profile.name, profile.adapter))
            .collect();
        let selected = choose_profile(&labels)?;
        let profile = &available[selected];
        print_execution_summary(profile, &prompt, &working_directory);
        if confirm("Run this task?", true)? {
            break profile;
        }
    };
    event_log.append(&Event::new(
        task_id,
        EventKind::Assigned {
            profile_id: profile.id.clone(),
        },
    ))?;

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

    let accepted = match confirm_required("Accept this result?") {
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

fn confirm_required(prompt: &str) -> io::Result<bool> {
    loop {
        let input = read_line(&format!("{prompt} [y/n]"))?;
        match input.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!("Please answer y or n."),
        }
    }
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

fn prompt_is_valid(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn resolve_executable(profile: &ExecutionProfile) -> Option<PathBuf> {
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
            println!("  {}. {candidate}", index + 1);
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
            let tag = item
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1))
                .and_then(|index| candidates.get(index))
                .cloned()
                .unwrap_or_else(|| item.to_owned());
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

fn print_execution_summary(profile: &ExecutionProfile, prompt: &str, working_directory: &Path) {
    println!("\nTask: {prompt}");
    println!("Profile: {} ({})", profile.name, profile.id);
    println!("Adapter: {}", profile.adapter);
    println!(
        "Model: {}",
        profile.model.as_deref().unwrap_or("CLI default")
    );
    println!("Working directory: {}", working_directory.display());
    println!("Permissions: inherited from the agent CLI");
    if !profile.args.is_empty() {
        println!("Additional arguments: {:?}", profile.args);
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

    #[test]
    fn rejects_blank_prompts() {
        assert!(!prompt_is_valid(""));
        assert!(!prompt_is_valid("  \t"));
        assert!(prompt_is_valid("do the work"));
    }

    #[test]
    fn parses_confirmation_answers_and_defaults() {
        assert_eq!(parse_confirmation("", true), Some(true));
        assert_eq!(parse_confirmation("  ", false), Some(false));
        assert_eq!(parse_confirmation("Y", false), Some(true));
        assert_eq!(parse_confirmation("yes", false), Some(true));
        assert_eq!(parse_confirmation("N", true), Some(false));
        assert_eq!(parse_confirmation("no", true), Some(false));
        assert_eq!(parse_confirmation("maybe", true), None);
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
    fn reports_an_explicit_missing_executable_as_unavailable() {
        let profile = ExecutionProfile {
            id: "missing".to_owned(),
            name: "Missing".to_owned(),
            adapter: agent_dock::AdapterKind::Claude,
            executable: Some(std::path::PathBuf::from("/definitely/not/installed")),
            model: None,
            args: Vec::new(),
        };

        assert!(resolve_executable(&profile).is_none());
    }

    #[test]
    fn finds_an_executable_in_the_given_path() {
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
    }

    #[test]
    fn rejects_a_non_executable_file_in_the_given_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-agent");
        fs::write(&executable, "not executable\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&executable, permissions).unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();

        assert!(executable_in_path(Path::new("fake-agent"), &path).is_none());
    }

    #[test]
    fn parses_numbers_free_tags_and_duplicates() {
        let candidates = vec!["rust".to_owned(), "docs".to_owned()];
        assert_eq!(
            parse_tags(" 1, custom,2,rust,custom,9, ", &candidates),
            ["rust", "custom", "docs", "9"]
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
}

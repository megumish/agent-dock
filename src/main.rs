use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::ExitCode,
};

use agent_dock::{
    Config, ConfigError, ExecutionEvent, ExecutionOutcome, ExecutionProfile, ExecutionRequest,
    OutputSource, ReplaceConfigOutcome, default_config_path, execute,
};
use rustyline::{DefaultEditor, error::ReadlineError};
use thiserror::Error;
use tokio::sync::oneshot;

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

    let (available, unavailable): (Vec<_>, Vec<_>) = config
        .profiles
        .iter()
        .partition(|profile| executable_is_available(profile));
    for profile in unavailable {
        eprintln!(
            "Unavailable profile '{}': executable '{}' was not found",
            profile.name,
            profile.executable().display()
        );
    }
    if available.is_empty() {
        return Err(AppError::NoAvailableProfiles);
    }

    let working_directory = std::env::current_dir()?;
    let prompt = read_non_empty_prompt()?;
    let profile = loop {
        let labels: Vec<_> = available
            .iter()
            .map(|profile| format!("{} [{}]", profile.name, profile.adapter))
            .collect();
        let selected = choose_profile(&labels)?;
        let profile = available[selected];
        print_execution_summary(profile, &prompt, &working_directory);
        if confirm("Run this task?", true)? {
            break profile;
        }
    };

    let (cancellation_sender, cancellation_receiver) = oneshot::channel();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancellation_sender.send(());
        }
    });
    let outcome = execute(
        profile,
        ExecutionRequest {
            prompt,
            working_directory,
        },
        cancellation_receiver,
        print_event,
    )
    .await?;
    signal_task.abort();

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
    Ok(outcome.process_exit_code())
}

fn load_or_create_config(path: &Path) -> Result<Option<Config>, AppError> {
    load_or_create_config_with(path, &mut confirm)
}

fn load_or_create_config_with(
    path: &Path,
    ask: &mut impl FnMut(&str, bool) -> io::Result<bool>,
) -> Result<Option<Config>, AppError> {
    loop {
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
                    match Config::backup_and_replace_default_if_unchanged(path, &revision)? {
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
                            continue;
                        }
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }

        println!("Configuration does not exist: {}", path.display());
        if !ask(
            "Create default Claude, Codex, Gemini, and Antigravity profiles?",
            true,
        )? {
            println!("No configuration was created.");
            return Ok(None);
        }
        let config = Config::create_default(path)?;
        println!("Created {}", path.display());
        return Ok(Some(config));
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

fn executable_is_available(profile: &ExecutionProfile) -> bool {
    let executable = profile.executable();
    if executable.components().count() > 1 {
        is_executable_file(&executable)
    } else {
        std::env::var_os("PATH")
            .as_deref()
            .is_some_and(|path| executable_is_in_path(&executable, path))
    }
}

fn executable_is_in_path(executable: &Path, path: &OsStr) -> bool {
    std::env::split_paths(path)
        .map(|directory| directory.join(executable))
        .any(|candidate| is_executable_file(&candidate))
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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

        assert!(!executable_is_available(&profile));
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

        assert!(executable_is_in_path(Path::new("fake-agent"), &path));
        assert!(!executable_is_in_path(Path::new("missing"), &path));
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

        assert!(!executable_is_in_path(Path::new("fake-agent"), &path));
    }

    #[test]
    fn recreates_an_incompatible_configuration_only_after_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "schema_version = 1\n";
        fs::write(&path, original).unwrap();
        let mut prompts = Vec::new();
        let mut ask = |prompt: &str, default: bool| {
            prompts.push((prompt.to_owned(), default));
            Ok(true)
        };

        let config = load_or_create_config_with(&path, &mut ask)
            .unwrap()
            .unwrap();

        assert_eq!(config.api_version, agent_dock::CONFIG_API_VERSION);
        assert_eq!(
            prompts,
            [("Recreate the incompatible configuration?".to_owned(), false)]
        );
        let backups: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.toml.backup.")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), original);
    }

    #[test]
    fn preserves_an_incompatible_configuration_when_recreation_is_declined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "schema_version = 1\n";
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
    fn preserves_an_incompatible_configuration_when_confirmation_is_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "schema_version = 1\n";
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
}

use std::{fs, path::Path, process::ExitCode};

use agent_dock::{
    Config, ConfigError, ExecutionEvent, ExecutionOutcome, ExecutionProfile, ExecutionRequest,
    OutputSource, ReplaceConfigOutcome, default_config_path, execute,
};
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

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
    let theme = ColorfulTheme::default();
    let config_path = default_config_path()?;
    let Some(config) = load_or_create_config(&config_path, &theme)? else {
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
    let prompt = read_non_empty_prompt(&theme)?;
    let profile = loop {
        let labels: Vec<_> = available
            .iter()
            .map(|profile| format!("{} [{}]", profile.name, profile.adapter))
            .collect();
        let selected = Select::with_theme(&theme)
            .with_prompt("Choose a crew member")
            .items(&labels)
            .default(0)
            .interact()?;
        let profile = available[selected];
        print_execution_summary(profile, &prompt, &working_directory);
        if Confirm::with_theme(&theme)
            .with_prompt("Run this task?")
            .default(true)
            .interact()?
        {
            break profile;
        }
    };

    let cancellation = CancellationToken::new();
    let signal_token = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_token.cancel();
        }
    });
    let outcome = execute(
        profile,
        ExecutionRequest {
            prompt,
            working_directory,
        },
        cancellation,
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

fn load_or_create_config(path: &Path, theme: &ColorfulTheme) -> Result<Option<Config>, AppError> {
    load_or_create_config_with(path, &mut |prompt, default| {
        Ok(Confirm::with_theme(theme)
            .with_prompt(prompt)
            .default(default)
            .interact()?)
    })
}

fn load_or_create_config_with(
    path: &Path,
    ask: &mut impl FnMut(&str, bool) -> Result<bool, AppError>,
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
        if !ask("Create default Claude, Codex, and Gemini profiles?", true)? {
            println!("No configuration was created.");
            return Ok(None);
        }
        let config = Config::create_default(path)?;
        println!("Created {}", path.display());
        return Ok(Some(config));
    }
}

fn read_non_empty_prompt(theme: &ColorfulTheme) -> Result<String, dialoguer::Error> {
    loop {
        let prompt: String = Input::with_theme(theme)
            .with_prompt("Task")
            .interact_text()?;
        if prompt_is_valid(&prompt) {
            return Ok(prompt);
        }
        eprintln!("Task must not be empty.");
    }
}

fn prompt_is_valid(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn executable_is_available(profile: &ExecutionProfile) -> bool {
    let executable = profile.executable();
    if executable.components().count() > 1 {
        executable.is_file()
    } else {
        which::which(executable).is_ok()
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
    Dialog(#[from] dialoguer::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no configured agent CLI is available")]
    NoAvailableProfiles,
}

impl AppError {
    fn is_interrupted(&self) -> bool {
        match self {
            Self::Dialog(dialoguer::Error::IO(source)) | Self::Io(source) => {
                source.kind() == std::io::ErrorKind::Interrupted
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Err(AppError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "test interruption",
            )))
        };

        assert!(matches!(
            load_or_create_config_with(&path, &mut ask),
            Err(AppError::Io(source)) if source.kind() == std::io::ErrorKind::Interrupted
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn rejects_blank_prompts() {
        assert!(!prompt_is_valid(""));
        assert!(!prompt_is_valid("  \t"));
        assert!(prompt_is_valid("do the work"));
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
}

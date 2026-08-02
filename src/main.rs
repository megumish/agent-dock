use std::{path::Path, process::ExitCode};

use agent_dock::{
    Config, ExecutionEvent, ExecutionOutcome, ExecutionProfile, ExecutionRequest, OutputSource,
    default_config_path, execute,
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
    let config = if config_path.exists() {
        Config::load(&config_path)?
    } else {
        println!("Configuration does not exist: {}", config_path.display());
        let create = Confirm::with_theme(&theme)
            .with_prompt("Create default Claude, Codex, and Gemini profiles?")
            .default(true)
            .interact()?;
        if !create {
            println!("No configuration was created.");
            return Ok(0);
        }
        let config = Config::create_default(&config_path)?;
        println!("Created {}", config_path.display());
        config
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

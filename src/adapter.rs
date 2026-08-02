use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdapterKind {
    Test,
    Claude,
    Codex,
    Gemini,
    Antigravity,
}

impl AdapterKind {
    pub fn default_executable(self) -> &'static str {
        match self {
            Self::Test => "/usr/bin/true",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Antigravity => "agy",
        }
    }
}

impl std::fmt::Display for AdapterKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Test => formatter.write_str("test"),
            _ => formatter.write_str(self.default_executable()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProfile {
    pub id: String,
    pub name: String,
    pub adapter: AdapterKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

pub fn safe_test_profile() -> ExecutionProfile {
    ExecutionProfile {
        id: "test-default".to_owned(),
        name: "Safe test (no agent)".to_owned(),
        adapter: AdapterKind::Test,
        executable: None,
        model: None,
        args: Vec::new(),
    }
}

impl ExecutionProfile {
    pub fn executable(&self) -> PathBuf {
        self.executable
            .clone()
            .unwrap_or_else(|| PathBuf::from(self.adapter.default_executable()))
    }

    pub fn command_spec(&self, working_directory: PathBuf) -> CommandSpec {
        let mut args = Vec::new();

        match self.adapter {
            AdapterKind::Test => {
                args.extend(self.args.clone());
            }
            AdapterKind::Claude => {
                args.push("--print".to_owned());
                if let Some(model) = &self.model {
                    args.extend(["--model".to_owned(), model.clone()]);
                }
                args.extend(self.args.clone());
            }
            AdapterKind::Codex => {
                args.push("exec".to_owned());
                if let Some(model) = &self.model {
                    args.extend(["--model".to_owned(), model.clone()]);
                }
                args.extend(self.args.clone());
                args.push("-".to_owned());
            }
            AdapterKind::Gemini => {
                if let Some(model) = &self.model {
                    args.extend(["--model".to_owned(), model.clone()]);
                }
                args.extend(self.args.clone());
                args.extend(["--prompt".to_owned(), String::new()]);
            }
            AdapterKind::Antigravity => {
                if let Some(model) = &self.model {
                    args.extend(["--model".to_owned(), model.clone()]);
                }
                args.extend(self.args.clone());
                args.push("-p".to_owned());
            }
        }

        CommandSpec {
            program: self.executable(),
            args,
            working_directory,
            prompt_transport: match self.adapter {
                AdapterKind::Antigravity => PromptTransport::Argument,
                AdapterKind::Test
                | AdapterKind::Claude
                | AdapterKind::Codex
                | AdapterKind::Gemini => PromptTransport::Stdin,
            },
        }
    }

    pub fn validate(&self) -> Result<(), ProfileValidationError> {
        if self.id.is_empty()
            || !self.id.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || ".-_".contains(character)
            })
        {
            return Err(ProfileValidationError::InvalidId(self.id.clone()));
        }
        if self.name.trim().is_empty() {
            return Err(ProfileValidationError::EmptyName(self.id.clone()));
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(ProfileValidationError::EmptyModel(self.id.clone()));
        }
        if let Some(argument) = self.args.iter().find(|argument| {
            adapter_owned_arguments(self.adapter)
                .iter()
                .any(|owned| argument == owned || argument.starts_with(&format!("{owned}=")))
        }) {
            return Err(ProfileValidationError::ReservedArgument {
                id: self.id.clone(),
                argument: argument.clone(),
            });
        }
        Ok(())
    }
}

fn adapter_owned_arguments(adapter: AdapterKind) -> &'static [&'static str] {
    match adapter {
        AdapterKind::Test => &[],
        AdapterKind::Claude => &["-p", "--print", "--model"],
        AdapterKind::Codex => &["exec", "-m", "--model", "-C", "--cd"],
        AdapterKind::Gemini => &["-p", "--prompt", "-m", "--model"],
        AdapterKind::Antigravity => &["-p", "--print", "--model"],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTransport {
    Stdin,
    Argument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub prompt_transport: PromptTransport,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileValidationError {
    #[error("profile id `{0}` must contain only lowercase ASCII letters, digits, '.', '-', or '_'")]
    InvalidId(String),
    #[error("profile `{0}` has an empty name")]
    EmptyName(String),
    #[error("profile `{0}` has an empty model")]
    EmptyModel(String),
    #[error("profile `{id}` uses adapter-owned argument `{argument}`")]
    ReservedArgument { id: String, argument: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(adapter: AdapterKind) -> ExecutionProfile {
        ExecutionProfile {
            id: "test-profile".to_owned(),
            name: "Test profile".to_owned(),
            adapter,
            executable: None,
            model: Some("test-model".to_owned()),
            args: vec!["--safe-option".to_owned()],
        }
    }

    #[test]
    fn builds_claude_command() {
        let spec = profile(AdapterKind::Claude).command_spec(PathBuf::from("/work"));
        assert_eq!(spec.program, PathBuf::from("claude"));
        assert_eq!(
            spec.args,
            ["--print", "--model", "test-model", "--safe-option"]
        );
    }

    #[test]
    fn builds_a_safe_test_command_without_agent_arguments() {
        let mut profile = profile(AdapterKind::Test);
        profile.model = None;
        profile.args.clear();

        let spec = profile.command_spec(PathBuf::from("/work"));

        assert_eq!(spec.program, PathBuf::from("/usr/bin/true"));
        assert!(spec.args.is_empty());
        assert_eq!(spec.prompt_transport, PromptTransport::Stdin);
    }

    #[test]
    fn builds_codex_command_with_stdin_marker() {
        let spec = profile(AdapterKind::Codex).command_spec(PathBuf::from("/work"));
        assert_eq!(spec.program, PathBuf::from("codex"));
        assert_eq!(
            spec.args,
            ["exec", "--model", "test-model", "--safe-option", "-"]
        );
    }

    #[test]
    fn builds_gemini_command_with_empty_prompt_argument() {
        let spec = profile(AdapterKind::Gemini).command_spec(PathBuf::from("/work"));
        assert_eq!(spec.program, PathBuf::from("gemini"));
        assert_eq!(
            spec.args,
            ["--model", "test-model", "--safe-option", "--prompt", ""]
        );
        assert_eq!(spec.prompt_transport, PromptTransport::Stdin);
    }

    #[test]
    fn builds_antigravity_command_with_prompt_argument_marker() {
        let spec = profile(AdapterKind::Antigravity).command_spec(PathBuf::from("/work"));
        assert_eq!(spec.program, PathBuf::from("agy"));
        assert_eq!(spec.args, ["--model", "test-model", "--safe-option", "-p"]);
        assert_eq!(spec.prompt_transport, PromptTransport::Argument);
    }

    #[test]
    fn rejects_adapter_owned_arguments() {
        let mut profile = profile(AdapterKind::Codex);
        profile.args = vec!["--model=other".to_owned()];
        assert!(matches!(
            profile.validate(),
            Err(ProfileValidationError::ReservedArgument { .. })
        ));
    }
}

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const PROFILE_IDENTITY_NAMESPACE: Uuid = Uuid::from_u128(0xa5e6727d6a3755978280568e3f54c1ff);
const PROFILE_IDENTITY_DOMAIN: &[u8] = b"agent-dock/profile-identity/v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliKind {
    Test,
    Claude,
    Codex,
    Gemini,
    Antigravity,
}

impl CliKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
        }
    }

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

impl std::fmt::Display for CliKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDeclaration {
    pub name: String,
    pub cli: CliKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionProfile {
    pub id: String,
    pub declaration: ProfileDeclaration,
    pub resolved_executable: PathBuf,
}

impl ProfileDeclaration {
    pub fn id(&self) -> Result<String, ProfileValidationError> {
        Ok(Uuid::new_v5(&PROFILE_IDENTITY_NAMESPACE, &self.canonical_identity()?).to_string())
    }

    pub fn canonical_identity(&self) -> Result<Vec<u8>, ProfileValidationError> {
        let mut encoded = PROFILE_IDENTITY_DOMAIN.to_vec();
        encoded.push(0x01);
        push_string(&mut encoded, self.cli.as_str())?;
        encoded.push(0x02);
        push_option_string(
            &mut encoded,
            self.executable
                .as_ref()
                .map(|path| {
                    path.to_str()
                        .map(std::borrow::Cow::Borrowed)
                        .ok_or_else(|| ProfileValidationError::NonUtf8Executable(self.name.clone()))
                })
                .transpose()?,
        )?;
        encoded.push(0x03);
        push_option_string(&mut encoded, self.model.as_deref().map(Into::into))?;
        encoded.push(0x04);
        push_len(&mut encoded, self.args.len())?;
        for argument in &self.args {
            push_string(&mut encoded, argument)?;
        }
        Ok(encoded)
    }

    pub fn executable(&self) -> PathBuf {
        self.executable
            .clone()
            .unwrap_or_else(|| PathBuf::from(self.cli.default_executable()))
    }

    pub fn resolve(
        &self,
        resolved_executable: PathBuf,
    ) -> Result<ExecutionProfile, ProfileValidationError> {
        Ok(ExecutionProfile {
            id: self.id()?,
            declaration: self.clone(),
            resolved_executable,
        })
    }

    pub fn validate(&self, allow_test: bool) -> Result<(), ProfileValidationError> {
        if self.name.trim().is_empty() {
            return Err(ProfileValidationError::EmptyName(self.name.clone()));
        }
        if self.name.trim() != self.name {
            return Err(ProfileValidationError::PaddedName(self.name.clone()));
        }
        if !allow_test && self.cli == CliKind::Test {
            return Err(ProfileValidationError::ReservedTestCli(self.name.clone()));
        }
        if self
            .executable
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ProfileValidationError::EmptyExecutable(self.name.clone()));
        }
        if self.model.as_ref().is_some_and(String::is_empty) {
            return Err(ProfileValidationError::EmptyModel(self.name.clone()));
        }
        if let Some(argument) = self.args.iter().find(|argument| {
            cli_owned_arguments(self.cli)
                .iter()
                .any(|owned| argument == owned || argument.starts_with(&format!("{owned}=")))
        }) {
            return Err(ProfileValidationError::ReservedArgument {
                name: self.name.clone(),
                argument: argument.clone(),
            });
        }
        self.canonical_identity()?;
        Ok(())
    }
}

impl ExecutionProfile {
    pub fn name(&self) -> &str {
        &self.declaration.name
    }
    pub fn cli(&self) -> CliKind {
        self.declaration.cli
    }
    pub fn model(&self) -> Option<&str> {
        self.declaration.model.as_deref()
    }
    pub fn args(&self) -> &[String] {
        &self.declaration.args
    }

    pub fn command_spec(&self, working_directory: PathBuf) -> CommandSpec {
        let mut args = Vec::new();
        match self.cli() {
            CliKind::Test => args.extend(self.args().to_owned()),
            CliKind::Claude => {
                args.push("--print".to_owned());
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
            }
            CliKind::Codex => {
                args.push("exec".to_owned());
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
                args.push("-".to_owned());
            }
            CliKind::Gemini => {
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
                args.extend(["--prompt".to_owned(), String::new()]);
            }
            CliKind::Antigravity => {
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
                args.push("-p".to_owned());
            }
        }
        CommandSpec {
            program: self.resolved_executable.clone(),
            args,
            working_directory,
            prompt_transport: if self.cli() == CliKind::Antigravity {
                PromptTransport::Argument
            } else {
                PromptTransport::Stdin
            },
        }
    }
}

fn push_model(args: &mut Vec<String>, model: Option<&str>) {
    if let Some(model) = model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
}

fn push_len(output: &mut Vec<u8>, length: usize) -> Result<(), ProfileValidationError> {
    let length = u32::try_from(length).map_err(|_| ProfileValidationError::IdentityTooLarge)?;
    output.extend(length.to_be_bytes());
    Ok(())
}

fn push_string(output: &mut Vec<u8>, value: &str) -> Result<(), ProfileValidationError> {
    push_len(output, value.len())?;
    output.extend(value.as_bytes());
    Ok(())
}

fn push_option_string(
    output: &mut Vec<u8>,
    value: Option<std::borrow::Cow<'_, str>>,
) -> Result<(), ProfileValidationError> {
    match value {
        None => output.push(0x00),
        Some(value) => {
            output.push(0x01);
            push_string(output, &value)?;
        }
    }
    Ok(())
}

fn cli_owned_arguments(cli: CliKind) -> &'static [&'static str] {
    match cli {
        CliKind::Test => &[],
        CliKind::Claude => &["-p", "--print", "--model"],
        CliKind::Codex => &["exec", "-m", "--model", "-C", "--cd"],
        CliKind::Gemini => &["-p", "--prompt", "-m", "--model"],
        CliKind::Antigravity => &["-p", "--print", "--model"],
    }
}

pub fn safe_test_declaration() -> ProfileDeclaration {
    ProfileDeclaration {
        name: "Safe test (no agent)".to_owned(),
        cli: CliKind::Test,
        executable: None,
        model: None,
        args: Vec::new(),
    }
}

pub fn safe_test_profile() -> ExecutionProfile {
    safe_test_declaration()
        .resolve(PathBuf::from("/usr/bin/true"))
        .expect("the built-in safe-test profile is valid")
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
    #[error("profile `{0}` has an empty name")]
    EmptyName(String),
    #[error("profile name `{0}` has leading or trailing whitespace")]
    PaddedName(String),
    #[error("profile `{0}` cannot use the reserved test CLI")]
    ReservedTestCli(String),
    #[error("profile `{0}` has an empty executable override")]
    EmptyExecutable(String),
    #[error("profile `{0}` has a non-UTF-8 executable override")]
    NonUtf8Executable(String),
    #[error("profile `{0}` has an empty model")]
    EmptyModel(String),
    #[error("profile `{name}` uses CLI-owned argument `{argument}`")]
    ReservedArgument { name: String, argument: String },
    #[error("profile identity contains a value too large to encode")]
    IdentityTooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(cli: CliKind) -> ProfileDeclaration {
        ProfileDeclaration {
            name: "Test profile".to_owned(),
            cli,
            executable: None,
            model: Some("test-model".to_owned()),
            args: vec!["--safe-option".to_owned()],
        }
    }

    #[test]
    fn derives_stable_profile_identity() {
        let profile = ProfileDeclaration {
            name: "ignored".to_owned(),
            cli: CliKind::Codex,
            executable: None,
            model: Some("gpt-5".to_owned()),
            args: vec!["--foo".to_owned(), "bar".to_owned()],
        };
        assert_eq!(
            hex(&profile.canonical_identity().unwrap()),
            "6167656e742d646f636b2f70726f66696c652d6964656e746974792f7631000100000005636f64657802000301000000056770742d350400000002000000052d2d666f6f00000003626172"
        );
        assert_eq!(
            profile.id().unwrap(),
            "fd1b9963-4d3e-516a-92be-9a18305c71c7"
        );
        assert_eq!(
            safe_test_profile().id,
            "42d38cde-50a8-5024-9f12-d164e82adea6"
        );
    }

    #[test]
    fn identity_ignores_name_but_not_declared_execution_fields() {
        let base = declaration(CliKind::Codex);
        let mut renamed = base.clone();
        renamed.name = "Renamed".to_owned();
        assert_eq!(base.id().unwrap(), renamed.id().unwrap());
        let mut changed = base.clone();
        changed.args.push("x".to_owned());
        assert_ne!(base.id().unwrap(), changed.id().unwrap());
    }

    #[test]
    fn builds_command_specs_for_every_cli() {
        let cases = [
            (
                safe_test_profile(),
                "/usr/bin/true",
                vec![],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Claude)
                    .resolve("claude".into())
                    .unwrap(),
                "claude",
                vec!["--print", "--model", "test-model", "--safe-option"],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Codex).resolve("codex".into()).unwrap(),
                "codex",
                vec!["exec", "--model", "test-model", "--safe-option", "-"],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Gemini)
                    .resolve("gemini".into())
                    .unwrap(),
                "gemini",
                vec!["--model", "test-model", "--safe-option", "--prompt", ""],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Antigravity)
                    .resolve("agy".into())
                    .unwrap(),
                "agy",
                vec!["--model", "test-model", "--safe-option", "-p"],
                PromptTransport::Argument,
            ),
        ];
        for (profile, program, args, transport) in cases {
            let spec = profile.command_spec("/work".into());
            assert_eq!(spec.program, PathBuf::from(program));
            assert_eq!(spec.args, args);
            assert_eq!(spec.prompt_transport, transport);
        }
    }

    #[test]
    fn rejects_cli_owned_arguments() {
        for (cli, argument) in [
            (CliKind::Claude, "--print"),
            (CliKind::Codex, "--model=other"),
            (CliKind::Gemini, "--prompt"),
            (CliKind::Antigravity, "-p"),
        ] {
            let mut profile = declaration(cli);
            profile.args = vec![argument.to_owned()];
            assert!(matches!(
                profile.validate(false),
                Err(ProfileValidationError::ReservedArgument { .. })
            ));
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

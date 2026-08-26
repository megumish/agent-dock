use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const PROFILE_IDENTITY_NAMESPACE: Uuid = Uuid::from_u128(0xa5e6727d6a3755978280568e3f54c1ff);
const PROFILE_IDENTITY_DOMAIN: &[u8] = b"agent-dock/profile-identity/v2\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliKind {
    Test,
    Claude,
    Codex,
    Gemini,
    Antigravity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPlatform {
    #[default]
    Headless,
    Interactive,
}

impl std::fmt::Display for ExecutionPlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Headless => "headless",
            Self::Interactive => "interactive",
        })
    }
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
    #[serde(default)]
    pub execution_platform: ExecutionPlatform,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub identity: BTreeMap<String, String>,
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
        push_string(&mut encoded, &self.execution_platform.to_string())?;
        encoded.push(0x03);
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
        encoded.push(0x04);
        push_option_string(&mut encoded, self.model.as_deref().map(Into::into))?;
        encoded.push(0x05);
        push_len(&mut encoded, self.args.len())?;
        for argument in &self.args {
            push_string(&mut encoded, argument)?;
        }
        encoded.push(0x06);
        push_len(&mut encoded, self.identity.len())?;
        for (key, value) in &self.identity {
            push_string(&mut encoded, key)?;
            push_string(&mut encoded, value)?;
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
        if self.execution_platform == ExecutionPlatform::Interactive
            && !matches!(self.cli, CliKind::Claude | CliKind::Codex)
        {
            return Err(ProfileValidationError::UnsupportedInteractiveCli {
                name: self.name.clone(),
                cli: self.cli,
            });
        }
        if self.execution_platform == ExecutionPlatform::Interactive
            && (self.executable.is_some() || !self.args.is_empty())
        {
            return Err(ProfileValidationError::InteractiveLaunchFields(
                self.name.clone(),
            ));
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
    pub fn execution_platform(&self) -> ExecutionPlatform {
        self.declaration.execution_platform
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
                args.extend(["--output-format".to_owned(), "json".to_owned()]);
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
            }
            CliKind::Codex => {
                args.push("exec".to_owned());
                args.push("--json".to_owned());
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
                args.push("-".to_owned());
            }
            CliKind::Gemini => {
                args.extend(["--output-format".to_owned(), "json".to_owned()]);
                push_model(&mut args, self.model());
                args.extend(self.args().to_owned());
                args.extend(["--prompt".to_owned(), String::new()]);
            }
            CliKind::Antigravity => {
                args.extend(["--output-format".to_owned(), "json".to_owned()]);
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
        CliKind::Claude => &["-p", "--print", "--output-format", "--model"],
        CliKind::Codex => &["exec", "--json", "-m", "--model", "-C", "--cd"],
        CliKind::Gemini => &["-p", "--prompt", "-m", "--model", "--output-format", "-o"],
        CliKind::Antigravity => &["-p", "--print", "--model", "--output-format"],
    }
}

pub fn safe_test_declaration() -> ProfileDeclaration {
    ProfileDeclaration {
        name: "Safe test (no agent)".to_owned(),
        cli: CliKind::Test,
        execution_platform: ExecutionPlatform::Headless,
        executable: None,
        model: None,
        args: Vec::new(),
        identity: BTreeMap::new(),
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
    #[error("interactive profile `{name}` cannot use CLI `{cli}`")]
    UnsupportedInteractiveCli { name: String, cli: CliKind },
    #[error("interactive profile `{0}` cannot declare executable or launch arguments")]
    InteractiveLaunchFields(String),
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
            execution_platform: ExecutionPlatform::Headless,
            executable: None,
            model: Some("test-model".to_owned()),
            args: vec!["--safe-option".to_owned()],
            identity: BTreeMap::new(),
        }
    }

    #[test]
    fn derives_stable_profile_identity() {
        let profile = ProfileDeclaration {
            name: "ignored".to_owned(),
            cli: CliKind::Codex,
            execution_platform: ExecutionPlatform::Headless,
            executable: None,
            model: Some("gpt-5".to_owned()),
            args: vec!["--foo".to_owned(), "bar".to_owned()],
            identity: BTreeMap::new(),
        };
        assert_eq!(
            hex(&profile.canonical_identity().unwrap()),
            "6167656e742d646f636b2f70726f66696c652d6964656e746974792f7632000100000005636f6465780200000008686561646c65737303000401000000056770742d350500000002000000052d2d666f6f000000036261720600000000"
        );
        assert_eq!(
            profile.id().unwrap(),
            "1592eb4a-f65d-5373-a66f-7e821622d652"
        );
        assert_eq!(
            safe_test_profile().id,
            "570e4209-66ea-5701-b399-d631bf676519"
        );
    }

    #[test]
    fn identity_ignores_name_but_not_declared_execution_fields() {
        let mut base = declaration(CliKind::Codex);
        base.args.push("second".to_owned());
        let mut renamed = base.clone();
        renamed.name = "Renamed".to_owned();
        assert_eq!(base.id().unwrap(), renamed.id().unwrap());

        let mut changed_cli = base.clone();
        changed_cli.cli = CliKind::Claude;
        let mut changed_executable = base.clone();
        changed_executable.executable = Some("/custom/codex".into());
        let mut changed_model = base.clone();
        changed_model.model = Some("other-model".to_owned());
        let mut reordered_args = base.clone();
        reordered_args.args.reverse();
        for changed in [
            changed_cli,
            changed_executable,
            changed_model,
            reordered_args,
        ] {
            assert_ne!(base.id().unwrap(), changed.id().unwrap());
        }
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
                vec![
                    "--print",
                    "--output-format",
                    "json",
                    "--model",
                    "test-model",
                    "--safe-option",
                ],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Codex).resolve("codex".into()).unwrap(),
                "codex",
                vec![
                    "exec",
                    "--json",
                    "--model",
                    "test-model",
                    "--safe-option",
                    "-",
                ],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Gemini)
                    .resolve("gemini".into())
                    .unwrap(),
                "gemini",
                vec![
                    "--output-format",
                    "json",
                    "--model",
                    "test-model",
                    "--safe-option",
                    "--prompt",
                    "",
                ],
                PromptTransport::Stdin,
            ),
            (
                declaration(CliKind::Antigravity)
                    .resolve("agy".into())
                    .unwrap(),
                "agy",
                vec![
                    "--output-format",
                    "json",
                    "--model",
                    "test-model",
                    "--safe-option",
                    "-p",
                ],
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
    fn rejects_invalid_profile_declarations() {
        for (cli, argument) in [
            (CliKind::Claude, "--print"),
            (CliKind::Claude, "--output-format"),
            (CliKind::Claude, "--output-format=json"),
            (CliKind::Codex, "--model=other"),
            (CliKind::Codex, "--json"),
            (CliKind::Gemini, "--prompt"),
            (CliKind::Gemini, "--output-format"),
            (CliKind::Gemini, "--output-format=json"),
            (CliKind::Gemini, "-o"),
            (CliKind::Gemini, "-o=json"),
            (CliKind::Antigravity, "-p"),
            (CliKind::Antigravity, "--output-format"),
            (CliKind::Antigravity, "--output-format=json"),
        ] {
            let mut profile = declaration(cli);
            profile.args = vec![argument.to_owned()];
            assert!(matches!(
                profile.validate(false),
                Err(ProfileValidationError::ReservedArgument { .. })
            ));
        }

        for name in [" Padded", "Padded "] {
            let mut padded = declaration(CliKind::Codex);
            padded.name = name.to_owned();
            assert!(matches!(
                padded.validate(false),
                Err(ProfileValidationError::PaddedName(_))
            ));
        }

        let test = declaration(CliKind::Test);
        assert!(matches!(
            test.validate(false),
            Err(ProfileValidationError::ReservedTestCli(_))
        ));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

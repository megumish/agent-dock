use std::{ffi::OsString, path::PathBuf};

use uuid::Uuid;

use crate::{AttributionFailure, CliKind, ObservedTaskStatus, SessionPhase};

const USAGE: &str = "Usage: agent-dock observe \
<session-start|session-resume|session-end|session-find|task-start|task-complete|task-interrupt|task-attribute|task-unattribute|hook> ...";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveCliCommand {
    Session {
        phase: SessionPhase,
        session_id: Option<Uuid>,
        cli: CliKind,
        external_session_id: Option<String>,
        working_directory: Option<PathBuf>,
    },
    SessionFind {
        cli: CliKind,
        external_session_id: String,
    },
    TaskStart {
        session_id: Uuid,
        profile: Option<String>,
        tags: Vec<String>,
        prompt_chars: usize,
    },
    TaskEnd {
        session_id: Uuid,
        profile: Option<String>,
        status: ObservedTaskStatus,
    },
    Attribution {
        session_id: Uuid,
        task_id: Uuid,
        profile: Option<String>,
        failure: Option<AttributionFailure>,
    },
    Hook {
        provider: crate::hook::Provider,
    },
}

pub fn parse_observe_args(
    args: impl Iterator<Item = OsString>,
) -> Result<ObserveCliCommand, String> {
    let mut args = args;
    let command = args.next().ok_or_else(|| usage("missing command"))?;
    let command = command
        .to_str()
        .ok_or_else(|| usage("command is not valid UTF-8"))?;

    match command {
        "session-start" => parse_session(args, SessionPhase::Started, false),
        "session-resume" => parse_session(args, SessionPhase::Resumed, false),
        "session-end" => parse_session(args, SessionPhase::Ended, true),
        "session-find" => parse_session_find(args),
        "task-start" => parse_task_start(args),
        "task-complete" => parse_task_end(args, ObservedTaskStatus::Completed),
        "task-interrupt" => parse_task_end(args, ObservedTaskStatus::Interrupted),
        "task-attribute" => parse_attribution(args, true),
        "task-unattribute" => parse_attribution(args, false),
        "hook" => parse_hook(args),
        _ => Err(usage("unknown command")),
    }
}

fn parse_session_find(
    mut args: impl Iterator<Item = OsString>,
) -> Result<ObserveCliCommand, String> {
    let mut cli = None;
    let mut external_session_id = None;
    while let Some(option) = args.next() {
        match utf8(option, "option is not valid UTF-8")?.as_str() {
            "--cli" => {
                ensure_unset(&cli)?;
                cli = Some(parse_cli(value(&mut args)?)?);
            }
            "--external-id" => {
                ensure_unset(&external_session_id)?;
                external_session_id = Some(nonempty(value(&mut args)?)?);
            }
            _ => return Err(usage("unknown or trailing argument")),
        }
    }
    Ok(ObserveCliCommand::SessionFind {
        cli: cli.ok_or_else(|| usage("missing required option"))?,
        external_session_id: external_session_id.ok_or_else(|| usage("missing required option"))?,
    })
}

fn parse_attribution(
    mut args: impl Iterator<Item = OsString>,
    matched: bool,
) -> Result<ObserveCliCommand, String> {
    let mut session_id = None;
    let mut task_id = None;
    let mut profile = None;
    let mut failure = None;
    while let Some(option) = args.next() {
        let option = utf8(option, "option is not valid UTF-8")?;
        match option.as_str() {
            "--session" => {
                ensure_unset(&session_id)?;
                session_id = Some(parse_uuid(value(&mut args)?)?);
            }
            "--task" => {
                ensure_unset(&task_id)?;
                task_id = Some(parse_uuid(value(&mut args)?)?);
            }
            "--profile" if matched => {
                ensure_unset(&profile)?;
                profile = Some(nonempty(value(&mut args)?)?);
            }
            "--reason" if !matched => {
                ensure_unset(&failure)?;
                failure = Some(match value(&mut args)?.as_str() {
                    "missing-configuration" => AttributionFailure::MissingConfiguration,
                    "configuration-changed" => AttributionFailure::ConfigurationChanged,
                    "no-matching-profile" => AttributionFailure::NoMatchingProfile,
                    "ambiguous-profile" => AttributionFailure::AmbiguousProfile,
                    _ => return Err(usage("invalid attribution reason")),
                });
            }
            _ => return Err(usage("unknown or trailing argument")),
        }
    }
    if matched && profile.is_none() || !matched && failure.is_none() {
        return Err(usage("missing required option"));
    }
    Ok(ObserveCliCommand::Attribution {
        session_id: session_id.ok_or_else(|| usage("missing required option"))?,
        task_id: task_id.ok_or_else(|| usage("missing required option"))?,
        profile,
        failure,
    })
}

fn parse_session(
    mut args: impl Iterator<Item = OsString>,
    phase: SessionPhase,
    require_correlation: bool,
) -> Result<ObserveCliCommand, String> {
    let mut cli = None;
    let mut session_id = None;
    let mut external_session_id = None;
    let mut working_directory = None;

    while let Some(option) = args.next() {
        let option = utf8(option, "option is not valid UTF-8")?;
        match option.as_str() {
            "--cli" => {
                ensure_unset(&cli)?;
                cli = Some(parse_cli(value(&mut args)?)?);
            }
            "--session" => {
                ensure_unset(&session_id)?;
                session_id = Some(parse_uuid(value(&mut args)?)?);
            }
            "--external-id" => {
                ensure_unset(&external_session_id)?;
                external_session_id = Some(nonempty(value(&mut args)?)?);
            }
            "--cwd" => {
                ensure_unset(&working_directory)?;
                working_directory = Some(PathBuf::from(nonempty(value(&mut args)?)?));
            }
            _ => return Err(usage("unknown or trailing argument")),
        }
    }

    let cli = cli.ok_or_else(|| usage("missing required option"))?;
    if require_correlation && session_id.is_none() && external_session_id.is_none() {
        return Err(usage("session-end requires a session or external id"));
    }

    Ok(ObserveCliCommand::Session {
        phase,
        session_id,
        cli,
        external_session_id,
        working_directory,
    })
}

fn parse_task_start(mut args: impl Iterator<Item = OsString>) -> Result<ObserveCliCommand, String> {
    let mut session_id = None;
    let mut profile = None;
    let mut tags = Vec::new();
    let mut prompt_chars = None;

    while let Some(option) = args.next() {
        let option = utf8(option, "option is not valid UTF-8")?;
        match option.as_str() {
            "--session" => {
                ensure_unset(&session_id)?;
                session_id = Some(parse_uuid(value(&mut args)?)?);
            }
            "--profile" => {
                ensure_unset(&profile)?;
                profile = Some(nonempty(value(&mut args)?)?);
            }
            "--tag" => tags.push(nonempty(value(&mut args)?)?),
            "--prompt-chars" => {
                ensure_unset(&prompt_chars)?;
                prompt_chars = Some(
                    value(&mut args)?
                        .parse()
                        .map_err(|_| usage("invalid prompt character count"))?,
                );
            }
            _ => return Err(usage("unknown or trailing argument")),
        }
    }

    Ok(ObserveCliCommand::TaskStart {
        session_id: session_id.ok_or_else(|| usage("missing required option"))?,
        profile,
        tags,
        prompt_chars: prompt_chars.unwrap_or(0),
    })
}

fn parse_task_end(
    mut args: impl Iterator<Item = OsString>,
    status: ObservedTaskStatus,
) -> Result<ObserveCliCommand, String> {
    let mut session_id = None;
    let mut profile = None;

    while let Some(option) = args.next() {
        let option = utf8(option, "option is not valid UTF-8")?;
        match option.as_str() {
            "--session" => {
                ensure_unset(&session_id)?;
                session_id = Some(parse_uuid(value(&mut args)?)?);
            }
            "--profile" => {
                ensure_unset(&profile)?;
                profile = Some(nonempty(value(&mut args)?)?);
            }
            _ => return Err(usage("unknown or trailing argument")),
        }
    }

    Ok(ObserveCliCommand::TaskEnd {
        session_id: session_id.ok_or_else(|| usage("missing required option"))?,
        profile,
        status,
    })
}

fn parse_hook(mut args: impl Iterator<Item = OsString>) -> Result<ObserveCliCommand, String> {
    let provider = args.next().ok_or_else(|| usage("missing hook provider"))?;
    let provider = utf8(provider, "hook provider is not valid UTF-8")?;
    let provider = match provider.as_str() {
        "claude" => crate::hook::Provider::Claude,
        "codex" => crate::hook::Provider::Codex,
        _ => return Err(usage("unsupported hook provider")),
    };
    if args.next().is_some() {
        return Err(usage("trailing hook argument"));
    }
    Ok(ObserveCliCommand::Hook { provider })
}

fn value(args: &mut impl Iterator<Item = OsString>) -> Result<String, String> {
    let value = args.next().ok_or_else(|| usage("missing option value"))?;
    let value = utf8(value, "option value is not valid UTF-8")?;
    if value.starts_with("--") {
        return Err(usage("missing option value"));
    }
    Ok(value)
}

fn utf8(value: OsString, reason: &'static str) -> Result<String, String> {
    value.into_string().map_err(|_| usage(reason))
}

fn nonempty(value: String) -> Result<String, String> {
    if value.is_empty() {
        Err(usage("option value must not be empty"))
    } else {
        Ok(value)
    }
}

fn parse_uuid(value: String) -> Result<Uuid, String> {
    value.parse().map_err(|_| usage("invalid UUID"))
}

fn parse_cli(value: String) -> Result<CliKind, String> {
    match value.as_str() {
        "claude" => Ok(CliKind::Claude),
        "codex" => Ok(CliKind::Codex),
        _ => Err(usage("unsupported CLI")),
    }
}

fn ensure_unset<T>(slot: &Option<T>) -> Result<(), String> {
    if slot.is_some() {
        Err(usage("duplicate option"))
    } else {
        Ok(())
    }
}

fn usage(reason: &str) -> String {
    format!("{reason}\n{USAGE}")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    const SESSION: &str = "01910d70-2e49-7abf-86a9-f140cf0a8d36";

    fn parse(args: &[&str]) -> Result<ObserveCliCommand, String> {
        parse_observe_args(args.iter().map(|arg| OsString::from(*arg)))
    }

    fn session_id() -> Uuid {
        SESSION.parse().unwrap()
    }

    #[test]
    fn parses_all_session_phases_and_optional_fields() {
        assert_eq!(
            parse(&[
                "session-start",
                "--external-id",
                "provider-1",
                "--cwd",
                "/work",
                "--cli",
                "claude",
                "--session",
                SESSION,
            ]),
            Ok(ObserveCliCommand::Session {
                phase: SessionPhase::Started,
                session_id: Some(session_id()),
                cli: CliKind::Claude,
                external_session_id: Some("provider-1".to_owned()),
                working_directory: Some(PathBuf::from("/work")),
            })
        );
        assert!(matches!(
            parse(&["session-resume", "--cli", "codex"]),
            Ok(ObserveCliCommand::Session {
                phase: SessionPhase::Resumed,
                cli: CliKind::Codex,
                ..
            })
        ));
        assert!(matches!(
            parse(&["session-end", "--cli", "codex", "--session", SESSION]),
            Ok(ObserveCliCommand::Session {
                phase: SessionPhase::Ended,
                ..
            })
        ));
        assert!(matches!(
            parse(&[
                "session-end",
                "--cli",
                "claude",
                "--external-id",
                "provider-1",
            ]),
            Ok(ObserveCliCommand::Session {
                phase: SessionPhase::Ended,
                ..
            })
        ));
    }

    #[test]
    fn parses_session_lookup_by_external_id() {
        assert_eq!(
            parse(&[
                "session-find",
                "--cli",
                "codex",
                "--external-id",
                "vendor-session",
            ]),
            Ok(ObserveCliCommand::SessionFind {
                cli: CliKind::Codex,
                external_session_id: "vendor-session".to_owned(),
            })
        );
    }

    #[test]
    fn parses_task_commands_and_defaults_prompt_chars() {
        assert_eq!(
            parse(&[
                "task-start",
                "--tag",
                "rust",
                "--session",
                SESSION,
                "--tag",
                "cli",
                "--profile",
                "interactive-codex",
                "--prompt-chars",
                "42",
            ]),
            Ok(ObserveCliCommand::TaskStart {
                session_id: session_id(),
                profile: Some("interactive-codex".to_owned()),
                tags: vec!["rust".to_owned(), "cli".to_owned()],
                prompt_chars: 42,
            })
        );
        assert!(matches!(
            parse(&["task-start", "--session", SESSION, "--profile", "profile",]),
            Ok(ObserveCliCommand::TaskStart {
                prompt_chars: 0,
                ..
            })
        ));
        assert!(matches!(
            parse(&[
                "task-complete",
                "--session",
                SESSION,
                "--profile",
                "profile",
            ]),
            Ok(ObserveCliCommand::TaskEnd {
                status: ObservedTaskStatus::Completed,
                ..
            })
        ));
        assert!(matches!(
            parse(&[
                "task-interrupt",
                "--session",
                SESSION,
                "--profile",
                "profile",
            ]),
            Ok(ObserveCliCommand::TaskEnd {
                status: ObservedTaskStatus::Interrupted,
                ..
            })
        ));
    }

    #[test]
    fn parses_supported_hook_providers_only() {
        assert_eq!(
            parse(&["hook", "claude"]),
            Ok(ObserveCliCommand::Hook {
                provider: crate::hook::Provider::Claude,
            })
        );
        assert_eq!(
            parse(&["hook", "codex"]),
            Ok(ObserveCliCommand::Hook {
                provider: crate::hook::Provider::Codex,
            })
        );
        assert!(parse(&["hook", "gemini"]).is_err());
    }

    #[test]
    fn parses_attribution_corrections() {
        assert!(matches!(
            parse(&[
                "task-attribute",
                "--session",
                SESSION,
                "--task",
                SESSION,
                "--profile",
                "Codex interactive",
            ]),
            Ok(ObserveCliCommand::Attribution {
                profile: Some(_),
                failure: None,
                ..
            })
        ));
        assert!(matches!(
            parse(&[
                "task-unattribute",
                "--session",
                SESSION,
                "--task",
                SESSION,
                "--reason",
                "configuration-changed",
            ]),
            Ok(ObserveCliCommand::Attribution {
                profile: None,
                failure: Some(AttributionFailure::ConfigurationChanged),
                ..
            })
        ));
    }

    #[test]
    fn rejects_unknown_duplicate_missing_and_trailing_arguments() {
        for args in [
            vec!["unknown"],
            vec!["session-start", "--cli", "claude", "--wat"],
            vec!["session-start", "--cli", "claude", "--cli", "codex"],
            vec!["session-start", "--cli"],
            vec!["session-start", "--cli", "--session", SESSION],
            vec!["session-start", "--cli", "claude", "trailing"],
            vec!["session-start"],
            vec!["session-end", "--cli", "claude"],
            vec!["task-complete", "--session", SESSION, "--profile"],
            vec!["hook", "claude", "trailing"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn rejects_invalid_values_and_non_observable_clis() {
        for args in [
            vec!["session-start", "--cli", "gemini"],
            vec!["session-start", "--cli", "test"],
            vec!["session-start", "--cli", "claude", "--session", "bad"],
            vec![
                "task-start",
                "--session",
                SESSION,
                "--profile",
                "profile",
                "--prompt-chars",
                "-1",
            ],
            vec!["task-start", "--session", SESSION, "--profile", ""],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_values_without_leaking_raw_or_control_input() {
        use std::os::unix::ffi::OsStringExt;

        let error = parse_observe_args(
            [
                OsString::from("session-start"),
                OsString::from("--external-id"),
                OsString::from_vec(vec![b's', 0xff, b'\n']),
                OsString::from("--cli"),
                OsString::from("claude"),
            ]
            .into_iter(),
        )
        .unwrap_err();
        assert!(!error.as_bytes().contains(&0xff));

        let error = parse(&["bad\ncommand\u{1b}[31m"]).unwrap_err();
        assert!(!error.contains("bad\ncommand"));
        assert!(!error.contains('\u{1b}'));
        assert!(error.contains("Usage:"));
    }
}

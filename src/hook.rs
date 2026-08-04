use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum hook payload accepted from a CLI, in bytes.
pub const MAX_HOOK_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

/// A normalized, secret-minimized observation received from a CLI hook.
///
/// This type deliberately contains no transcript, prompt, assistant message,
/// tool input/output, or provider-specific diagnostic detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookObservation {
    pub provider: Provider,
    pub external_session_id: String,
    pub cwd: String,
    pub event: HookEvent,
}

/// Hook events that are useful for observing a direct CLI session.
///
/// In particular, `Stop` and `StopFailure` describe the end of a model turn;
/// they do not carry a task completion or interruption status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart {
        source: Option<String>,
        model: Option<String>,
        permission_mode: Option<String>,
    },
    SessionEnd {
        reason: Option<String>,
    },
    CwdChanged {
        old_cwd: String,
        new_cwd: String,
    },
    StopFailure {
        error: Option<String>,
    },
    Stop {
        model: Option<String>,
        permission_mode: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum HookParseError {
    #[error("hook payload exceeds the input limit")]
    InputTooLarge,
    #[error("unsupported hook event")]
    UnsupportedEvent,
    #[error("invalid hook payload")]
    InvalidPayload,
}

#[derive(Deserialize)]
struct EventDiscriminator {
    hook_event_name: String,
}

#[derive(Deserialize)]
struct CommonInput {
    session_id: String,
    cwd: String,
}

#[derive(Deserialize)]
struct SessionStartInput {
    #[serde(flatten)]
    common: CommonInput,
    source: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
}

#[derive(Deserialize)]
struct SessionEndInput {
    #[serde(flatten)]
    common: CommonInput,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct CwdChangedInput {
    #[serde(flatten)]
    common: CommonInput,
    old_cwd: String,
    new_cwd: String,
}

#[derive(Deserialize)]
struct StopFailureInput {
    #[serde(flatten)]
    common: CommonInput,
    error: Option<String>,
}

#[derive(Deserialize)]
struct StopInput {
    #[serde(flatten)]
    common: CommonInput,
    model: Option<String>,
    permission_mode: Option<String>,
}

/// Parses and normalizes one hook JSON object from stdin-compatible bytes.
///
/// Unknown JSON fields are ignored without being retained. Parse errors are
/// intentionally collapsed to fixed variants so malformed input and secret
/// values can never appear in an error or its `Debug` representation.
pub fn parse_hook_observation(
    provider: Provider,
    input: &[u8],
) -> Result<HookObservation, HookParseError> {
    if input.len() > MAX_HOOK_INPUT_BYTES {
        return Err(HookParseError::InputTooLarge);
    }

    let discriminator: EventDiscriminator =
        serde_json::from_slice(input).map_err(|_| HookParseError::InvalidPayload)?;

    match (provider, discriminator.hook_event_name.as_str()) {
        (Provider::Claude | Provider::Codex, "SessionStart") => {
            let input: SessionStartInput = parse_payload(input)?;
            Ok(observation(
                provider,
                input.common,
                HookEvent::SessionStart {
                    source: known(
                        input.source,
                        match provider {
                            Provider::Claude => &["startup", "resume", "clear", "compact", "fork"],
                            Provider::Codex => &["startup", "resume", "clear", "compact"],
                        },
                    ),
                    model: input.model,
                    permission_mode: known(
                        input.permission_mode,
                        &[
                            "default",
                            "acceptEdits",
                            "plan",
                            "dontAsk",
                            "bypassPermissions",
                        ],
                    ),
                },
            ))
        }
        (Provider::Claude | Provider::Codex, "SessionEnd") => {
            let input: SessionEndInput = parse_payload(input)?;
            Ok(observation(
                provider,
                input.common,
                HookEvent::SessionEnd {
                    reason: known(
                        input.reason,
                        &[
                            "clear",
                            "resume",
                            "logout",
                            "prompt_input_exit",
                            "bypass_permissions_disabled",
                            "other",
                        ],
                    ),
                },
            ))
        }
        (Provider::Claude, "CwdChanged") => {
            let input: CwdChangedInput = parse_payload(input)?;
            Ok(observation(
                provider,
                input.common,
                HookEvent::CwdChanged {
                    old_cwd: input.old_cwd,
                    new_cwd: input.new_cwd,
                },
            ))
        }
        (Provider::Claude, "StopFailure") => {
            let input: StopFailureInput = parse_payload(input)?;
            Ok(observation(
                provider,
                input.common,
                HookEvent::StopFailure {
                    error: known(
                        input.error,
                        &[
                            "rate_limit",
                            "overloaded",
                            "authentication_failed",
                            "oauth_org_not_allowed",
                            "billing_error",
                            "invalid_request",
                            "model_not_found",
                            "server_error",
                            "max_output_tokens",
                            "unknown",
                        ],
                    ),
                },
            ))
        }
        (Provider::Codex, "Stop") => {
            let input: StopInput = parse_payload(input)?;
            Ok(observation(
                provider,
                input.common,
                HookEvent::Stop {
                    model: input.model,
                    permission_mode: known(
                        input.permission_mode,
                        &[
                            "default",
                            "acceptEdits",
                            "plan",
                            "dontAsk",
                            "bypassPermissions",
                        ],
                    ),
                },
            ))
        }
        _ => Err(HookParseError::UnsupportedEvent),
    }
}

fn known(value: Option<String>, allowed: &[&str]) -> Option<String> {
    value.filter(|value| allowed.contains(&value.as_str()))
}

fn parse_payload<T: for<'de> Deserialize<'de>>(input: &[u8]) -> Result<T, HookParseError> {
    serde_json::from_slice(input).map_err(|_| HookParseError::InvalidPayload)
}

fn observation(provider: Provider, common: CommonInput, event: HookEvent) -> HookObservation {
    HookObservation {
        provider,
        external_session_id: common.session_id,
        cwd: common.cwd,
        event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(provider: Provider, fixture: &str) -> HookObservation {
        parse_hook_observation(provider, fixture.as_bytes()).expect("fixture should parse")
    }

    #[test]
    fn parses_claude_session_start_fixture() {
        let observation = parse(
            Provider::Claude,
            r#"{
                "session_id":"abc123",
                "transcript_path":"/Users/example/.claude/transcript.jsonl",
                "cwd":"/Users/example/project",
                "permission_mode":"default",
                "hook_event_name":"SessionStart",
                "source":"startup",
                "model":"claude-sonnet-4-6"
            }"#,
        );

        assert_eq!(
            observation,
            HookObservation {
                provider: Provider::Claude,
                external_session_id: "abc123".into(),
                cwd: "/Users/example/project".into(),
                event: HookEvent::SessionStart {
                    source: Some("startup".into()),
                    model: Some("claude-sonnet-4-6".into()),
                    permission_mode: Some("default".into()),
                },
            }
        );
    }

    #[test]
    fn parses_claude_session_end_fixture() {
        let observation = parse(
            Provider::Claude,
            r#"{
                "session_id":"abc123",
                "transcript_path":"/Users/example/.claude/transcript.jsonl",
                "cwd":"/Users/example/project",
                "hook_event_name":"SessionEnd",
                "reason":"other"
            }"#,
        );

        assert_eq!(
            observation.event,
            HookEvent::SessionEnd {
                reason: Some("other".into())
            }
        );
    }

    #[test]
    fn parses_claude_cwd_changed_fixture() {
        let observation = parse(
            Provider::Claude,
            r#"{
                "session_id":"abc123",
                "transcript_path":"/Users/example/.claude/transcript.jsonl",
                "cwd":"/Users/example/project/src",
                "hook_event_name":"CwdChanged",
                "old_cwd":"/Users/example/project",
                "new_cwd":"/Users/example/project/src"
            }"#,
        );

        assert_eq!(
            observation.event,
            HookEvent::CwdChanged {
                old_cwd: "/Users/example/project".into(),
                new_cwd: "/Users/example/project/src".into(),
            }
        );
    }

    #[test]
    fn parses_claude_stop_failure_fixture_without_diagnostics() {
        let observation = parse(
            Provider::Claude,
            r#"{
                "session_id":"abc123",
                "transcript_path":"/Users/example/.claude/transcript.jsonl",
                "cwd":"/Users/example/project",
                "hook_event_name":"StopFailure",
                "error":"rate_limit",
                "error_details":"429 Too Many Requests",
                "last_assistant_message":"API Error: Rate limit reached"
            }"#,
        );

        assert_eq!(
            observation.event,
            HookEvent::StopFailure {
                error: Some("rate_limit".into())
            }
        );
    }

    #[test]
    fn parses_codex_session_start_fixture() {
        let observation = parse(
            Provider::Codex,
            r#"{
                "session_id":"thr_123",
                "transcript_path":"/workspace/.codex/rollout.jsonl",
                "cwd":"/workspace",
                "hook_event_name":"SessionStart",
                "model":"gpt-5.6-codex",
                "permission_mode":"default",
                "source":"resume"
            }"#,
        );

        assert_eq!(
            observation.event,
            HookEvent::SessionStart {
                source: Some("resume".into()),
                model: Some("gpt-5.6-codex".into()),
                permission_mode: Some("default".into()),
            }
        );
    }

    #[test]
    fn parses_codex_session_end_fixture() {
        let observation = parse(
            Provider::Codex,
            r#"{
                "session_id":"thr_123",
                "transcript_path":"/workspace/.codex/rollout.jsonl",
                "cwd":"/workspace",
                "hook_event_name":"SessionEnd",
                "reason":"other"
            }"#,
        );

        assert_eq!(
            observation.event,
            HookEvent::SessionEnd {
                reason: Some("other".into())
            }
        );
    }

    #[test]
    fn parses_codex_stop_fixture_as_turn_observation() {
        let observation = parse(
            Provider::Codex,
            r#"{
                "session_id":"thr_123",
                "transcript_path":"/workspace/.codex/rollout.jsonl",
                "cwd":"/workspace",
                "hook_event_name":"Stop",
                "model":"gpt-5.6-codex",
                "permission_mode":"acceptEdits",
                "turn_id":"turn_456",
                "stop_hook_active":false,
                "last_assistant_message":"Finished the requested change"
            }"#,
        );

        // The shape intentionally has configuration only: no task-end status.
        let HookEvent::Stop {
            model,
            permission_mode,
        } = observation.event
        else {
            panic!("expected Stop");
        };
        assert_eq!(model.as_deref(), Some("gpt-5.6-codex"));
        assert_eq!(permission_mode.as_deref(), Some("acceptEdits"));
    }

    #[test]
    fn normalized_output_and_debug_omit_secret_fields() {
        const SECRET: &str = "SENTINEL_TOP_SECRET_42";
        let fixture = format!(
            r#"{{
                "session_id":"safe-session",
                "cwd":"/safe/cwd",
                "hook_event_name":"Stop",
                "model":"safe-model",
                "permission_mode":"default",
                "transcript_path":"/{SECRET}",
                "prompt":"{SECRET}",
                "assistant":"{SECRET}",
                "last_assistant_message":"{SECRET}",
                "tool":"{SECRET}",
                "tool_input":{{"command":"{SECRET}"}},
                "tool_output":"{SECRET}",
                "error_details":"{SECRET}",
                "unknown_field":"{SECRET}"
            }}"#
        );

        let observation = parse(Provider::Codex, &fixture);
        let normalized = serde_json::to_string(&observation).expect("observation serializes");
        let debug = format!("{observation:?}");

        assert!(!normalized.contains(SECRET));
        assert!(!debug.contains(SECRET));
        assert!(!normalized.contains("transcript_path"));
        assert!(!normalized.contains("last_assistant_message"));
        assert!(!normalized.contains("tool_input"));
        assert!(!normalized.contains("error_details"));
    }

    #[test]
    fn unknown_and_provider_mismatched_events_are_content_free_errors() {
        const SECRET: &str = "SENTINEL_UNKNOWN_EVENT_SECRET";
        let unknown = format!(r#"{{"hook_event_name":"{SECRET}"}}"#);
        let mismatch = format!(
            r#"{{"session_id":"s","cwd":"/w","hook_event_name":"Stop","prompt":"{SECRET}"}}"#
        );

        for error in [
            parse_hook_observation(Provider::Claude, unknown.as_bytes()).unwrap_err(),
            parse_hook_observation(Provider::Claude, mismatch.as_bytes()).unwrap_err(),
        ] {
            assert_eq!(error, HookParseError::UnsupportedEvent);
            assert!(!error.to_string().contains(SECRET));
            assert!(!format!("{error:?}").contains(SECRET));
        }
    }

    #[test]
    fn malformed_payload_is_a_content_free_error() {
        const SECRET: &str = "SENTINEL_MALFORMED_SECRET";
        let malformed =
            format!(r#"{{"session_id":42,"cwd":"/{SECRET}","hook_event_name":"SessionStart"}}"#);

        let error = parse_hook_observation(Provider::Claude, malformed.as_bytes()).unwrap_err();

        assert_eq!(error, HookParseError::InvalidPayload);
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn rejects_payload_over_one_mib_without_parsing_it() {
        let input = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];

        assert_eq!(
            parse_hook_observation(Provider::Codex, &input),
            Err(HookParseError::InputTooLarge)
        );
    }

    #[test]
    fn accepts_payload_at_one_mib() {
        let prefix =
            b"{\"session_id\":\"s\",\"cwd\":\"/w\",\"hook_event_name\":\"Stop\",\"padding\":\"";
        let suffix = br#""}"#;
        let padding_len = MAX_HOOK_INPUT_BYTES - prefix.len() - suffix.len();
        let mut input = Vec::with_capacity(MAX_HOOK_INPUT_BYTES);
        input.extend_from_slice(prefix);
        input.extend(std::iter::repeat_n(b'x', padding_len));
        input.extend_from_slice(suffix);

        assert_eq!(input.len(), MAX_HOOK_INPUT_BYTES);
        assert!(parse_hook_observation(Provider::Codex, &input).is_ok());
    }
}

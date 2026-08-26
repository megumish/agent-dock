use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

use agent_dock::{
    Advice, AdviceEvidence, AdviceOutcome, AdviceReason, AdviceSegmentCount, AppendBatchOutcome,
    AppendJudgementOutcome, AttributionFailure, BackupEventLogOutcome, Config, ConfigError,
    EVENT_FORMAT_VERSION, EstimatedCostSummary, Event, EventKind, EventLog, EvidenceAcceptance,
    EvidenceCost, EvidenceDuration, EvidenceSegment, ExclusionReason, ExecutionError,
    ExecutionEvent, ExecutionOrigin, ExecutionOutcome, ExecutionPlatform, ExecutionProfile,
    ExecutionReport, ExecutionRequest, FailureKind, HookEvent, JudgementCandidate,
    MAX_HOOK_INPUT_BYTES, ObservationCommand, ObserveCliCommand, OutputSource, ProfileAttribution,
    ProfileDeclaration, ProfileSnapshot, Projection, Provenance, ReadEvents,
    RecordedExecutionOutcome, ReplaceConfigOutcome, Segment, SegmentScore, SessionPhase,
    SkippedLineReason, Verdict, advise, apply_observation, configuration_for_profile,
    default_config_path, default_events_path, escape_terminal, execute, format_acceptance,
    format_duration, format_recency, judgement_candidates, parse_hook_observation,
    parse_observe_args, project, resolve_observed_session, safe_test_profile, segments_for_tags,
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
        Err(AppError::IncompatibleEventLogDeclined) => ExitCode::SUCCESS,
        Err(error) if error.is_interrupted() => {
            eprintln!("agent-dock: cancelled");
            ExitCode::from(130)
        }
        Err(AppError::Usage(message)) => {
            eprintln!("agent-dock: {message}");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("agent-dock: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<i32, AppError> {
    match command_from_args(std::env::args_os().skip(1))? {
        AppCommand::Run => run_task().await,
        AppCommand::Judge => judge(),
        AppCommand::Scorecard => scorecard(),
        AppCommand::Observe(command) => observe(command),
        AppCommand::Help => {
            print_usage();
            Ok(0)
        }
    }
}

async fn run_task() -> Result<i32, AppError> {
    let config_path = default_config_path()?;
    let Some(config) = load_or_create_config(&config_path)? else {
        return Ok(0);
    };

    let mut available = vec![safe_test_profile()];
    for declaration in &config.profiles {
        if declaration.execution_platform != ExecutionPlatform::Headless {
            continue;
        }
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
    let mut projection_profiles = available.clone();
    projection_profiles.extend(
        config
            .profiles
            .iter()
            .filter(|profile| profile.execution_platform == ExecutionPlatform::Interactive)
            .map(|profile| {
                profile
                    .resolve(profile.executable())
                    .expect("validated configuration")
            }),
    );

    let working_directory = std::env::current_dir()?;
    let event_log = EventLog::new(default_events_path()?);
    let history = read_event_history(&event_log)?;
    for skipped in &history.skipped_lines {
        eprintln!(
            "Warning: skipped event log line {}: {}",
            skipped.line_number,
            escape_terminal(&skipped.reason.to_string())
        );
    }
    let tag_candidates = collect_tag_candidates(&config.tag_candidates, &history.events);
    let mut prompt_editor = DefaultEditor::new()?;
    let mut tag_editor = DefaultEditor::new()?;
    let prompt = read_non_empty_prompt(&mut prompt_editor, None)?;
    let tags = read_tags(&mut tag_editor, &tag_candidates, None)?;
    let (prompt, (task_id, selected)) = resolve_task_with_editable_tags(
        prompt,
        tags,
        |initial| {
            read_tags(&mut tag_editor, &tag_candidates, Some(initial)).map_err(AppError::from)
        },
        |prompt, tags| {
            let prompt_approved = if config.record_prompt {
                confirm("Record this task prompt?", true)?
            } else {
                false
            };
            let resolution = resolve_advised_task_with(&event_log, |fresh_history| {
                let (all_segments, projection, advice) = prepare_advice_for_task(
                    &fresh_history.events,
                    tags,
                    &projection_profiles,
                    &available,
                );
                let hidden_tags = all_segments.len() - visible_segments(&all_segments).len();
                for warning in &projection.warnings {
                    eprintln!("Warning: {}", escape_terminal(warning));
                }
                for exclusion in &projection.exclusions {
                    eprintln!(
                        "Direct observation excluded ({}): {}",
                        exclusion_reason_label(exclusion.reason),
                        exclusion.count
                    );
                }
                let now = Timestamp::now();
                print_advice(&advice, &available, now);
                let choices =
                    render_profile_choices(&available, &projection.scorecards, hidden_tags, now);
                let default_index = advice_default_index(&advice, &available);
                let selected = choose_profile(&choices, default_index)?;
                let profile = &available[selected];
                print_execution_summary(profile, prompt, &working_directory);
                let confirmation = confirm_run()?;
                if let Some((warning, transition)) = run_confirmation_notice(confirmation) {
                    eprintln!("Warning: {warning}");
                    println!("{transition}");
                }
                resolve_run_confirmation(
                    prompt,
                    confirmation,
                    &mut |initial| {
                        read_non_empty_prompt(&mut prompt_editor, Some(initial))
                            .map_err(AppError::from)
                    },
                    || {
                        let task_id = Uuid::now_v7();
                        let received = task_received_event(
                            task_id,
                            tags.to_vec(),
                            prompt,
                            config.record_prompt,
                            prompt_approved,
                            Timestamp::now(),
                        );
                        Ok((received, advice, profile.id.clone(), (task_id, selected)))
                    },
                )
            })?;
            Ok(resolution)
        },
    )?;
    let profile = &available[selected];

    let (cancellation_sender, cancellation_receiver) = oneshot::channel();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancellation_sender.send(());
        }
    });
    let execution = execute_and_record(
        &event_log,
        profile,
        task_id,
        prompt,
        working_directory,
        cancellation_receiver,
        print_event,
    )
    .await;
    signal_task.abort();

    let Some(outcome) = execution? else {
        return Ok(1);
    };
    let process_exit_code = outcome.process_exit_code();

    if !should_ask_immediate_judgement(&outcome) {
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
            verdict: if accepted {
                Verdict::Accepted
            } else {
                Verdict::Rejected
            },
        },
    ))?;
    Ok(process_exit_code)
}

async fn execute_and_record(
    event_log: &EventLog,
    profile: &ExecutionProfile,
    task_id: Uuid,
    prompt: String,
    working_directory: PathBuf,
    cancellation: oneshot::Receiver<()>,
    mut on_event: impl FnMut(ExecutionEvent),
) -> Result<Option<ExecutionOutcome>, AppError> {
    let started_at = Timestamp::now();
    let attempt_started = Instant::now();
    let mut report = ExecutionReport::default();
    let execution = execute(
        profile,
        ExecutionRequest {
            prompt,
            working_directory: working_directory.clone(),
        },
        cancellation,
        |event| {
            if let ExecutionEvent::Report { report: parsed } = &event {
                report = parsed.clone();
            }
            on_event(event);
        },
    )
    .await;

    let outcome = match execution {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = error.to_string();
            eprintln!("Execution failed: {message}");
            let kind = executed_event_kind(
                ProfileSnapshot::from(profile),
                working_directory,
                started_at,
                elapsed_millis(attempt_started.elapsed()),
                RecordedExecutionOutcome::Failed {
                    failure_kind: failure_kind(&error),
                    message,
                },
                report,
            );
            event_log.append(&Event::new(task_id, kind))?;
            return Ok(None);
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
    let (recorded_outcome, elapsed_ms) = recorded_execution_outcome(&outcome);
    let executed = Event::new(
        task_id,
        executed_event_kind(
            ProfileSnapshot::from(profile),
            working_directory,
            started_at,
            elapsed_ms,
            recorded_outcome,
            report,
        ),
    );
    event_log.append(&executed)?;
    Ok(Some(outcome))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppCommand {
    Run,
    Judge,
    Scorecard,
    Observe(ObserveCliCommand),
    Help,
}

fn command_from_args(
    mut arguments: impl Iterator<Item = OsString>,
) -> Result<AppCommand, AppError> {
    let command = match arguments.next().as_deref() {
        None => AppCommand::Run,
        Some(command) if command == "judge" => AppCommand::Judge,
        Some(command) if command == "scorecard" => AppCommand::Scorecard,
        Some(command) if command == "observe" => {
            return parse_observe_args(arguments)
                .map(AppCommand::Observe)
                .map_err(AppError::Usage);
        }
        Some(command) if command == "-h" || command == "--help" => AppCommand::Help,
        Some(command) => {
            return Err(AppError::Usage(format!(
                "unknown command `{}`\nUsage: agent-dock [judge|scorecard|observe]",
                escape_terminal(&command.to_string_lossy())
            )));
        }
    };
    if arguments.next().is_some() {
        return Err(AppError::Usage(
            "too many arguments\nUsage: agent-dock [judge|scorecard|observe]".to_owned(),
        ));
    }
    Ok(command)
}

fn print_usage() {
    println!("Usage: agent-dock [judge|scorecard|observe]");
    println!("\nCommands:");
    println!("  judge  Append a new judgement to a completed or cancelled execution");
    println!("  scorecard  Show brokered and direct evidence for every profile");
    println!("  observe  Record a direct Claude Code or Codex CLI session");
}

fn observe(command: ObserveCliCommand) -> Result<i32, AppError> {
    let event_log = EventLog::new(default_events_path()?);
    match command {
        ObserveCliCommand::Session {
            phase,
            session_id,
            cli,
            external_session_id,
            working_directory,
        } => {
            let outcome = apply_observation(
                &event_log,
                &[],
                ObservationCommand::ObserveSession {
                    session_id,
                    cli,
                    phase,
                    external_session_id,
                    working_directory,
                    configuration: None,
                    provenance: Provenance::ManualMark,
                },
            )?;
            println!("{}", outcome.session_id);
        }
        ObserveCliCommand::SessionFind {
            cli,
            external_session_id,
        } => {
            let history = event_log.read_all()?;
            let session_id = resolve_observed_session(&history, cli, &external_session_id)?
                .ok_or_else(|| AppError::Usage("observed session was not found".to_owned()))?;
            println!("{session_id}");
        }
        ObserveCliCommand::TaskStart {
            session_id,
            profile,
            tags,
            prompt_chars,
        } => {
            let config = load_observation_config()?;
            let configuration = profile
                .as_deref()
                .map(|profile| interactive_profile(&config, profile))
                .transpose()?
                .map(configuration_for_profile);
            let outcome = apply_observation(
                &event_log,
                &config.profiles,
                ObservationCommand::StartTask {
                    session_id,
                    tags,
                    prompt: None,
                    prompt_chars,
                    configuration,
                },
            )?;
            println!("{}", outcome.task_id.expect("task start returns a task id"));
        }
        ObserveCliCommand::TaskEnd {
            session_id,
            profile,
            status,
        } => {
            let config = load_observation_config()?;
            let configuration = profile
                .as_deref()
                .map(|profile| interactive_profile(&config, profile))
                .transpose()?
                .map(configuration_for_profile);
            let outcome = apply_observation(
                &event_log,
                &config.profiles,
                ObservationCommand::EndTask {
                    session_id,
                    status,
                    configuration,
                },
            )?;
            if outcome.anomaly_recorded {
                return Err(AppError::OrphanTaskEnd(session_id));
            }
            println!("{}", outcome.task_id.expect("task end returns a task id"));
        }
        ObserveCliCommand::Attribution {
            session_id,
            task_id,
            profile,
            failure,
        } => {
            let config = load_observation_config()?;
            let attribution = if let Some(profile) = profile {
                let profile = interactive_profile(&config, &profile)?;
                ProfileAttribution::Matched {
                    profile: ProfileSnapshot::observed(profile)
                        .map_err(|error| AppError::Usage(error.to_string()))?,
                }
            } else {
                ProfileAttribution::Unattributed {
                    reason: failure.unwrap_or(AttributionFailure::MissingConfiguration),
                }
            };
            let outcome = apply_observation(
                &event_log,
                &config.profiles,
                ObservationCommand::CorrectAttribution {
                    session_id,
                    task_id,
                    attribution,
                },
            )?;
            println!("{}", outcome.task_id.expect("correction returns a task id"));
        }
        ObserveCliCommand::Hook { provider } => {
            let mut payload = Vec::new();
            io::stdin()
                .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
                .read_to_end(&mut payload)?;
            let hook = parse_hook_observation(provider, &payload)?;
            let cli = match hook.provider {
                agent_dock::Provider::Claude => agent_dock::CliKind::Claude,
                agent_dock::Provider::Codex => agent_dock::CliKind::Codex,
            };
            let phase = match &hook.event {
                HookEvent::SessionEnd { .. } => SessionPhase::Ended,
                HookEvent::SessionStart { source, .. }
                    if matches!(source.as_deref(), Some("startup" | "fork")) =>
                {
                    SessionPhase::Started
                }
                HookEvent::SessionStart { .. } => SessionPhase::Resumed,
                _ => SessionPhase::Started,
            };
            let working_directory = match &hook.event {
                HookEvent::CwdChanged { new_cwd, .. } => PathBuf::from(new_cwd),
                _ => PathBuf::from(&hook.cwd),
            };
            let configuration = hook_configuration(cli, &hook.event);
            let outcome = apply_observation(
                &event_log,
                &[],
                ObservationCommand::ObserveSession {
                    session_id: None,
                    cli,
                    phase,
                    external_session_id: Some(hook.external_session_id),
                    working_directory: Some(working_directory),
                    configuration,
                    provenance: Provenance::CliHook,
                },
            )?;
            println!("{}", outcome.session_id);
        }
    }
    Ok(0)
}

fn hook_configuration(
    cli: agent_dock::CliKind,
    event: &HookEvent,
) -> Option<agent_dock::ObservedConfiguration> {
    let (model, permission_mode) = match event {
        HookEvent::SessionStart {
            model,
            permission_mode,
            ..
        }
        | HookEvent::Stop {
            model,
            permission_mode,
        } => (model, permission_mode),
        _ => return None,
    };
    let model = model
        .clone()
        .map(|model| agent_dock::ObservedValue::Observed(Some(model)))
        .unwrap_or(agent_dock::ObservedValue::Missing);
    let permission_mode = permission_mode
        .clone()
        .map(agent_dock::ObservedValue::Observed)
        .unwrap_or(agent_dock::ObservedValue::Missing);
    Some(agent_dock::ObservedConfiguration {
        cli,
        execution_platform: ExecutionPlatform::Interactive,
        model,
        identity: BTreeMap::from([("permission_mode".to_owned(), permission_mode)]),
    })
}

fn load_observation_config() -> Result<Config, AppError> {
    let path = default_config_path()?;
    if !path.exists() {
        return Err(AppError::Usage(format!(
            "configuration does not exist: {}",
            path.display()
        )));
    }
    Ok(Config::load(&path)?)
}

fn interactive_profile<'a>(
    config: &'a Config,
    name: &str,
) -> Result<&'a ProfileDeclaration, AppError> {
    config
        .profiles
        .iter()
        .find(|profile| {
            profile.name == name && profile.execution_platform == ExecutionPlatform::Interactive
        })
        .ok_or_else(|| {
            AppError::Usage(format!(
                "unknown interactive profile `{}`",
                escape_terminal(name)
            ))
        })
}

fn scorecard() -> Result<i32, AppError> {
    let config = load_observation_config()?;
    let profiles: Vec<_> = config
        .profiles
        .iter()
        .map(|profile| {
            profile
                .resolve(profile.executable())
                .expect("validated configuration")
        })
        .collect();
    let event_log = EventLog::new(default_events_path()?);
    let history = read_event_history_without_migration(&event_log)?;
    print_skipped_event_warnings(&history);
    let tags = collect_tag_candidates(&config.tag_candidates, &history.events);
    let all_segments = segments_for_tags(&tags);
    let hidden_tags = all_segments.len() - visible_segments(&all_segments).len();
    let projection = project(&history.events, &profiles, &all_segments);
    for warning in &projection.warnings {
        eprintln!("Warning: {}", escape_terminal(warning));
    }
    for exclusion in &projection.exclusions {
        eprintln!(
            "Direct observation excluded ({}): {}",
            exclusion_reason_label(exclusion.reason),
            exclusion.count
        );
    }
    let rendered = render_profile_choices(
        &profiles,
        &projection.scorecards,
        hidden_tags,
        Timestamp::now(),
    );
    for card in rendered {
        println!("{card}");
    }
    Ok(0)
}

fn judge() -> Result<i32, AppError> {
    let event_log = EventLog::new(default_events_path()?);
    let history = read_event_history_without_migration(&event_log)?;
    print_skipped_event_warnings(&history);
    let resolution = resolve_judgement_with(
        &event_log,
        |candidates| {
            println!("Choose an execution:");
            for (index, candidate) in candidates.iter().enumerate() {
                println!("  {}. {}", index + 1, render_judgement_candidate(candidate));
            }
            choose_judgement_candidate(candidates.len()).map_err(AppError::from)
        },
        || read_verdict().map_err(AppError::from),
        |candidate, verdict| {
            println!("Selected: {}", render_judgement_candidate(candidate));
            confirm(
                &format!(
                    "Append {} judgement to execution {}?",
                    verdict_label(verdict),
                    candidate.task_id
                ),
                false,
            )
            .map_err(AppError::from)
        },
        |previous, current| {
            eprintln!(
                "The current judgement changed from {} to {}; please confirm again.",
                optional_verdict_label(previous),
                optional_verdict_label(current)
            );
        },
    )?;
    match resolution {
        JudgementResolution::NoCandidates => {
            println!("No completed or cancelled executions are available to judge.");
        }
        JudgementResolution::Quit => {}
        JudgementResolution::Declined => println!("No judgement was recorded."),
        JudgementResolution::Appended { task_id, verdict } => {
            println!("Recorded {} for task {}.", verdict_label(verdict), task_id)
        }
    }
    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgementResolution {
    NoCandidates,
    Quit,
    Declined,
    Appended { task_id: Uuid, verdict: Verdict },
}

fn resolve_judgement_with(
    event_log: &EventLog,
    mut choose: impl FnMut(&[JudgementCandidate]) -> Result<Option<usize>, AppError>,
    mut read_new_verdict: impl FnMut() -> Result<Option<Verdict>, AppError>,
    mut confirm_append: impl FnMut(&JudgementCandidate, Verdict) -> Result<bool, AppError>,
    mut judgement_changed: impl FnMut(Option<Verdict>, Option<Verdict>),
) -> Result<JudgementResolution, AppError> {
    let history = read_event_history_without_migration(event_log)?;
    let candidates = judgement_candidates(&history.events);
    if candidates.is_empty() {
        return Ok(JudgementResolution::NoCandidates);
    }
    let Some(selected) = choose(&candidates)? else {
        return Ok(JudgementResolution::Quit);
    };
    let task_id = candidates
        .get(selected)
        .ok_or_else(|| AppError::Usage("selected execution is out of range".to_owned()))?
        .task_id;
    let Some(verdict) = read_new_verdict()? else {
        return Ok(JudgementResolution::Quit);
    };

    loop {
        let latest = read_event_history_without_migration(event_log)?;
        let Some(candidate) = judgement_candidates(&latest.events)
            .into_iter()
            .find(|candidate| candidate.task_id == task_id)
        else {
            return Err(AppError::ExecutionNoLongerAvailable(task_id));
        };
        if !confirm_append(&candidate, verdict)? {
            return Ok(JudgementResolution::Declined);
        }
        match event_log.append_judgement_if_current(task_id, candidate.current_verdict, verdict)? {
            AppendJudgementOutcome::Appended => {
                return Ok(JudgementResolution::Appended { task_id, verdict });
            }
            AppendJudgementOutcome::CurrentVerdictChanged { current } => {
                judgement_changed(candidate.current_verdict, current);
            }
            AppendJudgementOutcome::TaskMissing => {
                return Err(AppError::ExecutionNoLongerAvailable(task_id));
            }
        }
    }
}

fn print_skipped_event_warnings(history: &ReadEvents) {
    for skipped in &history.skipped_lines {
        eprintln!(
            "Warning: skipped event log line {}: {}",
            skipped.line_number,
            escape_terminal(&skipped.reason.to_string())
        );
    }
}

fn render_judgement_candidate(candidate: &JudgementCandidate) -> String {
    let model = candidate
        .profile
        .model
        .as_deref()
        .map(escape_terminal)
        .unwrap_or_else(|| "default".to_owned());
    let tags = if candidate.tags.is_empty() {
        "untagged".to_owned()
    } else {
        candidate
            .tags
            .iter()
            .map(|tag| escape_terminal(tag))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "{} | {} | {} [{}; {}; {}] | {} | tags: {} | current: {}",
        candidate.target_event_id,
        candidate.started_at,
        escape_terminal(&candidate.profile.name),
        candidate.profile.cli,
        model,
        escape_terminal(&candidate.profile.id),
        execution_outcome_label(&candidate.outcome),
        tags,
        optional_verdict_label(candidate.current_verdict),
    )
}

fn execution_outcome_label(outcome: &RecordedExecutionOutcome) -> String {
    match outcome {
        RecordedExecutionOutcome::Completed {
            exit_code: Some(code),
        } => {
            format!("completed with exit code {code}")
        }
        RecordedExecutionOutcome::Completed { exit_code: None } => {
            "completed with unknown exit code".to_owned()
        }
        RecordedExecutionOutcome::Cancelled => "cancelled".to_owned(),
        RecordedExecutionOutcome::Failed { failure_kind, .. } => {
            format!("failed ({failure_kind:?})")
        }
    }
}

fn choose_judgement_candidate(count: usize) -> io::Result<Option<usize>> {
    loop {
        let input = read_line("Selection (q to quit):")?;
        if input.trim().eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        if let Some(index) = parse_required_selection(&input, count) {
            return Ok(Some(index));
        }
        eprintln!("Enter a number from 1 to {count}, or q to quit.");
    }
}

fn parse_required_selection(input: &str, count: usize) -> Option<usize> {
    let selected = input.trim().parse::<usize>().ok()?;
    selected.checked_sub(1).filter(|index| *index < count)
}

fn read_verdict() -> io::Result<Option<Verdict>> {
    loop {
        let input = read_line("New judgement (accepted/rejected, q to quit):")?;
        if input.trim().eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        if let Some(verdict) = parse_verdict(&input) {
            return Ok(Some(verdict));
        }
        eprintln!("Enter accepted or rejected, or q to quit.");
    }
}

fn parse_verdict(input: &str) -> Option<Verdict> {
    match input.trim().to_ascii_lowercase().as_str() {
        "a" | "accepted" => Some(Verdict::Accepted),
        "r" | "rejected" => Some(Verdict::Rejected),
        _ => None,
    }
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Accepted => "accepted",
        Verdict::Rejected => "rejected",
    }
}

fn optional_verdict_label(verdict: Option<Verdict>) -> &'static str {
    verdict.map_or("unjudged", verdict_label)
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

fn read_event_history(event_log: &EventLog) -> Result<ReadEvents, AppError> {
    read_event_history_with(event_log, &mut confirm)
}

fn read_event_history_without_migration(event_log: &EventLog) -> Result<ReadEvents, AppError> {
    let history = event_log.read_all()?;
    if history
        .skipped_lines
        .iter()
        .any(|skipped| matches!(skipped.reason, SkippedLineReason::UnsupportedFormat { .. }))
    {
        return Err(AppError::IncompatibleEventLog);
    }
    Ok(history)
}

fn read_event_history_with(
    event_log: &EventLog,
    ask: &mut impl FnMut(&str, bool) -> io::Result<bool>,
) -> Result<ReadEvents, AppError> {
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
        return Ok(history);
    }

    eprintln!(
        "Event log `{}` contains {incompatible_count} event(s) using format(s) {}; expected {EVENT_FORMAT_VERSION}.",
        event_log.path().display(),
        incompatible_formats.join(", ")
    );
    if !ask("Back up and clear the incompatible event log?", false)? {
        eprintln!("Event log was not changed.");
        return Err(AppError::IncompatibleEventLogDeclined);
    }
    match event_log.backup_and_delete_if_unchanged(&history)? {
        BackupEventLogOutcome::BackedUpAndDeleted { backup_path } => {
            eprintln!(
                "Warning: backed up incompatible event log to `{}` and cleared `{}`.",
                backup_path.display(),
                event_log.path().display()
            );
            Ok(ReadEvents::default())
        }
        BackupEventLogOutcome::Changed => {
            eprintln!("Event log changed while waiting for confirmation; it was not changed.");
            Err(AppError::IncompatibleEventLogDeclined)
        }
    }
}

fn read_non_empty_prompt(
    editor: &mut DefaultEditor,
    initial: Option<&str>,
) -> Result<String, ReadlineError> {
    loop {
        let prompt = if let Some(initial) = initial {
            editor.readline_with_initial("Task: ", (initial, ""))?
        } else {
            editor.readline("Task: ")?
        };
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
    Edit,
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
        RunConfirmation::Edit
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunResolution<T> {
    Edit(String),
    Run(T),
}

fn resolve_task_with_editable_tags<T>(
    mut prompt: String,
    mut tags: Vec<String>,
    mut edit_tags: impl FnMut(&[String]) -> Result<Vec<String>, AppError>,
    mut attempt: impl FnMut(&str, &[String]) -> Result<RunResolution<T>, AppError>,
) -> Result<(String, T), AppError> {
    loop {
        match attempt(&prompt, &tags)? {
            RunResolution::Edit(edited) => {
                prompt = edited;
                tags = edit_tags(&tags)?;
            }
            RunResolution::Run(approved) => return Ok((prompt, approved)),
        }
    }
}

fn resolve_advised_task_with<T>(
    event_log: &EventLog,
    mut prepare: impl FnMut(&ReadEvents) -> Result<RunResolution<(Event, Advice, String, T)>, AppError>,
) -> Result<RunResolution<T>, AppError> {
    loop {
        let history = read_event_history(event_log)?;
        match prepare(&history)? {
            RunResolution::Edit(prompt) => return Ok(RunResolution::Edit(prompt)),
            RunResolution::Run((received, advice, selected_profile_id, value)) => {
                match record_advised_task(
                    event_log,
                    &history,
                    received,
                    &advice,
                    &selected_profile_id,
                )? {
                    AppendBatchOutcome::Appended => return Ok(RunResolution::Run(value)),
                    AppendBatchOutcome::Changed => {
                        eprintln!(
                            "Event log changed while waiting for confirmation; recalculating advice."
                        );
                    }
                }
            }
        }
    }
}

fn run_confirmation_notice(confirmation: RunConfirmation) -> Option<(&'static str, &'static str)> {
    (confirmation == RunConfirmation::Edit)
        .then_some(("Task was not run.", "Returning to task definition..."))
}

fn resolve_run_confirmation<T>(
    prompt: &str,
    confirmation: RunConfirmation,
    edit: &mut impl FnMut(&str) -> Result<String, AppError>,
    approve: impl FnOnce() -> Result<T, AppError>,
) -> Result<RunResolution<T>, AppError> {
    match confirmation {
        RunConfirmation::Run => approve().map(RunResolution::Run),
        RunConfirmation::Edit => edit(prompt).map(RunResolution::Edit),
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

fn choose_profile(labels: &[String], default_index: usize) -> io::Result<usize> {
    println!("Choose a crew member:");
    for (index, label) in labels.iter().enumerate() {
        println!("  {}. {label}", index + 1);
    }

    loop {
        let default_label = default_index
            .checked_add(1)
            .filter(|index| *index <= labels.len())
            .unwrap_or(1);
        let input = read_line(&format!("Selection [{default_label}]:"))?;
        if let Some(index) = parse_selection(&input, labels.len(), default_index) {
            return Ok(index);
        }
        eprintln!("Enter a number from 1 to {}.", labels.len());
    }
}

fn parse_selection(input: &str, count: usize, default_index: usize) -> Option<usize> {
    let input = input.trim();
    if input.is_empty() && default_index < count {
        return Some(default_index);
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

fn prepare_advice_for_task(
    events: &[Event],
    tags: &[String],
    projection_profiles: &[ExecutionProfile],
    selectable_profiles: &[ExecutionProfile],
) -> (Vec<Segment>, Projection, Advice) {
    let segments = segments_for_tags(tags);
    let projection = project(events, projection_profiles, &segments);
    let advice = advise(&projection, tags, selectable_profiles);
    (segments, projection, advice)
}

fn advice_default_index(advice: &Advice, profiles: &[ExecutionProfile]) -> usize {
    advice
        .proposed_profile_id()
        .and_then(|profile_id| profiles.iter().position(|profile| profile.id == profile_id))
        .unwrap_or(0)
}

fn print_advice(advice: &Advice, profiles: &[ExecutionProfile], now: Timestamp) {
    match &advice.outcome {
        AdviceOutcome::Proposed {
            profile_id,
            evidence,
        } => {
            println!("{}", render_advice_proposal(profile_id, profiles));
            for evidence in evidence {
                println!("  {}", render_advice_evidence(evidence, now));
            }
        }
        AdviceOutcome::Abstained { reason } => {
            println!("Advice: abstain ({})", advice_reason_label(reason));
            match reason {
                AdviceReason::NoEvidence { max_judged }
                | AdviceReason::InsufficientEvidence { max_judged } => {
                    println!(
                        "  Max judged by segment: {}",
                        render_segment_counts(max_judged)
                    );
                    if matches!(reason, AdviceReason::InsufficientEvidence { .. }) {
                        println!("  Threshold: {} judged", advice.threshold);
                    }
                }
                AdviceReason::NoUniqueLeader { profile_ids } => {
                    println!(
                        "  Competing profiles: {}",
                        profile_ids
                            .iter()
                            .map(|profile_id| escape_terminal(profile_id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
    }
}

fn render_advice_proposal(profile_id: &str, profiles: &[ExecutionProfile]) -> String {
    let profile = profiles
        .iter()
        .find(|profile| profile.id == profile_id)
        .map(|profile| escape_terminal(profile.name()))
        .unwrap_or_else(|| escape_terminal(profile_id));
    format!(
        "Advice: propose {profile} ({})",
        escape_terminal(profile_id)
    )
}

fn advice_reason_label(reason: &AdviceReason) -> &'static str {
    match reason {
        AdviceReason::NoEvidence { .. } => "no_evidence",
        AdviceReason::InsufficientEvidence { .. } => "insufficient_evidence",
        AdviceReason::NoUniqueLeader { .. } => "no_unique_leader",
    }
}

fn render_segment_counts(counts: &[AdviceSegmentCount]) -> String {
    counts
        .iter()
        .map(|count| {
            format!(
                "{}={}",
                render_evidence_segment(&count.segment),
                count.max_judged
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_advice_evidence(evidence: &AdviceEvidence, _now: Timestamp) -> String {
    let acceptance_latest = render_evidence_latest(&evidence.acceptance.summary);
    let duration_latest = render_evidence_latest(&evidence.duration.summary);
    let cost_latest = render_evidence_latest(&evidence.cost.summary);
    format!(
        "{}: Acceptance {}{} | Duration {}{} | {}{}",
        render_evidence_segment(&evidence.segment),
        format_evidence_acceptance(&evidence.acceptance),
        acceptance_latest,
        format_evidence_duration(&evidence.duration),
        duration_latest,
        format_evidence_cost(&evidence.cost),
        cost_latest,
    )
}

fn render_evidence_segment(segment: &EvidenceSegment) -> String {
    match segment {
        EvidenceSegment::Overall => "Overall".to_owned(),
        EvidenceSegment::Untagged => "Untagged".to_owned(),
        EvidenceSegment::Tag(tag) => {
            format!("Tag \"{}\"", escape_terminal(tag).replace('"', "\\\""))
        }
    }
}

fn render_evidence_latest(summary: &agent_dock::EvidenceAxisSummary) -> String {
    summary
        .latest_started_at
        .map(|latest| format!(", latest {latest}"))
        .unwrap_or_default()
}

fn format_evidence_acceptance(axis: &EvidenceAcceptance) -> String {
    if axis.summary.count == 0 {
        return "- (0/0)".to_owned();
    }
    let tenths = ((axis.accepted as u128 * 1_000) + axis.summary.count as u128 / 2)
        / axis.summary.count as u128;
    format!(
        "{}.{:01}% ({}/{})",
        tenths / 10,
        tenths % 10,
        axis.accepted,
        axis.summary.count
    )
}

fn format_evidence_duration(axis: &EvidenceDuration) -> String {
    let Some(ms) = axis.median_ms else {
        return format!("- ({})", axis.summary.count);
    };
    let value = if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{:01}s", ms / 1_000, (ms % 1_000) / 100)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms / 1_000) % 60)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms / 60_000) % 60)
    };
    format!("{value} ({})", axis.summary.count)
}

fn format_evidence_cost(cost: &EvidenceCost) -> String {
    match (
        cost.currency.as_deref(),
        cost.total_minor_units,
        cost.summary.count,
    ) {
        (Some(currency), Some(total), count) => format!(
            "Cost {} {total} minor units ({count}, {} missing)",
            escape_terminal(currency),
            cost.missing_count
        ),
        (None, None, count) if count > 0 => format!(
            "Cost mixed currencies ({count}, {} missing)",
            cost.missing_count
        ),
        _ => format!("Cost missing ({})", cost.missing_count),
    }
}

fn render_profile_choices(
    profiles: &[ExecutionProfile],
    scorecards: &[agent_dock::ProfileScorecard],
    hidden_tags: usize,
    now: Timestamp,
) -> Vec<String> {
    profiles
        .iter()
        .map(|profile| {
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
            if let Some(card) = scorecards.iter().find(|card| card.profile_id == profile.id) {
                let visible_score_count = card.scores.len().saturating_sub(hidden_tags);
                for score in card.scores.iter().take(visible_score_count) {
                    lines.push(format!("     {}", render_segment(score, now)));
                    lines.push(format!(
                        "         {}",
                        render_reported_disclosure(&score.reported, now)
                    ));
                }
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
        Segment::Tag(tag) => format!("Tag \"{}\"", escape_terminal(tag).replace('"', "\\\"")),
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
        "{label}: Acceptance {}{} | Duration {}{} | {}{}",
        format_acceptance(&score.acceptance),
        acceptance_recency,
        format_duration(&score.duration),
        duration_recency,
        format_cost(&score.cost),
        excluded,
    )
}

fn render_reported_disclosure(
    disclosure: &agent_dock::ReportedDisclosure,
    now: Timestamp,
) -> String {
    if disclosure.brokered_count == 0 {
        return "CLI-reported: not applicable".to_owned();
    }
    let latest = format_recency(now, disclosure.latest_started_at)
        .map(|value| format!(" | latest {value}"))
        .unwrap_or_default();
    format!(
        "CLI-reported (not actual cost): reported {}/{} | none {} | failures parse {} / limit {} / model {} | {}{}",
        disclosure.reported_count,
        disclosure.brokered_count,
        disclosure.missing_count,
        disclosure.failures.parse,
        disclosure.failures.limit,
        disclosure.failures.model,
        format_estimated_cost(&disclosure.estimated_cost),
        latest,
    )
}

fn format_estimated_cost(cost: &EstimatedCostSummary) -> String {
    match cost {
        EstimatedCostSummary::None => "est none".to_owned(),
        EstimatedCostSummary::Total {
            currency,
            amount_micros,
            count,
        } => format!(
            "est {} {}.{:06} ({count})",
            escape_terminal(currency),
            amount_micros / 1_000_000,
            amount_micros % 1_000_000,
        ),
        EstimatedCostSummary::MixedCurrencies { count } => {
            format!("est mixed currencies ({count})")
        }
        EstimatedCostSummary::Overflow => "est overflow".to_owned(),
    }
}

fn format_cost(cost: &agent_dock::CostAxis) -> String {
    match (
        cost.currency.as_deref(),
        cost.total_minor_units,
        cost.summary.count,
    ) {
        (Some(currency), Some(total), count) => format!(
            "Cost {} {total} minor units ({count}, {} missing)",
            escape_terminal(currency),
            cost.missing_count
        ),
        (None, None, count) if count > 0 => format!(
            "Cost mixed currencies ({count}, {} missing)",
            cost.missing_count
        ),
        _ => format!("Cost missing ({})", cost.missing_count),
    }
}

fn exclusion_reason_label(reason: ExclusionReason) -> &'static str {
    match reason {
        ExclusionReason::Ongoing => "ongoing",
        ExclusionReason::Interrupted => "interrupted",
        ExclusionReason::InvalidTiming => "invalid timing",
        ExclusionReason::MissingConfiguration => "missing configuration",
        ExclusionReason::ConfigurationChanged => "configuration changed",
        ExclusionReason::NoMatchingProfile => "no matching profile",
        ExclusionReason::AmbiguousProfile => "ambiguous profile",
    }
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

fn read_tags(
    editor: &mut DefaultEditor,
    candidates: &[String],
    initial: Option<&[String]>,
) -> Result<Vec<String>, ReadlineError> {
    if !candidates.is_empty() {
        println!("Tag candidates:");
        for (index, candidate) in candidates.iter().enumerate() {
            println!("  {}. {}", index + 1, escape_terminal(candidate));
        }
    }
    let input = if let Some(initial) = initial {
        let initial = initial.join(", ");
        editor.readline_with_initial(
            "Tags (comma-separated, blank for unclassified): ",
            (&initial, ""),
        )?
    } else {
        editor.readline("Tags (comma-separated, blank for unclassified): ")?
    };
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

fn recorded_execution_outcome(outcome: &ExecutionOutcome) -> (RecordedExecutionOutcome, u64) {
    match outcome {
        ExecutionOutcome::Completed { exit_code, elapsed } => (
            RecordedExecutionOutcome::Completed {
                exit_code: *exit_code,
            },
            elapsed_millis(*elapsed),
        ),
        ExecutionOutcome::Cancelled { elapsed } => (
            RecordedExecutionOutcome::Cancelled,
            elapsed_millis(*elapsed),
        ),
    }
}

fn executed_event_kind(
    profile: ProfileSnapshot,
    working_directory: PathBuf,
    started_at: Timestamp,
    elapsed_ms: u64,
    outcome: RecordedExecutionOutcome,
    report: ExecutionReport,
) -> EventKind {
    EventKind::Executed {
        origin: ExecutionOrigin::Brokered,
        profile,
        working_directory,
        started_at,
        elapsed_ms,
        outcome,
        cost: None,
        estimated_cost: report.estimated_cost,
        usage: report.usage,
        report_failure: report.report_failure,
    }
}

fn should_ask_immediate_judgement(outcome: &ExecutionOutcome) -> bool {
    matches!(outcome, ExecutionOutcome::Completed { .. })
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

fn advised_task_events(
    received: Event,
    advice: &Advice,
    selected_profile_id: &str,
) -> Result<Vec<Event>, AppError> {
    let task_id = received
        .task_id
        .ok_or_else(|| AppError::Usage("task event is missing task identity".to_owned()))?;
    let advised = Event::new(task_id, advice.event_kind());
    let advice_event_id = advised.event_id;
    let mut events = vec![received, advised];
    match &advice.outcome {
        AdviceOutcome::Proposed { profile_id, .. } if profile_id == selected_profile_id => {
            events.push(Event::new(
                task_id,
                EventKind::Approved {
                    advice_event_id,
                    profile_id: selected_profile_id.to_owned(),
                },
            ));
        }
        AdviceOutcome::Proposed { .. } => {
            events.push(Event::new(task_id, EventKind::Declined { advice_event_id }));
            events.push(Event::new(
                task_id,
                EventKind::Designated {
                    advice_event_id,
                    profile_id: selected_profile_id.to_owned(),
                },
            ));
        }
        AdviceOutcome::Abstained { .. } => {
            events.push(Event::new(
                task_id,
                EventKind::Designated {
                    advice_event_id,
                    profile_id: selected_profile_id.to_owned(),
                },
            ));
        }
    }
    Ok(events)
}

fn record_advised_task(
    event_log: &EventLog,
    expected: &ReadEvents,
    received: Event,
    advice: &Advice,
    selected_profile_id: &str,
) -> Result<AppendBatchOutcome, AppError> {
    let events = advised_task_events(received, advice, selected_profile_id)?;
    Ok(event_log.append_batch_if_unchanged(expected, &events)?)
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
        } => println!("{}", format_stdout_output(&line)),
        ExecutionEvent::Output {
            source: OutputSource::Stderr,
            line,
        } => eprintln!("[stderr] {line}"),
        ExecutionEvent::Warning { message } => {
            eprintln!("Warning: {}", escape_terminal(&message));
        }
        ExecutionEvent::Report { .. } => {}
        ExecutionEvent::Finished => {}
    }
}

fn format_stdout_output(line: &str) -> String {
    format!("[stdout] {}", escape_stdout_controls(line))
}

fn escape_stdout_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character != '\n' && character.is_control() {
            escaped.push_str(&format!("\\u{{{:x}}}", character as u32));
        } else {
            escaped.push(character);
        }
    }
    escaped
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
    Observation(#[from] agent_dock::ObservationError),
    #[error(transparent)]
    Hook(#[from] agent_dock::HookParseError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Readline(#[from] ReadlineError),
    #[error("no configured agent CLI is available")]
    NoAvailableProfiles,
    #[error("incompatible event log was not changed")]
    IncompatibleEventLogDeclined,
    #[error("incompatible event log")]
    IncompatibleEventLog,
    #[error("{0}")]
    Usage(String),
    #[error("execution {0} is no longer available")]
    ExecutionNoLongerAvailable(Uuid),
    #[error("session {0} has no active task; recorded an orphan end mark")]
    OrphanTaskEnd(Uuid),
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
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn hook_configuration_keeps_only_attribution_fields() {
        let configuration = hook_configuration(
            agent_dock::CliKind::Codex,
            &HookEvent::Stop {
                model: Some("gpt-5".to_owned()),
                permission_mode: Some("default".to_owned()),
            },
        )
        .unwrap();
        assert_eq!(
            configuration.model,
            agent_dock::ObservedValue::Observed(Some("gpt-5".to_owned()))
        );
        assert_eq!(
            configuration.identity,
            BTreeMap::from([(
                "permission_mode".to_owned(),
                agent_dock::ObservedValue::Observed("default".to_owned()),
            )])
        );
        assert!(
            hook_configuration(
                agent_dock::CliKind::Codex,
                &HookEvent::SessionEnd { reason: None }
            )
            .is_none()
        );
    }

    fn judgement_candidate() -> JudgementCandidate {
        JudgementCandidate {
            target_event_id: Uuid::parse_str("0198a8e2-9a80-7000-8000-000000000001").unwrap(),
            task_id: Uuid::parse_str("0198a8e2-9a80-7000-8000-000000000002").unwrap(),
            started_at: "2026-08-01T01:02:03Z".parse().unwrap(),
            profile: ProfileSnapshot {
                id: "profile-id".to_owned(),
                name: "Historical\nProfile".to_owned(),
                cli: agent_dock::CliKind::Test,
                execution_platform: ExecutionPlatform::Headless,
                executable_override: None,
                model: Some("model\tname".to_owned()),
                args: Vec::new(),
                identity: Default::default(),
                resolved_executable: Some(PathBuf::from("/usr/bin/true")),
            },
            outcome: RecordedExecutionOutcome::Completed { exit_code: Some(2) },
            tags: vec!["rust\nreview".to_owned()],
            current_verdict: Some(Verdict::Rejected),
        }
    }

    fn judgement_execution() -> Event {
        let candidate = judgement_candidate();
        let mut execution = Event::new(
            candidate.task_id,
            EventKind::Executed {
                origin: ExecutionOrigin::Brokered,
                profile: candidate.profile,
                working_directory: PathBuf::from("/tmp/project"),
                started_at: candidate.started_at,
                elapsed_ms: 100,
                outcome: candidate.outcome,
                cost: None,
                estimated_cost: None,
                usage: None,
                report_failure: None,
            },
        );
        execution.event_id = candidate.target_event_id;
        execution
    }

    fn old_event_with_format(format_version: &str) -> Event {
        let mut event = Event::new(
            Uuid::now_v7(),
            EventKind::TaskReceived {
                tags: vec!["old".to_owned()],
                prompt: None,
                prompt_chars: 1,
            },
        );
        event.format_version = format_version.to_owned();
        event
    }

    fn test_advice() -> Advice {
        Advice {
            referenced_segments: vec![Segment::Untagged],
            outcome: AdviceOutcome::Abstained {
                reason: AdviceReason::NoEvidence {
                    max_judged: vec![AdviceSegmentCount {
                        segment: EvidenceSegment::Untagged,
                        max_judged: 0,
                    }],
                },
            },
            threshold: agent_dock::ADVICE_MIN_JUDGED,
        }
    }

    fn proposed_test_advice(profile_id: &str) -> Advice {
        Advice {
            referenced_segments: vec![Segment::Untagged],
            outcome: AdviceOutcome::Proposed {
                profile_id: profile_id.to_owned(),
                evidence: vec![AdviceEvidence {
                    segment: EvidenceSegment::Untagged,
                    acceptance: EvidenceAcceptance {
                        summary: agent_dock::EvidenceAxisSummary {
                            count: 3,
                            latest_started_at: None,
                        },
                        accepted: 2,
                    },
                    duration: EvidenceDuration {
                        summary: agent_dock::EvidenceAxisSummary {
                            count: 0,
                            latest_started_at: None,
                        },
                        median_ms: None,
                    },
                    cost: EvidenceCost {
                        summary: agent_dock::EvidenceAxisSummary {
                            count: 0,
                            latest_started_at: None,
                        },
                        currency: None,
                        total_minor_units: None,
                        missing_count: 3,
                    },
                }],
            },
            threshold: agent_dock::ADVICE_MIN_JUDGED,
        }
    }

    fn write_test_executable(directory: &Path, source: &str) -> PathBuf {
        let path = directory.join("fake-agent");
        fs::write(&path, source).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn test_claude_profile(executable: PathBuf) -> ExecutionProfile {
        ProfileDeclaration {
            name: "Fake Claude".to_owned(),
            cli: agent_dock::CliKind::Claude,
            execution_platform: ExecutionPlatform::Headless,
            executable: Some(executable.clone()),
            model: None,
            args: Vec::new(),
            identity: Default::default(),
        }
        .resolve(executable)
        .unwrap()
    }

    #[tokio::test]
    async fn execute_and_record_persists_reports_for_completed_cancelled_and_parse_failed_runs() {
        let cases = [
            (
                "#!/bin/sh\nprintf '%s\\n' '{\"result\":\"done\",\"total_cost_usd\":0.25,\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}'\n",
                false,
                Some(agent_dock::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 250_000,
                }),
                Some(agent_dock::TokenUsage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                None,
            ),
            (
                "#!/bin/sh\ntrap 'exit 0' INT\nprintf '%s\\n' '{\"result\":\"cancelled\",\"total_cost_usd\":0.25,\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}'\nprintf 'ready\\n' >&2\nwhile :; do sleep 1; done\n",
                true,
                Some(agent_dock::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 250_000,
                }),
                Some(agent_dock::TokenUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(4),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                None,
            ),
            (
                "#!/bin/sh\nprintf 'not-json\\n'\n",
                false,
                None,
                None,
                Some(agent_dock::ReportFailure::ParseFailure),
            ),
        ];

        for (index, (script, should_cancel, expected_cost, expected_usage, expected_failure)) in
            cases.into_iter().enumerate()
        {
            let directory = tempfile::tempdir().unwrap();
            let executable = write_test_executable(directory.path(), script);
            let profile = test_claude_profile(executable);
            let log = EventLog::new(directory.path().join("events.jsonl"));
            let task_id = Uuid::now_v7();
            let (cancellation_sender, cancellation) = oneshot::channel();
            let trigger = Arc::new(Mutex::new(should_cancel.then_some(cancellation_sender)));
            let trigger_for_callback = trigger.clone();

            let recorded = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                execute_and_record(
                    &log,
                    &profile,
                    task_id,
                    format!("fixture {index}"),
                    directory.path().to_path_buf(),
                    cancellation,
                    move |event| {
                        if should_cancel
                            && matches!(
                                &event,
                                ExecutionEvent::Output {
                                    source: OutputSource::Stderr,
                                    line
                                } if line == "ready"
                            )
                            && let Some(sender) = trigger_for_callback.lock().unwrap().take()
                        {
                            let _ = sender.send(());
                        }
                    },
                ),
            )
            .await
            .expect("fixture execution should not hang")
            .unwrap()
            .expect("execution should append an executed event");

            assert_eq!(
                matches!(recorded, ExecutionOutcome::Cancelled { .. }),
                should_cancel
            );
            let events = log.read_all().unwrap().events;
            assert_eq!(events.len(), 1);
            let EventKind::Executed {
                cost,
                estimated_cost,
                usage,
                report_failure,
                ..
            } = &events[0].kind
            else {
                panic!("the helper should append an executed event");
            };
            assert_eq!(events[0].task_id, Some(task_id));
            assert_eq!(cost, &None);
            assert_eq!(estimated_cost, &expected_cost);
            assert_eq!(usage, &expected_usage);
            assert_eq!(*report_failure, expected_failure);
        }
    }

    #[tokio::test]
    async fn execute_and_record_records_spawn_failure_for_missing_executable() {
        let directory = tempfile::tempdir().unwrap();
        let profile = test_claude_profile(directory.path().join("missing-agent"));
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let task_id = Uuid::now_v7();
        let (_cancellation_sender, cancellation) = oneshot::channel();

        let recorded = execute_and_record(
            &log,
            &profile,
            task_id,
            "missing executable".to_owned(),
            directory.path().to_path_buf(),
            cancellation,
            |_| {},
        )
        .await
        .unwrap();

        assert!(recorded.is_none());
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].task_id, Some(task_id));
        let EventKind::Executed {
            outcome,
            cost,
            estimated_cost,
            usage,
            report_failure,
            ..
        } = &events[0].kind
        else {
            panic!("the helper should append a failed executed event");
        };
        assert!(matches!(
            outcome,
            RecordedExecutionOutcome::Failed {
                failure_kind: FailureKind::Spawn,
                ..
            }
        ));
        assert_eq!(cost, &None);
        assert_eq!(estimated_cost, &None);
        assert_eq!(usage, &None);
        assert_eq!(*report_failure, None);
    }

    #[test]
    fn rejects_blank_prompts() {
        assert!(!prompt_is_valid(""));
        assert!(!prompt_is_valid("  \t"));
        assert!(prompt_is_valid("do the work"));
    }

    #[test]
    fn parses_commands_without_loading_the_execution_configuration() {
        assert_eq!(command_from_args([].into_iter()).unwrap(), AppCommand::Run);
        assert_eq!(
            command_from_args([OsString::from("judge")].into_iter()).unwrap(),
            AppCommand::Judge
        );
        assert_eq!(
            command_from_args([OsString::from("scorecard")].into_iter()).unwrap(),
            AppCommand::Scorecard
        );
        assert_eq!(
            command_from_args([OsString::from("--help")].into_iter()).unwrap(),
            AppCommand::Help
        );
        assert!(matches!(
            command_from_args([OsString::from("unknown")].into_iter()),
            Err(AppError::Usage(message)) if message.contains("Usage: agent-dock [judge|scorecard|observe]")
        ));
        assert!(matches!(
            command_from_args(
                [OsString::from("judge"), OsString::from("extra")].into_iter()
            ),
            Err(AppError::Usage(message)) if message.contains("too many arguments")
        ));
    }

    #[test]
    fn parses_explicit_judgement_selections_and_verdicts() {
        assert_eq!(parse_required_selection("1", 2), Some(0));
        assert_eq!(parse_required_selection(" 2 ", 2), Some(1));
        assert_eq!(parse_required_selection("", 2), None);
        assert_eq!(parse_required_selection("0", 2), None);
        assert_eq!(parse_required_selection("3", 2), None);
        assert_eq!(parse_verdict("a"), Some(Verdict::Accepted));
        assert_eq!(parse_verdict(" ACCEPTED "), Some(Verdict::Accepted));
        assert_eq!(parse_verdict("r"), Some(Verdict::Rejected));
        assert_eq!(parse_verdict("rejected"), Some(Verdict::Rejected));
        assert_eq!(parse_verdict(""), None);
    }

    #[test]
    fn judgement_flow_cancels_without_recording_events() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        assert_eq!(
            resolve_judgement_with(
                &log,
                |_| panic!("an empty history must not ask for a selection"),
                || panic!("an empty history must not ask for a verdict"),
                |_, _| panic!("an empty history must not ask for confirmation"),
                |_, _| panic!("an empty history cannot have a stale judgement"),
            )
            .unwrap(),
            JudgementResolution::NoCandidates
        );

        log.append(&judgement_execution()).unwrap();
        assert_eq!(
            resolve_judgement_with(
                &log,
                |_| Ok(None),
                || panic!("quitting selection must not ask for a verdict"),
                |_, _| panic!("quitting selection must not ask for confirmation"),
                |_, _| panic!("quitting selection cannot have a stale judgement"),
            )
            .unwrap(),
            JudgementResolution::Quit
        );
        assert_eq!(
            resolve_judgement_with(
                &log,
                |_| Ok(Some(0)),
                || Ok(None),
                |_, _| panic!("quitting verdict input must not ask for confirmation"),
                |_, _| panic!("quitting verdict input cannot have a stale judgement"),
            )
            .unwrap(),
            JudgementResolution::Quit
        );
        assert_eq!(
            resolve_judgement_with(
                &log,
                |_| Ok(Some(0)),
                || Ok(Some(Verdict::Accepted)),
                |_, _| Ok(false),
                |_, _| panic!("declining confirmation cannot have a stale judgement"),
            )
            .unwrap(),
            JudgementResolution::Declined
        );
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::UnexpectedEof] {
            let error = resolve_judgement_with(
                &log,
                |_| Ok(Some(0)),
                || Ok(Some(Verdict::Accepted)),
                |_, _| Err(AppError::Io(io::Error::new(kind, "stopped"))),
                |_, _| panic!("interrupted confirmation cannot have a stale judgement"),
            )
            .unwrap_err();
            assert!(matches!(error, AppError::Io(source) if source.kind() == kind));
        }
        assert_eq!(log.read_all().unwrap().events.len(), 1);
    }

    #[test]
    fn judgement_flow_reconfirms_a_concurrently_changed_verdict() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let execution = judgement_execution();
        let task_id = execution.task_id.unwrap();
        log.append(&execution).unwrap();
        let mut confirmed = Vec::new();
        let mut changes = Vec::new();

        let resolution = resolve_judgement_with(
            &log,
            |_| Ok(Some(0)),
            || Ok(Some(Verdict::Rejected)),
            |candidate, _| {
                confirmed.push(candidate.current_verdict);
                if confirmed.len() == 1 {
                    log.append(&Event::new(
                        candidate.task_id,
                        EventKind::Judged {
                            verdict: Verdict::Accepted,
                        },
                    ))?;
                }
                Ok(true)
            },
            |previous, current| changes.push((previous, current)),
        )
        .unwrap();

        assert_eq!(
            resolution,
            JudgementResolution::Appended {
                task_id,
                verdict: Verdict::Rejected
            }
        );
        assert_eq!(confirmed, [None, Some(Verdict::Accepted)]);
        assert_eq!(changes, [(None, Some(Verdict::Accepted))]);
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[2].kind,
            EventKind::Judged {
                verdict: Verdict::Rejected
            }
        ));
    }

    #[test]
    fn judgement_flow_reports_a_selected_execution_removed_before_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let execution = judgement_execution();
        let task_id = execution.task_id.unwrap();
        log.append(&execution).unwrap();

        let error = resolve_judgement_with(
            &log,
            |_| {
                fs::remove_file(&path).unwrap();
                Ok(Some(0))
            },
            || Ok(Some(Verdict::Accepted)),
            |_, _| panic!("a removed execution must not ask for confirmation"),
            |_, _| panic!("a removed execution has no changed verdict"),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            AppError::ExecutionNoLongerAvailable(target) if target == task_id
        ));
        assert!(!path.exists());
    }

    #[test]
    fn renders_identifying_execution_fields_without_terminal_controls() {
        assert_eq!(
            render_judgement_candidate(&judgement_candidate()),
            "0198a8e2-9a80-7000-8000-000000000001 | 2026-08-01T01:02:03Z | Historical\\nProfile [test; model\\tname; profile-id] | completed with exit code 2 | tags: rust\\nreview | current: rejected"
        );
    }

    #[test]
    fn builds_executed_event_kind_for_completed_cancelled_and_parse_failed_reports() {
        let profile = safe_test_profile();
        let profile_snapshot = ProfileSnapshot::from(&profile);
        let working_directory = PathBuf::from("/tmp/project");
        let started_at: Timestamp = "2026-08-01T01:02:03Z".parse().unwrap();
        let cases = vec![
            (
                RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                ExecutionReport {
                    estimated_cost: Some(agent_dock::EstimatedCost {
                        currency: "USD".to_owned(),
                        amount_micros: 42,
                    }),
                    usage: Some(agent_dock::TokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        cached_input_tokens: Some(3),
                        reasoning_tokens: Some(4),
                    }),
                    report_failure: None,
                },
                7,
            ),
            (
                RecordedExecutionOutcome::Cancelled,
                ExecutionReport {
                    estimated_cost: None,
                    usage: Some(agent_dock::TokenUsage {
                        input_tokens: Some(5),
                        output_tokens: None,
                        cached_input_tokens: None,
                        reasoning_tokens: None,
                    }),
                    report_failure: None,
                },
                11,
            ),
            (
                RecordedExecutionOutcome::Completed { exit_code: None },
                ExecutionReport {
                    estimated_cost: None,
                    usage: None,
                    report_failure: Some(agent_dock::ReportFailure::ParseFailure),
                },
                13,
            ),
        ];

        for (outcome, report, elapsed_ms) in cases {
            let expected = EventKind::Executed {
                origin: ExecutionOrigin::Brokered,
                profile: profile_snapshot.clone(),
                working_directory: working_directory.clone(),
                started_at,
                elapsed_ms,
                outcome: outcome.clone(),
                cost: None,
                estimated_cost: report.estimated_cost.clone(),
                usage: report.usage.clone(),
                report_failure: report.report_failure,
            };
            assert_eq!(
                executed_event_kind(
                    profile_snapshot.clone(),
                    working_directory.clone(),
                    started_at,
                    elapsed_ms,
                    outcome,
                    report,
                ),
                expected
            );
        }
    }

    #[test]
    fn escapes_terminal_controls_in_machine_readable_and_raw_output_display() {
        let machine_readable_fixture = r#"{"result":"json \u001b]8;;https://example.invalid\u0007link\u001b]8;;\u0007 carriage\rreturn"}"#;
        let machine_readable_line =
            serde_json::from_str::<serde_json::Value>(machine_readable_fixture).unwrap()["result"]
                .as_str()
                .unwrap()
                .to_owned();
        assert_eq!(
            format_stdout_output(&machine_readable_line),
            "[stdout] json \\u{1b}]8;;https://example.invalid\\u{7}link\\u{1b}]8;;\\u{7} carriage\\u{d}return"
        );

        let raw_output_line = "raw \x1b[2J bell\x07 carriage\rreturn";
        assert_eq!(
            format_stdout_output(raw_output_line),
            "[stdout] raw \\u{1b}[2J bell\\u{7} carriage\\u{d}return"
        );
    }

    #[test]
    fn stdout_display_preserves_backslashes_and_escapes_all_terminal_controls() {
        let printable = r"Windows C:\work\agent\output.txt regex ^\d+\s+$";
        assert_eq!(
            format_stdout_output(printable),
            r"[stdout] Windows C:\work\agent\output.txt regex ^\d+\s+$"
        );
        assert_eq!(
            format_stdout_output("line\nnext\x7f c1\u{009d} osc\x1b]0;title\x07"),
            "[stdout] line\nnext\\u{7f} c1\\u{9d} osc\\u{1b}]0;title\\u{7}"
        );
    }

    #[test]
    fn identifies_cancelled_and_unknown_exit_executions_in_the_judgement_list() {
        for (outcome, expected) in [
            (RecordedExecutionOutcome::Cancelled, "cancelled"),
            (
                RecordedExecutionOutcome::Completed { exit_code: None },
                "completed with unknown exit code",
            ),
        ] {
            assert_eq!(execution_outcome_label(&outcome), expected);
        }
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
    fn declining_run_records_no_events() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Run this task?");
            assert!(default);
            Ok(false)
        };
        let confirmation = confirm_run_with(&mut ask).unwrap();
        let mut first_edit = |initial: &str| -> Result<String, AppError> {
            assert_eq!(initial, "do not run");
            Ok("try another task".to_owned())
        };
        let first_attempt =
            resolve_run_confirmation("do not run", confirmation, &mut first_edit, || {
                record_advised_task(
                    &log,
                    &log.read_all()?,
                    task_received_event(
                        Uuid::now_v7(),
                        Vec::new(),
                        "do not run",
                        false,
                        false,
                        Timestamp::now(),
                    ),
                    &test_advice(),
                    "test-default",
                )
                .map(|_| ())
            })
            .unwrap();
        let RunResolution::Edit(edited) = first_attempt else {
            panic!("declining the run must return the edited prompt")
        };
        assert!(log.read_all().unwrap().events.is_empty());

        let confirmation = confirm_run_with(&mut ask).unwrap();
        let mut second_edit = |initial: &str| -> Result<String, AppError> {
            assert_eq!(initial, "try another task");
            Ok("run this instead".to_owned())
        };
        let second_attempt =
            resolve_run_confirmation(&edited, confirmation, &mut second_edit, || {
                record_advised_task(
                    &log,
                    &log.read_all()?,
                    task_received_event(
                        Uuid::now_v7(),
                        Vec::new(),
                        "try another task",
                        false,
                        false,
                        Timestamp::now(),
                    ),
                    &test_advice(),
                    "test-default",
                )
                .map(|_| ())
            })
            .unwrap();
        let RunResolution::Edit(edited) = second_attempt else {
            panic!("declining the run must return the edited prompt")
        };

        assert_eq!(edited, "run this instead");
        assert!(log.read_all().unwrap().events.is_empty());
    }

    #[test]
    fn run_confirmation_does_not_edit_the_prompt() {
        let mut edit = |_: &str| -> Result<String, AppError> {
            panic!("the prompt must not be edited after approval")
        };

        let resolution =
            resolve_run_confirmation("run this", RunConfirmation::Run, &mut edit, || {
                Ok("approved")
            })
            .unwrap();
        assert_eq!(resolution, RunResolution::Run("approved"));
    }

    #[test]
    fn task_retries_offer_previous_tags_for_editing() {
        let mut tag_edits = 0;
        let mut attempts = 0;
        let (prompt, approved) = resolve_task_with_editable_tags(
            "first task".to_owned(),
            vec!["rust".to_owned(), "review".to_owned()],
            |initial| {
                tag_edits += 1;
                Ok(match tag_edits {
                    1 => {
                        assert_eq!(initial, ["rust", "review"]);
                        vec!["docs".to_owned()]
                    }
                    2 => {
                        assert_eq!(initial, ["docs"]);
                        vec!["docs".to_owned(), "review".to_owned()]
                    }
                    _ => panic!("tags must only be edited after a declined run"),
                })
            },
            |prompt, tags| {
                attempts += 1;
                Ok(match attempts {
                    1 => {
                        assert_eq!(prompt, "first task");
                        assert_eq!(tags, ["rust", "review"]);
                        RunResolution::Edit("second task".to_owned())
                    }
                    2 => {
                        assert_eq!(prompt, "second task");
                        assert_eq!(tags, ["docs"]);
                        RunResolution::Edit("approved task".to_owned())
                    }
                    3 => {
                        assert_eq!(prompt, "approved task");
                        assert_eq!(tags, ["docs", "review"]);
                        RunResolution::Run("approved")
                    }
                    _ => panic!("task must finish after the third attempt"),
                })
            },
        )
        .unwrap();

        assert_eq!(prompt, "approved task");
        assert_eq!(approved, "approved");
        assert_eq!(tag_edits, 2);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn records_only_the_edited_task_after_a_declined_run() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let task_id = Uuid::now_v7();
        let mut attempts = 0;

        let (prompt, approved_task_id) = resolve_task_with_editable_tags(
            "original task".to_owned(),
            vec!["original".to_owned()],
            |initial| {
                assert_eq!(initial, ["original"]);
                Ok(vec!["edited".to_owned()])
            },
            |prompt, tags| {
                attempts += 1;
                match attempts {
                    1 => {
                        assert_eq!(prompt, "original task");
                        assert_eq!(tags, ["original"]);
                        Ok(RunResolution::Edit("edited task".to_owned()))
                    }
                    2 => {
                        assert_eq!(prompt, "edited task");
                        assert_eq!(tags, ["edited"]);
                        record_advised_task(
                            &log,
                            &log.read_all()?,
                            task_received_event(
                                task_id,
                                tags.to_vec(),
                                prompt,
                                true,
                                true,
                                Timestamp::now(),
                            ),
                            &test_advice(),
                            "test-default",
                        )?;
                        Ok(RunResolution::Run(task_id))
                    }
                    _ => panic!("the edited task must be approved on the second attempt"),
                }
            },
        )
        .unwrap();

        assert_eq!(prompt, "edited task");
        assert_eq!(approved_task_id, task_id);
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0].kind,
            EventKind::TaskReceived { tags, prompt, .. }
                if tags == &["edited"] && prompt.as_deref() == Some("edited task")
        ));
        assert!(matches!(events[1].kind, EventKind::Advised { .. }));
        assert!(matches!(events[2].kind, EventKind::Designated { .. }));
    }

    #[test]
    fn tag_editing_propagates_interrupts_and_end_of_input() {
        for interrupted in [true, false] {
            let result = resolve_task_with_editable_tags(
                "edit this".to_owned(),
                vec!["rust".to_owned()],
                |_| {
                    Err(AppError::Readline(if interrupted {
                        ReadlineError::Interrupted
                    } else {
                        ReadlineError::Eof
                    }))
                },
                |_, _| Ok(RunResolution::<()>::Edit("edited".to_owned())),
            );
            assert!(matches!(
                (result, interrupted),
                (Err(AppError::Readline(ReadlineError::Interrupted)), true)
                    | (Err(AppError::Readline(ReadlineError::Eof)), false)
            ));
        }
    }

    #[test]
    fn declined_run_announces_return_to_task_definition() {
        assert_eq!(
            run_confirmation_notice(RunConfirmation::Edit),
            Some(("Task was not run.", "Returning to task definition..."))
        );
        assert_eq!(run_confirmation_notice(RunConfirmation::Run), None);
    }

    #[test]
    fn run_confirmation_propagates_interrupts_and_end_of_input() {
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::UnexpectedEof] {
            let mut ask = |_: &str, _: bool| Err(io::Error::new(kind, "stop"));
            assert!(matches!(
                confirm_run_with(&mut ask),
                Err(source) if source.kind() == kind
            ));
        }
    }

    #[test]
    fn prompt_editing_propagates_interrupts_and_end_of_input() {
        let mut interrupt = |_: &str| Err(AppError::Readline(ReadlineError::Interrupted));
        assert!(matches!(
            resolve_run_confirmation("edit this", RunConfirmation::Edit, &mut interrupt, || {
                Ok(())
            }),
            Err(AppError::Readline(ReadlineError::Interrupted))
        ));

        let mut end_of_input = |_: &str| Err(AppError::Readline(ReadlineError::Eof));
        assert!(matches!(
            resolve_run_confirmation(
                "edit this",
                RunConfirmation::Edit,
                &mut end_of_input,
                || Ok(())
            ),
            Err(AppError::Readline(ReadlineError::Eof))
        ));
    }

    #[test]
    fn records_task_events_only_after_run_approval() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let task_id = Uuid::now_v7();

        let mut edit = |_: &str| -> Result<String, AppError> {
            panic!("an approved task must not return to editing")
        };
        let resolution =
            resolve_run_confirmation("run this", RunConfirmation::Run, &mut edit, || {
                record_advised_task(
                    &log,
                    &log.read_all()?,
                    task_received_event(
                        task_id,
                        vec!["rust".to_owned()],
                        "run this",
                        false,
                        false,
                        Timestamp::now(),
                    ),
                    &test_advice(),
                    "test-default",
                )?;
                Ok(task_id)
            })
            .unwrap();

        let events = log.read_all().unwrap().events;
        assert_eq!(resolution, RunResolution::Run(task_id));
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].task_id, Some(task_id));
        assert!(matches!(events[0].kind, EventKind::TaskReceived { .. }));
        assert_eq!(events[1].task_id, Some(task_id));
        assert!(matches!(events[1].kind, EventKind::Advised { .. }));
        assert!(matches!(events[2].kind, EventKind::Designated { .. }));
    }

    #[test]
    fn records_approval_with_the_advice_event_id_in_one_batch() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = safe_test_profile();
        let task_id = Uuid::now_v7();
        let advice = proposed_test_advice(&profile.id);

        assert_eq!(
            record_advised_task(
                &log,
                &log.read_all().unwrap(),
                task_received_event(
                    task_id,
                    Vec::new(),
                    "run this",
                    false,
                    false,
                    Timestamp::now(),
                ),
                &advice,
                &profile.id,
            )
            .unwrap(),
            AppendBatchOutcome::Appended
        );
        log.append(&Event::new(
            task_id,
            EventKind::Executed {
                origin: ExecutionOrigin::Brokered,
                profile: ProfileSnapshot::from(&profile),
                working_directory: PathBuf::from("/tmp"),
                started_at: Timestamp::now(),
                elapsed_ms: 100,
                outcome: RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                cost: None,
                estimated_cost: None,
                usage: None,
                report_failure: None,
            },
        ))
        .unwrap();
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0].kind, EventKind::TaskReceived { .. }));
        let EventKind::Advised {
            referenced_segments,
            outcome,
            threshold,
        } = &events[1].kind
        else {
            panic!("the second event must contain the advice");
        };
        assert_eq!(referenced_segments, &vec![EvidenceSegment::Untagged]);
        assert_eq!(*threshold, agent_dock::ADVICE_MIN_JUDGED as u64);
        let AdviceOutcome::Proposed {
            profile_id,
            evidence,
        } = outcome
        else {
            panic!("the approval test must use a proposal");
        };
        assert_eq!(profile_id, &profile.id);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].segment, EvidenceSegment::Untagged);
        assert_eq!(evidence[0].acceptance.summary.count, 3);
        assert_eq!(evidence[0].acceptance.accepted, 2);
        assert_eq!(evidence[0].duration.summary.count, 0);
        assert_eq!(evidence[0].duration.median_ms, None);
        assert_eq!(evidence[0].cost.summary.count, 0);
        assert_eq!(evidence[0].cost.missing_count, 3);
        let advice_event_id = events[1].event_id;
        assert!(matches!(
            &events[2].kind,
            EventKind::Approved {
                advice_event_id: id,
                profile_id,
            } if *id == advice_event_id && profile_id == &profile.id
        ));
        assert!(matches!(events[3].kind, EventKind::Executed { .. }));
    }

    #[test]
    fn records_declined_advice_before_a_designation() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let selected = safe_test_profile();
        let task_id = Uuid::now_v7();
        let advice = proposed_test_advice("recommended");
        record_advised_task(
            &log,
            &log.read_all().unwrap(),
            task_received_event(
                task_id,
                vec!["rust".to_owned()],
                "run this",
                false,
                false,
                Timestamp::now(),
            ),
            &advice,
            &selected.id,
        )
        .unwrap();

        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 4);
        let advice_event_id = events[1].event_id;
        assert!(matches!(
            events[2].kind,
            EventKind::Declined { advice_event_id: id } if id == advice_event_id
        ));
        assert!(matches!(
            &events[3].kind,
            EventKind::Designated {
                advice_event_id: id,
                profile_id,
            } if *id == advice_event_id && profile_id == &selected.id
        ));
    }

    #[test]
    fn records_only_a_designation_after_abstaining() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = safe_test_profile();
        let task_id = Uuid::now_v7();
        record_advised_task(
            &log,
            &log.read_all().unwrap(),
            task_received_event(
                task_id,
                Vec::new(),
                "run this",
                false,
                false,
                Timestamp::now(),
            ),
            &test_advice(),
            &profile.id,
        )
        .unwrap();

        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 3);
        let advice_event_id = events[1].event_id;
        assert!(matches!(
            &events[2].kind,
            EventKind::Designated {
                advice_event_id: id,
                profile_id,
            } if *id == advice_event_id && profile_id == &profile.id
        ));
    }

    #[test]
    fn does_not_append_a_stale_advice_batch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        let initial = log.read_all().unwrap();
        log.append(&task_received_event(
            Uuid::now_v7(),
            Vec::new(),
            "another task",
            false,
            false,
            Timestamp::now(),
        ))
        .unwrap();
        let before = fs::read(&path).unwrap();
        let profile = safe_test_profile();

        assert_eq!(
            record_advised_task(
                &log,
                &initial,
                task_received_event(
                    Uuid::now_v7(),
                    Vec::new(),
                    "stale task",
                    false,
                    false,
                    Timestamp::now(),
                ),
                &test_advice(),
                &profile.id,
            )
            .unwrap(),
            AppendBatchOutcome::Changed
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn retries_advice_after_the_event_log_is_removed_during_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        log.append(&task_received_event(
            Uuid::now_v7(),
            Vec::new(),
            "existing task",
            false,
            false,
            Timestamp::now(),
        ))
        .unwrap();
        let profile = safe_test_profile();
        let mut preparations = 0;

        let result = resolve_advised_task_with(&log, |history| {
            preparations += 1;
            let received = task_received_event(
                Uuid::now_v7(),
                Vec::new(),
                "run this",
                false,
                false,
                Timestamp::now(),
            );
            if preparations == 1 {
                assert_eq!(history.events.len(), 1);
                fs::remove_file(&path)?;
                Ok(RunResolution::Run((
                    received,
                    test_advice(),
                    profile.id.clone(),
                    "stale".to_owned(),
                )))
            } else {
                assert!(history.events.is_empty());
                Ok(RunResolution::Run((
                    received,
                    proposed_test_advice(&profile.id),
                    profile.id.clone(),
                    "recalculated".to_owned(),
                )))
            }
        })
        .unwrap();

        assert_eq!(result, RunResolution::Run("recalculated".to_owned()));
        assert_eq!(preparations, 2);
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[1].kind,
            EventKind::Advised {
                outcome: AdviceOutcome::Proposed { .. },
                ..
            }
        ));
    }

    #[test]
    fn propagates_advice_batch_append_errors_before_run() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let log = EventLog::new(&path);
        log.append(&task_received_event(
            Uuid::now_v7(),
            Vec::new(),
            "existing task",
            false,
            false,
            Timestamp::now(),
        ))
        .unwrap();
        let profile = safe_test_profile();

        let result = resolve_advised_task_with(&log, |_| {
            fs::remove_file(&path)?;
            fs::create_dir(&path)?;
            Ok(RunResolution::Run((
                task_received_event(
                    Uuid::now_v7(),
                    Vec::new(),
                    "run this",
                    false,
                    false,
                    Timestamp::now(),
                ),
                test_advice(),
                profile.id.clone(),
                (),
            )))
        });

        let Err(error) = result else {
            panic!("an append error must not resolve the task for execution");
        };
        assert!(matches!(error, AppError::Record(_)));
        assert!(path.is_dir());
        assert!(fs::read_dir(path).unwrap().next().is_none());
    }

    #[test]
    fn propagates_interruption_after_representing_advice_without_recording_a_batch() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = safe_test_profile();
        let mut preparations = 0;

        let result = resolve_advised_task_with(&log, |history| {
            preparations += 1;
            if preparations == 1 {
                log.append(&task_received_event(
                    Uuid::now_v7(),
                    Vec::new(),
                    "intervening task",
                    false,
                    false,
                    Timestamp::now(),
                ))?;
                Ok(RunResolution::Run((
                    task_received_event(
                        Uuid::now_v7(),
                        Vec::new(),
                        "run this",
                        false,
                        false,
                        Timestamp::now(),
                    ),
                    test_advice(),
                    profile.id.clone(),
                    (),
                )))
            } else {
                assert_eq!(history.events.len(), 1);
                Err(AppError::Readline(ReadlineError::Interrupted))
            }
        });

        assert!(matches!(
            result,
            Err(AppError::Readline(ReadlineError::Interrupted))
        ));
        assert_eq!(preparations, 2);
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventKind::TaskReceived { .. }));
    }

    #[test]
    fn retries_advice_preparation_after_the_history_changes() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = safe_test_profile();
        let mut preparations = 0_usize;
        let result = resolve_advised_task_with(&log, |history| {
            preparations += 1;
            let received = task_received_event(
                Uuid::now_v7(),
                Vec::new(),
                "run this",
                false,
                false,
                Timestamp::now(),
            );
            let advice = if history.events.is_empty() {
                test_advice()
            } else {
                proposed_test_advice(&profile.id)
            };
            if preparations == 1 {
                log.append(&task_received_event(
                    Uuid::now_v7(),
                    Vec::new(),
                    "intervening task",
                    false,
                    false,
                    Timestamp::now(),
                ))?;
            }
            assert_eq!(history.events.len(), preparations.saturating_sub(1));
            Ok(RunResolution::Run((
                received,
                advice,
                profile.id.clone(),
                "done".to_owned(),
            )))
        })
        .unwrap();

        assert_eq!(result, RunResolution::Run("done".to_owned()));
        assert_eq!(preparations, 2);
        let events = log.read_all().unwrap().events;
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[2].kind,
            EventKind::Advised {
                outcome: AdviceOutcome::Proposed { profile_id, .. },
                ..
            } if profile_id == &profile.id
        ));
        assert!(matches!(
            &events[3].kind,
            EventKind::Approved { profile_id, .. } if profile_id == &profile.id
        ));
    }

    #[test]
    fn chooses_the_proposed_profile_as_the_default_by_id() {
        let first = safe_test_profile();
        let mut declaration = agent_dock::safe_test_declaration();
        declaration.model = Some("second".to_owned());
        let second = declaration.resolve(PathBuf::from("/usr/bin/true")).unwrap();
        let proposed = proposed_test_advice(&second.id);
        assert_eq!(advice_default_index(&proposed, &[first, second.clone()]), 1);
        assert_eq!(advice_default_index(&test_advice(), &[second]), 0);
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
    fn records_elapsed_reported_by_the_crew_member_execution() {
        for (outcome, expected, expected_ms) in [
            (
                ExecutionOutcome::Completed {
                    exit_code: Some(0),
                    elapsed: std::time::Duration::from_millis(7),
                },
                RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                7,
            ),
            (
                ExecutionOutcome::Cancelled {
                    elapsed: std::time::Duration::from_millis(11),
                },
                RecordedExecutionOutcome::Cancelled,
                11,
            ),
        ] {
            assert_eq!(
                recorded_execution_outcome(&outcome),
                (expected, expected_ms)
            );
        }
    }

    #[test]
    fn cancelled_executions_skip_the_immediate_judgement() {
        assert!(!should_ask_immediate_judgement(
            &ExecutionOutcome::Cancelled {
                elapsed: std::time::Duration::from_millis(1),
            }
        ));
        assert!(should_ask_immediate_judgement(
            &ExecutionOutcome::Completed {
                exit_code: Some(0),
                elapsed: std::time::Duration::from_millis(1),
            }
        ));
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
    fn preserves_a_new_incompatible_configuration_without_asking_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = \"old\"\n").unwrap();
        let newer = "schema_version = \"changed\"\n";
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
            "{}\n{}\n{}\n",
            serde_json::to_string(&current).unwrap(),
            serde_json::to_string(&old_event_with_format("old")).unwrap(),
            serde_json::to_string(&old_event_with_format("agent-dock/events/v1alpha4")).unwrap()
        );
        fs::write(&path, &original).unwrap();
        let mut ask = |prompt: &str, default: bool| {
            assert_eq!(prompt, "Back up and clear the incompatible event log?");
            assert!(!default);
            Ok(true)
        };
        let history = read_event_history_with(&EventLog::new(&path), &mut ask).unwrap();

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
        for format_version in ["old", "agent-dock/events/v1alpha4"] {
            let original =
                serde_json::to_string(&old_event_with_format(format_version)).unwrap() + "\n";
            fs::write(&path, &original).unwrap();
            let mut ask = |prompt: &str, default: bool| {
                assert_eq!(prompt, "Back up and clear the incompatible event log?");
                assert!(!default);
                Ok(false)
            };

            assert!(matches!(
                read_event_history_with(&EventLog::new(&path), &mut ask),
                Err(AppError::IncompatibleEventLogDeclined)
            ));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn preserves_an_incompatible_event_log_if_confirmation_is_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        for format_version in ["old", "agent-dock/events/v1alpha4"] {
            let original =
                serde_json::to_string(&old_event_with_format(format_version)).unwrap() + "\n";
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
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn non_interactive_history_reads_reject_old_formats_without_changing_the_log() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        for format_version in ["old", "agent-dock/events/v1alpha4"] {
            let original =
                serde_json::to_string(&old_event_with_format(format_version)).unwrap() + "\n";
            fs::write(&path, &original).unwrap();

            assert!(matches!(
                read_event_history_without_migration(&EventLog::new(&path)),
                Err(AppError::IncompatibleEventLog)
            ));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
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
        let history = read_event_history_with(&EventLog::new(&path), &mut ask).unwrap();

        assert!(matches!(
            &history.skipped_lines[0].reason,
            SkippedLineReason::InvalidEvent { .. }
        ));
        assert!(path.exists());
    }

    #[test]
    fn parses_one_based_profile_selections() {
        assert_eq!(parse_selection("", 3, 0), Some(0));
        assert_eq!(parse_selection("", 3, 2), Some(2));
        assert_eq!(parse_selection("1", 3, 2), Some(0));
        assert_eq!(parse_selection(" 3 ", 3, 0), Some(2));
        assert_eq!(parse_selection("0", 3, 0), None);
        assert_eq!(parse_selection("4", 3, 0), None);
        assert_eq!(parse_selection("one", 3, 0), None);
        assert_eq!(parse_selection("", 0, 0), None);
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
    fn renders_only_the_first_six_scorecard_segments_and_hidden_tag_count() {
        let profile = safe_test_profile();
        let score = |segment| SegmentScore {
            segment,
            execution_count: 0,
            excluded_count: 0,
            acceptance: agent_dock::AcceptanceAxis::default(),
            duration: agent_dock::DurationAxis::default(),
            cost: agent_dock::CostAxis::default(),
            reported: agent_dock::ReportedDisclosure::default(),
            origins: agent_dock::OriginCounts::default(),
        };
        let scores = std::iter::once(score(Segment::Overall))
            .chain((0..8).map(|index| score(Segment::Tag(format!("tag-{index}")))))
            .collect();
        let scorecards = vec![agent_dock::ProfileScorecard {
            profile_id: profile.id.clone(),
            scores,
        }];

        let rendered = render_profile_choices(&[profile], &scorecards, 3, Timestamp::now());
        let lines: Vec<_> = rendered[0].lines().collect();

        assert_eq!(lines.len(), 14);
        assert_eq!(lines[1], "     Overall: No history");
        assert_eq!(lines[2], "         CLI-reported: not applicable");
        for (index, tag_index) in (0..5).enumerate() {
            let line_index = 3 + index * 2;
            assert_eq!(
                lines[line_index],
                format!("     Tag \"tag-{tag_index}\": No history")
            );
            assert_eq!(
                lines[line_index + 1],
                "         CLI-reported: not applicable"
            );
        }
        assert_eq!(lines[13], "     ... 3 more tags");
        assert!(lines.iter().all(|line| {
            !line.contains("tag-5") && !line.contains("tag-6") && !line.contains("tag-7")
        }));
    }

    #[test]
    fn renders_all_reported_disclosure_states_in_a_table() {
        let now: Timestamp = "2026-01-04T00:00:00Z".parse().unwrap();
        let latest = Some("2026-01-01T00:00:00Z".parse().unwrap());
        let cases: [(&str, agent_dock::ReportedDisclosure, &str); 6] = [
            (
                "none",
                agent_dock::ReportedDisclosure {
                    brokered_count: 3,
                    reported_count: 0,
                    missing_count: 3,
                    failures: Default::default(),
                    estimated_cost: EstimatedCostSummary::None,
                    latest_started_at: None,
                },
                "CLI-reported (not actual cost): reported 0/3 | none 3 | failures parse 0 / limit 0 / model 0 | est none",
            ),
            (
                "total",
                agent_dock::ReportedDisclosure {
                    brokered_count: 5,
                    reported_count: 2,
                    missing_count: 3,
                    failures: agent_dock::ReportFailureCounts {
                        parse: 1,
                        limit: 0,
                        model: 0,
                    },
                    estimated_cost: EstimatedCostSummary::Total {
                        currency: "USD".to_owned(),
                        amount_micros: 3_000_003,
                        count: 2,
                    },
                    latest_started_at: latest,
                },
                "CLI-reported (not actual cost): reported 2/5 | none 3 | failures parse 1 / limit 0 / model 0 | est USD 3.000003 (2) | latest 3d ago",
            ),
            (
                "usage only",
                agent_dock::ReportedDisclosure {
                    brokered_count: 1,
                    reported_count: 1,
                    missing_count: 0,
                    failures: Default::default(),
                    estimated_cost: EstimatedCostSummary::None,
                    latest_started_at: latest,
                },
                "CLI-reported (not actual cost): reported 1/1 | none 0 | failures parse 0 / limit 0 / model 0 | est none | latest 3d ago",
            ),
            (
                "mixed currencies",
                agent_dock::ReportedDisclosure {
                    brokered_count: 2,
                    reported_count: 2,
                    missing_count: 0,
                    failures: Default::default(),
                    estimated_cost: EstimatedCostSummary::MixedCurrencies { count: 2 },
                    latest_started_at: latest,
                },
                "CLI-reported (not actual cost): reported 2/2 | none 0 | failures parse 0 / limit 0 / model 0 | est mixed currencies (2) | latest 3d ago",
            ),
            (
                "overflow",
                agent_dock::ReportedDisclosure {
                    brokered_count: 2,
                    reported_count: 2,
                    missing_count: 0,
                    failures: Default::default(),
                    estimated_cost: EstimatedCostSummary::Overflow,
                    latest_started_at: latest,
                },
                "CLI-reported (not actual cost): reported 2/2 | none 0 | failures parse 0 / limit 0 / model 0 | est overflow | latest 3d ago",
            ),
            (
                "not applicable",
                agent_dock::ReportedDisclosure::default(),
                "CLI-reported: not applicable",
            ),
        ];
        for (case, disclosure, expected) in cases {
            assert_eq!(
                render_reported_disclosure(&disclosure, now),
                expected,
                "case={case}"
            );
        }

        let escaped = agent_dock::ReportedDisclosure {
            brokered_count: 1,
            reported_count: 1,
            missing_count: 0,
            failures: Default::default(),
            estimated_cost: EstimatedCostSummary::Total {
                currency: "\x1b[31mUSD\\".to_owned(),
                amount_micros: 1,
                count: 1,
            },
            latest_started_at: None,
        };
        let rendered = render_reported_disclosure(&escaped, now);
        assert!(rendered.contains(r"est \u{1b}[31mUSD\\ 0.000001 (1)"));
        assert!(!rendered.contains('\x1b'));
    }

    #[test]
    fn advises_from_all_tag_segments_in_event_log() {
        let directory = tempfile::tempdir().unwrap();
        let log = EventLog::new(directory.path().join("events.jsonl"));
        let profile = safe_test_profile();
        let tags: Vec<_> = (0..7).map(|index| format!("tag-{index}")).collect();

        for index in 0..3 {
            let task_id = Uuid::now_v7();
            log.append(&task_received_event(
                task_id,
                tags.clone(),
                &format!("tagged task {index}"),
                false,
                false,
                Timestamp::now(),
            ))
            .unwrap();
            log.append(&Event::new(
                task_id,
                EventKind::Executed {
                    origin: ExecutionOrigin::Brokered,
                    profile: ProfileSnapshot::from(&profile),
                    working_directory: PathBuf::from("/tmp"),
                    started_at: Timestamp::now(),
                    elapsed_ms: 100,
                    outcome: RecordedExecutionOutcome::Completed { exit_code: Some(0) },
                    cost: None,
                    estimated_cost: None,
                    usage: None,
                    report_failure: None,
                },
            ))
            .unwrap();
            log.append(&Event::new(
                task_id,
                EventKind::Judged {
                    verdict: Verdict::Accepted,
                },
            ))
            .unwrap();
        }

        let history = log.read_all().unwrap();
        let profiles = [profile.clone()];
        let (segments, projection, advice) =
            prepare_advice_for_task(&history.events, &tags, &profiles, &profiles);

        assert_eq!(segments.len(), 8);
        assert_eq!(projection.scorecards[0].scores.len(), 8);
        let AdviceOutcome::Proposed {
            profile_id,
            evidence,
        } = advice.outcome
        else {
            panic!("enough evidence across all tags should produce advice");
        };
        assert_eq!(profile_id, profile.id);
        assert_eq!(evidence.len(), 7);
        for (index, evidence) in evidence.iter().enumerate().skip(5).take(2) {
            assert_eq!(
                evidence.segment,
                EvidenceSegment::Tag(format!("tag-{index}"))
            );
            assert_eq!(evidence.acceptance.summary.count, 3);
        }
    }

    #[test]
    fn escapes_terminal_controls_in_advice_display_fields() {
        let mut profile = safe_test_profile();
        profile.declaration.name = "\x1b[31mcrew".to_owned();
        profile.id = "\x1b[31mprofile-id".to_owned();
        let evidence = |segment, currency| AdviceEvidence {
            segment,
            acceptance: EvidenceAcceptance {
                summary: agent_dock::EvidenceAxisSummary {
                    count: 0,
                    latest_started_at: None,
                },
                accepted: 0,
            },
            duration: EvidenceDuration {
                summary: agent_dock::EvidenceAxisSummary {
                    count: 0,
                    latest_started_at: None,
                },
                median_ms: None,
            },
            cost: EvidenceCost {
                summary: agent_dock::EvidenceAxisSummary {
                    count: 1,
                    latest_started_at: None,
                },
                currency,
                total_minor_units: Some(42),
                missing_count: 0,
            },
        };
        let score = SegmentScore {
            segment: Segment::Tag("scorecard".to_owned()),
            execution_count: 1,
            excluded_count: 0,
            acceptance: agent_dock::AcceptanceAxis::default(),
            duration: agent_dock::DurationAxis::default(),
            cost: agent_dock::CostAxis {
                summary: agent_dock::AxisSummary {
                    count: 1,
                    latest_started_at: None,
                },
                currency: Some("\x1b[31mEUR".to_owned()),
                total_minor_units: Some(42),
                missing_count: 0,
            },
            reported: agent_dock::ReportedDisclosure::default(),
            origins: agent_dock::OriginCounts::default(),
        };
        let cases = [
            (
                "profile name",
                render_advice_proposal(&profile.id, &[profile.clone()]),
                r"\u{1b}[31mcrew",
            ),
            (
                "profile id",
                render_advice_proposal(&profile.id, &[profile.clone()]),
                r"\u{1b}[31mprofile-id",
            ),
            (
                "tag",
                render_advice_evidence(
                    &evidence(EvidenceSegment::Tag("\x1b[31mtag".to_owned()), None),
                    Timestamp::now(),
                ),
                r"\u{1b}[31mtag",
            ),
            (
                "advice currency",
                render_advice_evidence(
                    &evidence(EvidenceSegment::Untagged, Some("\x1b[31mUSD".to_owned())),
                    Timestamp::now(),
                ),
                r"\u{1b}[31mUSD",
            ),
            (
                "scorecard currency",
                render_segment(&score, Timestamp::now()),
                r"\u{1b}[31mEUR",
            ),
        ];

        for (case, rendered, expected) in cases {
            assert!(rendered.contains(expected), "case={case}: {rendered}");
            assert!(!rendered.contains('\x1b'), "case={case}: {rendered}");
        }
    }

    #[test]
    fn renders_quoted_tag_names_and_missing_cost() {
        let score = SegmentScore {
            segment: Segment::Tag("review: \"strict\"".to_owned()),
            execution_count: 1,
            excluded_count: 0,
            acceptance: agent_dock::AcceptanceAxis::default(),
            duration: agent_dock::DurationAxis::default(),
            cost: agent_dock::CostAxis::default(),
            reported: agent_dock::ReportedDisclosure::default(),
            origins: agent_dock::OriginCounts::default(),
        };

        assert_eq!(
            render_segment(&score, Timestamp::now()),
            "Tag \"review: \\\"strict\\\"\": Acceptance - (0/0) | Duration - (0) | Cost missing (0)"
        );
    }

    #[test]
    fn resolves_only_executable_files() {
        let profile = ProfileDeclaration {
            name: "Missing".to_owned(),
            cli: agent_dock::CliKind::Claude,
            execution_platform: ExecutionPlatform::Headless,
            executable: Some(std::path::PathBuf::from("/definitely/not/installed")),
            model: None,
            args: Vec::new(),
            identity: Default::default(),
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
}

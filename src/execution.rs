use std::{
    collections::BTreeMap,
    fmt, io,
    os::unix::process::CommandExt,
    process::ExitStatus,
    time::{Duration, Instant},
};

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use serde_json::value::RawValue;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::timeout,
};

use crate::{CliKind, EstimatedCost, ExecutionProfile, PromptTransport, ReportFailure, TokenUsage};

const TERMINATION_GRACE_PERIOD: Duration = Duration::from_secs(3);
pub const MAX_EXECUTION_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_CLI_VERSION_BYTES: usize = 256;
const CLI_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const CLI_VERSION_CAPTURE_BYTES: usize = 4096;
const RAW_OUTPUT_FORWARD_CHUNK_BYTES: usize = 8 * 1024;
const RAW_OUTPUT_FORWARD_MAX_PENDING_BYTES: usize = RAW_OUTPUT_FORWARD_CHUNK_BYTES + 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRequest {
    pub prompt: String,
    pub working_directory: std::path::PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSource {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionEvent {
    Started { process_id: u32 },
    Output { source: OutputSource, line: String },
    Warning { message: String },
    Report { report: ExecutionReport },
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionReport {
    pub estimated_cost: Option<EstimatedCost>,
    pub usage: Option<TokenUsage>,
    pub report_failure: Option<ReportFailure>,
    pub observed_models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Completed {
        exit_code: Option<i32>,
        elapsed: Duration,
    },
    Cancelled {
        elapsed: Duration,
    },
}

pub async fn observe_cli_version(profile: &ExecutionProfile) -> Option<String> {
    if profile.cli() == CliKind::Test {
        return None;
    }

    let mut command = Command::new(&profile.resolved_executable);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().ok()?;
    let process_id = child.id();
    let stdout = child.stdout.take()?;
    let reader = tokio::spawn(read_cli_version_stdout(stdout));
    let result = timeout(CLI_VERSION_TIMEOUT, async {
        let status = child.wait().await.map_err(|_| ())?;
        let bytes = reader.await.map_err(|_| ())?.map_err(|_| ())?;
        Ok::<_, ()>((status, bytes))
    })
    .await;
    let (status, bytes) = match result {
        Ok(Ok(result)) => result,
        Ok(Err(())) | Err(_) => {
            if let Some(process_id) = process_id {
                let _ = signal_process_group(process_id, libc::SIGKILL);
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    let output = String::from_utf8(bytes).ok()?;
    let output = output.trim();
    let length = utf8_prefix_len(output.as_bytes(), MAX_CLI_VERSION_BYTES);
    Some(output[..length].to_owned()).filter(|output| !output.is_empty())
}

async fn read_cli_version_stdout(
    mut reader: impl tokio::io::AsyncRead + Unpin,
) -> io::Result<Vec<u8>> {
    let mut first_line = Vec::new();
    let mut first_line_complete = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if first_line_complete {
            continue;
        }
        let chunk = &buffer[..read];
        let line_length = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(chunk.len());
        let remaining = CLI_VERSION_CAPTURE_BYTES.saturating_sub(first_line.len());
        first_line.extend_from_slice(&chunk[..line_length.min(remaining)]);
        if line_length < chunk.len() {
            first_line_complete = true;
        }
    }
    Ok(first_line)
}

impl ExecutionOutcome {
    pub fn process_exit_code(&self) -> i32 {
        match self {
            Self::Completed {
                exit_code: Some(code),
                ..
            } => *code,
            Self::Completed {
                exit_code: None, ..
            } => 1,
            Self::Cancelled { .. } => 130,
        }
    }
}

pub async fn execute(
    profile: &ExecutionProfile,
    request: ExecutionRequest,
    cancellation: oneshot::Receiver<()>,
    mut on_event: impl FnMut(ExecutionEvent),
) -> Result<ExecutionOutcome, ExecutionError> {
    let spec = profile.command_spec(request.working_directory);
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.working_directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let stdin_prompt = match spec.prompt_transport {
        PromptTransport::Stdin => {
            command.stdin(std::process::Stdio::piped());
            Some(request.prompt)
        }
        PromptTransport::Argument => {
            command
                .arg(request.prompt)
                .stdin(std::process::Stdio::null());
            None
        }
    };
    command.as_std_mut().process_group(0);

    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| ExecutionError::Spawn {
        program: spec.program,
        source,
    })?;
    let process_id = child.id().ok_or(ExecutionError::MissingProcessId)?;
    on_event(ExecutionEvent::Started { process_id });

    let stdin_task = if let Some(prompt) = stdin_prompt {
        let mut stdin = child
            .stdin
            .take()
            .ok_or(ExecutionError::MissingPipe("stdin"))?;
        Some(tokio::spawn(async move {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await
        }))
    } else {
        None
    };

    let stdout = child
        .stdout
        .take()
        .ok_or(ExecutionError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ExecutionError::MissingPipe("stderr"))?;
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(process_stdout(stdout, profile.cli(), sender.clone()));
    let stderr_task = tokio::spawn(forward_lines(stderr, OutputSource::Stderr, sender.clone()));

    let manager = tokio::spawn(manage_child(child, process_id, cancellation));
    tokio::pin!(manager);
    let outcome = loop {
        tokio::select! {
            Some(event) = receiver.recv() => on_event(event),
            result = &mut manager => {
                break result.map_err(ExecutionError::Task)??;
            }
        }
    };

    if let Some(stdin_task) = stdin_task
        && let Err(source) = stdin_task.await.map_err(ExecutionError::Task)?
        && source.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(ExecutionError::Io(source));
    }
    let stdout_result = stdout_task.await.map_err(ExecutionError::Task)??;
    let report = match stdout_result {
        StdoutProcessing::Lines(report) => report,
        StdoutProcessing::Single {
            bytes,
            exceeded,
            raw_output_forwarded,
        } => process_single_output(
            &bytes,
            exceeded,
            raw_output_forwarded,
            profile.cli(),
            profile.model(),
            &sender,
        ),
    };
    stderr_task.await.map_err(ExecutionError::Task)??;
    while let Ok(event) = receiver.try_recv() {
        on_event(event);
    }
    on_event(ExecutionEvent::Report { report });
    on_event(ExecutionEvent::Finished);

    Ok(match outcome {
        ManagedOutcome::Completed(status) => ExecutionOutcome::Completed {
            exit_code: status.code(),
            elapsed: started.elapsed(),
        },
        ManagedOutcome::Cancelled => ExecutionOutcome::Cancelled {
            elapsed: started.elapsed(),
        },
    })
}

async fn forward_lines(
    reader: impl tokio::io::AsyncRead + Unpin,
    source: OutputSource,
    sender: mpsc::UnboundedSender<ExecutionEvent>,
) -> io::Result<()> {
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if sender
            .send(ExecutionEvent::Output { source, line })
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

enum StdoutProcessing {
    Lines(ExecutionReport),
    Single {
        bytes: Vec<u8>,
        exceeded: bool,
        raw_output_forwarded: bool,
    },
}

async fn process_stdout(
    reader: impl tokio::io::AsyncRead + Unpin,
    cli: CliKind,
    sender: mpsc::UnboundedSender<ExecutionEvent>,
) -> io::Result<StdoutProcessing> {
    match cli {
        CliKind::Test => {
            forward_lines(reader, OutputSource::Stdout, sender).await?;
            Ok(StdoutProcessing::Lines(ExecutionReport::default()))
        }
        CliKind::Codex => Ok(StdoutProcessing::Lines(
            forward_codex_lines(reader, sender).await?,
        )),
        CliKind::Claude | CliKind::Gemini | CliKind::Antigravity => {
            let limited = read_limited(reader, &sender).await?;
            Ok(StdoutProcessing::Single {
                bytes: limited.bytes,
                exceeded: limited.exceeded,
                raw_output_forwarded: limited.raw_output_forwarded,
            })
        }
    }
}

struct LimitedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
    raw_output_forwarded: bool,
}

#[derive(Default)]
struct RawOutputForwarder {
    pending: Vec<u8>,
    scanned_through: usize,
}

impl RawOutputForwarder {
    fn push(&mut self, bytes: &[u8], sender: &mpsc::UnboundedSender<ExecutionEvent>) {
        let mut offset = 0;
        while offset < bytes.len() {
            if self.pending.len() >= RAW_OUTPUT_FORWARD_MAX_PENDING_BYTES {
                self.flush(sender, false);
            }
            let room = RAW_OUTPUT_FORWARD_MAX_PENDING_BYTES
                .saturating_sub(self.pending.len())
                .max(1);
            let amount = (bytes.len() - offset).min(room);
            self.pending
                .extend_from_slice(&bytes[offset..offset + amount]);
            offset += amount;
            debug_assert!(self.pending.len() <= RAW_OUTPUT_FORWARD_MAX_PENDING_BYTES);
            self.flush(sender, false);
        }
    }

    fn finish(&mut self, sender: &mpsc::UnboundedSender<ExecutionEvent>) {
        self.flush(sender, true);
    }

    fn flush(&mut self, sender: &mpsc::UnboundedSender<ExecutionEvent>, flush_incomplete: bool) {
        let mut consumed = 0;
        loop {
            let Some(relative_newline) = self.pending[self.scanned_through..]
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                self.scanned_through = self.pending.len();
                break;
            };
            let newline = self.scanned_through + relative_newline;
            let line = String::from_utf8_lossy(&self.pending[consumed..newline]).into_owned();
            send_output_line(sender, line.strip_suffix('\r').unwrap_or(&line).to_owned());
            consumed = newline + 1;
            self.scanned_through = consumed;
        }

        if flush_incomplete {
            if consumed < self.pending.len() {
                let line = String::from_utf8_lossy(&self.pending[consumed..]).into_owned();
                send_output_line(sender, line.strip_suffix('\r').unwrap_or(&line).to_owned());
                consumed = self.pending.len();
            }
        } else {
            while self.pending.len().saturating_sub(consumed) >= RAW_OUTPUT_FORWARD_CHUNK_BYTES {
                let chunk = &self.pending[consumed..];
                let chunk_len = utf8_prefix_len(chunk, RAW_OUTPUT_FORWARD_CHUNK_BYTES);
                if chunk_len == 0 {
                    break;
                }
                send_output_chunk(sender, &chunk[..chunk_len]);
                consumed += chunk_len;
            }
        }

        if consumed > 0 {
            self.pending.drain(..consumed);
            self.scanned_through = self.scanned_through.saturating_sub(consumed);
        }
    }
}

fn utf8_prefix_len(bytes: &[u8], limit: usize) -> usize {
    let candidate_len = bytes.len().min(limit);
    match std::str::from_utf8(&bytes[..candidate_len]) {
        Ok(_) => candidate_len,
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(error) => error.valid_up_to() + error.error_len().unwrap_or_default(),
    }
}

async fn read_limited(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    sender: &mpsc::UnboundedSender<ExecutionEvent>,
) -> io::Result<LimitedOutput> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut exceeded = false;
    let mut raw_output_forwarded = false;
    let mut raw_output = RawOutputForwarder::default();
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if !exceeded && output.len() < MAX_EXECUTION_OUTPUT_BYTES {
            let retained = read.min(MAX_EXECUTION_OUTPUT_BYTES - output.len());
            output.extend_from_slice(&buffer[..retained]);
            if retained != read {
                exceeded = true;
                raw_output.push(&output, sender);
                output.clear();
                raw_output_forwarded = true;
                raw_output.push(&buffer[retained..read], sender);
            }
        } else {
            if !exceeded {
                exceeded = true;
                raw_output.push(&output, sender);
                output.clear();
                raw_output_forwarded = true;
            }
            raw_output.push(&buffer[..read], sender);
        }
    }
    if exceeded {
        raw_output.finish(sender);
    }
    Ok(LimitedOutput {
        bytes: output,
        exceeded,
        raw_output_forwarded,
    })
}

async fn forward_codex_lines(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    sender: mpsc::UnboundedSender<ExecutionEvent>,
) -> io::Result<ExecutionReport> {
    let mut line = Vec::with_capacity(MAX_EXECUTION_OUTPUT_BYTES);
    let mut report = CodexReportAccumulator::default();
    let mut raw_output: Option<RawOutputForwarder> = None;
    let mut buffer = [0_u8; RAW_OUTPUT_FORWARD_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        if let Some(raw_output) = raw_output.as_mut() {
            raw_output.push(&buffer[..read], &sender);
            continue;
        }

        let mut offset = 0;
        while offset < read {
            let remaining = &buffer[offset..read];
            let line_end = remaining
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1)
                .unwrap_or(remaining.len());
            line.extend_from_slice(&remaining[..line_end]);
            if trim_line_ending(&line).len() > MAX_EXECUTION_OUTPUT_BYTES {
                report.mark_output_limit_exceeded();
                send_warning(
                    &sender,
                    "Codex JSONL line exceeded the 1 MiB retention limit; using raw output",
                );
                let mut forwarder = RawOutputForwarder::default();
                forwarder.push(&line, &sender);
                line.clear();
                forwarder.push(&remaining[line_end..], &sender);
                raw_output = Some(forwarder);
                break;
            }

            offset += line_end;
            if remaining[..line_end].last() == Some(&b'\n') {
                let line_bytes = trim_line_ending(&line);
                process_codex_line(line_bytes, &mut report, &sender);
                line.clear();
            }
        }
    }
    if let Some(mut raw_output) = raw_output {
        raw_output.finish(&sender);
    } else if !line.is_empty() {
        let line = trim_line_ending(&line);
        process_codex_line(line, &mut report, &sender);
    }
    Ok(report.finish())
}

fn trim_line_ending(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n")
        .map_or(line, |line| line.strip_suffix(b"\r").unwrap_or(line))
}

fn process_single_output(
    bytes: &[u8],
    exceeded: bool,
    raw_output_forwarded: bool,
    cli: CliKind,
    model: Option<&str>,
    sender: &mpsc::UnboundedSender<ExecutionEvent>,
) -> ExecutionReport {
    if exceeded {
        send_warning(
            sender,
            "standard output exceeded the 1 MiB retention limit; using raw output",
        );
        if !raw_output_forwarded {
            send_raw_output(bytes, sender);
        }
        return ExecutionReport {
            report_failure: Some(ReportFailure::OutputLimitExceeded),
            ..ExecutionReport::default()
        };
    }

    let parsed = match cli {
        CliKind::Claude => parse_claude_output(bytes),
        CliKind::Gemini => parse_gemini_output(bytes, model),
        CliKind::Antigravity => parse_antigravity_output(bytes),
        CliKind::Test | CliKind::Codex => unreachable!("single-output parser used for JSON CLI"),
    };
    match parsed {
        Ok(parsed) => {
            for warning in parsed.warnings {
                send_warning(sender, warning);
            }
            if let Some(response) = parsed.response {
                send_text_output(&response, sender);
            } else {
                send_raw_output(bytes, sender);
            }
            parsed.report
        }
        Err(message) => {
            send_warning(sender, message);
            send_raw_output(bytes, sender);
            ExecutionReport {
                report_failure: Some(ReportFailure::ParseFailure),
                ..ExecutionReport::default()
            }
        }
    }
}

struct ParsedSingleOutput {
    response: Option<String>,
    report: ExecutionReport,
    warnings: Vec<String>,
}

fn parse_claude_output(bytes: &[u8]) -> Result<ParsedSingleOutput, String> {
    let document = serde_json::from_slice::<ClaudeOutput>(bytes)
        .map_err(|error| format!("could not parse Claude JSON output: {error}"))?;
    let observed_models = document.model_usage.clone().unwrap_or_default();
    let mut warnings = Vec::new();
    let mut report_failure = None;
    let estimated_cost = match document.total_cost_usd {
        None => None,
        Some(None) => {
            report_failure = Some(ReportFailure::ParseFailure);
            warnings.push("Claude total_cost_usd was not recorded: value is null".to_owned());
            None
        }
        Some(Some(raw)) => match decimal_micros(raw.get()) {
            Ok(amount_micros) => Some(EstimatedCost {
                currency: "USD".to_owned(),
                amount_micros,
            }),
            Err(reason) => {
                report_failure = Some(ReportFailure::ParseFailure);
                warnings.push(format!("Claude total_cost_usd was not recorded: {reason}"));
                None
            }
        },
    };
    let usage = document.usage.map(|usage| TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cache_read_input_tokens,
        reasoning_tokens: None,
    });
    let response = match document.result {
        Some(response) => Some(response),
        None if document.is_error == Some(true) => {
            warnings.push(
                "Claude JSON error output has no string `result`; using raw output".to_owned(),
            );
            None
        }
        None => return Err("Claude JSON output has no string `result` response".to_owned()),
    };
    Ok(ParsedSingleOutput {
        response,
        report: ExecutionReport {
            estimated_cost,
            usage,
            report_failure,
            observed_models,
        },
        warnings,
    })
}

fn parse_gemini_output(
    bytes: &[u8],
    profile_model: Option<&str>,
) -> Result<ParsedSingleOutput, String> {
    let document = serde_json::from_slice::<GeminiOutput>(bytes)
        .map_err(|error| format!("could not parse Gemini JSON output: {error}"))?;
    let Some(response) = document.response else {
        return Err("Gemini JSON output has no string `response` response".to_owned());
    };
    let mut report = ExecutionReport::default();
    let mut warnings = Vec::new();
    let models = document
        .stats
        .as_ref()
        .and_then(|stats| stats.models.as_ref());
    let observed_models = models.map_or_else(Vec::new, |models| models.names.clone());
    if let Some(models) = models {
        if models.names.len() == 1 {
            let model_name = &models.names[0];
            let model = models
                .values
                .get(model_name)
                .expect("a one-entry model map must have an entry");
            if profile_model.is_none() || profile_model == Some(model_name.as_str()) {
                report.usage = Some(TokenUsage {
                    input_tokens: model.tokens.as_ref().and_then(|tokens| tokens.prompt),
                    output_tokens: model.tokens.as_ref().and_then(|tokens| tokens.candidates),
                    cached_input_tokens: model.tokens.as_ref().and_then(|tokens| tokens.cached),
                    reasoning_tokens: model.tokens.as_ref().and_then(|tokens| tokens.thoughts),
                });
            } else {
                report.report_failure = Some(ReportFailure::ModelMismatch);
                warnings.push(
                    "Gemini usage was not recorded because stats.models did not match the profile model"
                        .to_owned(),
                );
            }
        } else if models.names.len() > 1 {
            report.report_failure = Some(ReportFailure::ModelMismatch);
            warnings.push(
                "Gemini usage was not recorded because stats.models did not identify one model"
                    .to_owned(),
            );
        }
    }
    Ok(ParsedSingleOutput {
        response: Some(response),
        report: ExecutionReport {
            observed_models,
            ..report
        },
        warnings,
    })
}

fn parse_antigravity_output(bytes: &[u8]) -> Result<ParsedSingleOutput, String> {
    let document = serde_json::from_slice::<AntigravityOutput>(bytes)
        .map_err(|error| format!("could not parse Antigravity JSON output: {error}"))?;
    let Some(response) = document.response else {
        return Err("Antigravity JSON output has no string `response` response".to_owned());
    };
    let usage = document.usage.map(|usage| TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cache_read_tokens,
        reasoning_tokens: usage.thinking_tokens,
    });
    Ok(ParsedSingleOutput {
        response: Some(response),
        report: ExecutionReport {
            estimated_cost: None,
            usage,
            report_failure: None,
            observed_models: Vec::new(),
        },
        warnings: Vec::new(),
    })
}

#[derive(Debug, Deserialize)]
struct ClaudeOutput {
    is_error: Option<bool>,
    result: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_raw_value")]
    total_cost_usd: Option<Option<Box<RawValue>>>,
    usage: Option<ClaudeUsage>,
    #[serde(
        rename = "modelUsage",
        default,
        deserialize_with = "deserialize_model_names"
    )]
    model_usage: Option<Vec<String>>,
}

fn deserialize_model_names<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModelNamesVisitor;

    impl<'de> Visitor<'de> for ModelNamesVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a JSON object containing model names")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut names = Vec::new();
            while let Some(name) = access.next_key::<String>()? {
                access.next_value::<IgnoredAny>()?;
                if !names.iter().any(|candidate| candidate == &name) {
                    names.push(name);
                }
            }
            Ok(Some(names))
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }
    }

    deserializer.deserialize_any(ModelNamesVisitor)
}

fn deserialize_optional_raw_value<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Box<RawValue>>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::<Box<RawValue>>::deserialize(deserializer)?))
}

#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GeminiOutput {
    response: Option<String>,
    stats: Option<GeminiStats>,
}

#[derive(Debug, Deserialize)]
struct GeminiStats {
    models: Option<GeminiModels>,
}

#[derive(Debug)]
struct GeminiModels {
    names: Vec<String>,
    values: BTreeMap<String, GeminiModel>,
}

impl<'de> Deserialize<'de> for GeminiModels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct GeminiModelsVisitor;

        impl<'de> Visitor<'de> for GeminiModelsVisitor {
            type Value = GeminiModels;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object containing Gemini model usage")
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut names = Vec::new();
                let mut values = BTreeMap::new();
                while let Some((name, value)) = access.next_entry::<String, GeminiModel>()? {
                    if !names.iter().any(|candidate| candidate == &name) {
                        names.push(name.clone());
                    }
                    values.insert(name, value);
                }
                Ok(GeminiModels { names, values })
            }
        }

        deserializer.deserialize_map(GeminiModelsVisitor)
    }
}

#[derive(Debug, Deserialize)]
struct GeminiModel {
    tokens: Option<GeminiTokens>,
}

#[derive(Debug, Deserialize)]
struct GeminiTokens {
    prompt: Option<u64>,
    candidates: Option<u64>,
    cached: Option<u64>,
    thoughts: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AntigravityOutput {
    response: Option<String>,
    usage: Option<AntigravityUsage>,
}

#[derive(Debug, Deserialize)]
struct AntigravityUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    thinking_tokens: Option<u64>,
}

fn send_text_output(text: &str, sender: &mpsc::UnboundedSender<ExecutionEvent>) {
    if text.is_empty() {
        send_output_line(sender, String::new());
        return;
    }
    for line in text.split_terminator('\n') {
        send_output_line(sender, line.strip_suffix('\r').unwrap_or(line).to_owned());
    }
}

fn send_raw_output(bytes: &[u8], sender: &mpsc::UnboundedSender<ExecutionEvent>) {
    let text = String::from_utf8_lossy(bytes);
    if text.is_empty() {
        send_output_line(sender, String::new());
        return;
    }
    for line in text.split_terminator('\n') {
        send_output_line(sender, line.strip_suffix('\r').unwrap_or(line).to_owned());
    }
}

fn send_output_line(sender: &mpsc::UnboundedSender<ExecutionEvent>, line: String) {
    let _ = sender.send(ExecutionEvent::Output {
        source: OutputSource::Stdout,
        line,
    });
}

fn send_output_chunk(sender: &mpsc::UnboundedSender<ExecutionEvent>, bytes: &[u8]) {
    send_output_line(sender, String::from_utf8_lossy(bytes).into_owned());
}

fn send_warning(sender: &mpsc::UnboundedSender<ExecutionEvent>, message: impl Into<String>) {
    let _ = sender.send(ExecutionEvent::Warning {
        message: message.into(),
    });
}

fn decimal_micros(raw: &str) -> Result<u64, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("value is empty");
    }
    if raw.starts_with('-') {
        return Err("value is negative");
    }
    let (mantissa, exponent) = match raw.find(['e', 'E']) {
        Some(index) => {
            if raw[index + 1..].contains(['e', 'E']) {
                return Err("exponent is malformed");
            }
            let exponent = match parse_decimal_exponent(&raw[index + 1..]) {
                Ok(exponent) => exponent,
                Err("exponent is out of range") if raw[index + 1..].starts_with('-') => i128::MIN,
                Err("exponent is out of range") => i128::MAX,
                Err(reason) => return Err(reason),
            };
            (&raw[..index], exponent)
        }
        None => (raw, 0),
    };
    let (integer, fraction) = match mantissa.split_once('.') {
        Some((integer, fraction)) if !fraction.contains('.') => (integer, fraction),
        Some(_) => return Err("decimal point is malformed"),
        None => (mantissa, ""),
    };
    if integer.is_empty() && fraction.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("value is not a decimal number");
    }
    let mut digits = String::with_capacity(integer.len() + fraction.len());
    digits.push_str(integer);
    digits.push_str(fraction);
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Ok(0);
    }
    if exponent == i128::MIN {
        return Ok(0);
    }
    if exponent == i128::MAX {
        return Err("value is out of range");
    }
    let digits = trimmed.as_bytes();
    let fractional_digits = i128::try_from(fraction.len()).map_err(|_| "value is too large")?;
    let scale = match fractional_digits.checked_sub(exponent) {
        Some(scale) => scale,
        None if exponent.is_negative() => return Ok(0),
        None => return Err("value is out of range"),
    };
    if scale <= 6 {
        let zeroes = 6_i128.checked_sub(scale).ok_or("value is out of range")?;
        let zeroes = usize::try_from(zeroes).map_err(|_| "value is out of range")?;
        if digits.len().saturating_add(zeroes) > 20 {
            return Err("value is out of range");
        }
        let mut value = digits
            .iter()
            .try_fold(0_u64, |value, digit| {
                value
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            })
            .ok_or("value is out of range")?;
        for _ in 0..zeroes {
            value = value.checked_mul(10).ok_or("value is out of range")?;
        }
        return Ok(value);
    }

    let cut = match usize::try_from(scale - 6) {
        Ok(cut) => cut,
        Err(_) if exponent.is_negative() => return Ok(0),
        Err(_) => return Err("value is out of range"),
    };
    if cut >= digits.len() {
        let rounds_up = cut == digits.len() && digits.first().is_some_and(|digit| *digit >= b'5');
        return Ok(u64::from(rounds_up));
    }
    let quotient = digits
        .get(..digits.len() - cut)
        .ok_or("value is malformed")?
        .iter()
        .try_fold(0_u64, |value, digit| {
            value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(digit - b'0')))
        })
        .ok_or("value is out of range")?;
    let rounds_up = digits
        .get(digits.len() - cut)
        .is_some_and(|digit| *digit >= b'5');
    quotient
        .checked_add(u64::from(rounds_up))
        .ok_or("value is out of range")
}

fn parse_decimal_exponent(raw: &str) -> Result<i128, &'static str> {
    if raw.is_empty() {
        return Err("exponent is empty");
    }
    let negative = raw.starts_with('-');
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("exponent is malformed");
    }
    let exponent = digits
        .parse::<i128>()
        .map_err(|_| "exponent is out of range")?;
    Ok(if negative { -exponent } else { exponent })
}

#[derive(Default)]
struct CodexReportAccumulator {
    saw_turn_completed: bool,
    usage_invalid: bool,
    output_limit_exceeded: bool,
    parse_failure: bool,
    missing_fields: [bool; 4],
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

impl CodexReportAccumulator {
    fn finish(self) -> ExecutionReport {
        ExecutionReport {
            estimated_cost: None,
            usage: (self.saw_turn_completed && !self.usage_invalid).then_some(TokenUsage {
                input_tokens: (!self.missing_fields[0])
                    .then_some(self.input_tokens)
                    .flatten(),
                output_tokens: (!self.missing_fields[2])
                    .then_some(self.output_tokens)
                    .flatten(),
                cached_input_tokens: (!self.missing_fields[1])
                    .then_some(self.cached_input_tokens)
                    .flatten(),
                reasoning_tokens: (!self.missing_fields[3])
                    .then_some(self.reasoning_tokens)
                    .flatten(),
            }),
            report_failure: if self.output_limit_exceeded {
                Some(ReportFailure::OutputLimitExceeded)
            } else {
                self.parse_failure.then_some(ReportFailure::ParseFailure)
            },
            observed_models: Vec::new(),
        }
    }

    fn mark_output_limit_exceeded(&mut self) {
        self.output_limit_exceeded = true;
        invalidate_codex_usage(self);
    }
}

fn process_codex_line(
    line: &[u8],
    report: &mut CodexReportAccumulator,
    sender: &mpsc::UnboundedSender<ExecutionEvent>,
) {
    let document = match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(document) => document,
        Err(error) => {
            report.parse_failure = true;
            send_raw_codex_line(line, sender);
            send_warning(sender, format!("could not parse Codex JSONL line: {error}"));
            return;
        }
    };
    let Some(object) = document.as_object() else {
        report.parse_failure = true;
        send_raw_codex_line(line, sender);
        send_warning(sender, "Codex JSONL line is missing its event object");
        return;
    };
    let Some(event_type) = object.get("type").and_then(serde_json::Value::as_str) else {
        report.parse_failure = true;
        send_raw_codex_line(line, sender);
        send_warning(sender, "Codex JSONL line is missing its string `type`");
        return;
    };
    match event_type {
        "item.completed" => {
            let Some(item) = object.get("item").and_then(serde_json::Value::as_object) else {
                report.parse_failure = true;
                send_raw_codex_line(line, sender);
                send_warning(sender, "Codex item.completed line is missing its item");
                return;
            };
            let Some(item_type) = item.get("type").and_then(serde_json::Value::as_str) else {
                report.parse_failure = true;
                send_raw_codex_line(line, sender);
                send_warning(sender, "Codex completed item is missing its string `type`");
                return;
            };
            if item_type == "agent_message" {
                let Some(text) = item.get("text").and_then(serde_json::Value::as_str) else {
                    report.parse_failure = true;
                    send_raw_codex_line(line, sender);
                    send_warning(
                        sender,
                        "Codex agent_message item is missing its string `text`",
                    );
                    return;
                };
                send_text_output(text, sender);
            }
        }
        "turn.completed" => {
            if report.usage_invalid {
                return;
            }
            let Some(usage) = object.get("usage").and_then(serde_json::Value::as_object) else {
                invalidate_codex_usage(report);
                report.parse_failure = true;
                send_raw_codex_line(line, sender);
                send_warning(
                    sender,
                    "Codex turn.completed line is missing its usage object",
                );
                return;
            };
            let fields = [
                "input_tokens",
                "cached_input_tokens",
                "output_tokens",
                "reasoning_output_tokens",
            ];
            let mut values = [None; 4];
            for (index, name) in fields.into_iter().enumerate() {
                match optional_u64(usage.get(name)) {
                    Ok(value) => values[index] = value,
                    Err(error) => {
                        invalidate_codex_usage(report);
                        report.parse_failure = true;
                        send_raw_codex_line(line, sender);
                        send_warning(
                            sender,
                            format!("Codex usage field `{name}` is invalid: {error}"),
                        );
                        return;
                    }
                }
            }
            let mut missing_fields = report.missing_fields;
            let sums = [
                next_codex_usage_value(report.input_tokens, values[0], &mut missing_fields[0]),
                next_codex_usage_value(
                    report.cached_input_tokens,
                    values[1],
                    &mut missing_fields[1],
                ),
                next_codex_usage_value(report.output_tokens, values[2], &mut missing_fields[2]),
                next_codex_usage_value(report.reasoning_tokens, values[3], &mut missing_fields[3]),
            ];
            match sums {
                [
                    Ok(input_tokens),
                    Ok(cached_input_tokens),
                    Ok(output_tokens),
                    Ok(reasoning_tokens),
                ] => {
                    report.saw_turn_completed = true;
                    report.missing_fields = missing_fields;
                    report.input_tokens = input_tokens;
                    report.cached_input_tokens = cached_input_tokens;
                    report.output_tokens = output_tokens;
                    report.reasoning_tokens = reasoning_tokens;
                }
                _ => {
                    invalidate_codex_usage(report);
                    report.parse_failure = true;
                    send_raw_codex_line(line, sender);
                    send_warning(sender, "Codex token usage exceeded u64");
                }
            }
        }
        "error" | "turn.failed" => {
            let message = object
                .get("message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    object
                        .get("error")
                        .and_then(serde_json::Value::as_object)
                        .and_then(|error| error.get("message"))
                        .and_then(serde_json::Value::as_str)
                });
            if let Some(message) = message {
                send_warning(sender, format!("Codex {event_type}: {message}"));
            } else {
                send_warning(sender, format!("Codex {event_type}"));
            }
        }
        "thread.started" | "turn.started" | "item.started" => {}
        _ => {}
    }
}

fn invalidate_codex_usage(report: &mut CodexReportAccumulator) {
    report.usage_invalid = true;
    report.input_tokens = None;
    report.cached_input_tokens = None;
    report.output_tokens = None;
    report.reasoning_tokens = None;
}

fn next_codex_usage_value(
    total: Option<u64>,
    value: Option<u64>,
    missing: &mut bool,
) -> Result<Option<u64>, ()> {
    if *missing {
        return Ok(None);
    }
    let Some(value) = value else {
        *missing = true;
        return Ok(None);
    };
    match total {
        Some(total) => total.checked_add(value).map(Some).ok_or(()),
        None => Ok(Some(value)),
    }
}

fn optional_u64(value: Option<&serde_json::Value>) -> Result<Option<u64>, &'static str> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or("not a non-negative integer"),
        Some(_) => Err("not a non-negative integer"),
    }
}

fn send_raw_codex_line(line: &[u8], sender: &mpsc::UnboundedSender<ExecutionEvent>) {
    send_output_line(sender, String::from_utf8_lossy(line).into_owned());
}

enum ManagedOutcome {
    Completed(ExitStatus),
    Cancelled,
}

async fn manage_child(
    mut child: Child,
    process_id: u32,
    cancellation: oneshot::Receiver<()>,
) -> Result<ManagedOutcome, ExecutionError> {
    let cancellation = cancellation_requested(cancellation);
    tokio::pin!(cancellation);
    tokio::select! {
        status = child.wait() => Ok(ManagedOutcome::Completed(status?)),
        () = &mut cancellation => {
            signal_process_group(process_id, libc::SIGINT)?;
            if timeout(TERMINATION_GRACE_PERIOD, child.wait()).await.is_err() {
                signal_process_group(process_id, libc::SIGKILL)?;
                child.wait().await?;
            }
            Ok(ManagedOutcome::Cancelled)
        }
    }
}

async fn cancellation_requested(receiver: oneshot::Receiver<()>) {
    if receiver.await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn signal_process_group(process_id: u32, signal: libc::c_int) -> Result<(), ExecutionError> {
    // SAFETY: `kill` receives a valid signal and a negated child PID, which targets
    // the process group created immediately before spawning this child.
    let result = unsafe { libc::kill(-(process_id as libc::pid_t), signal) };
    if result == 0 {
        Ok(())
    } else {
        let source = io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(ExecutionError::Signal { process_id, source })
        }
    }
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("could not start `{}`: {source}", program.display())]
    Spawn {
        program: std::path::PathBuf,
        source: io::Error,
    },
    #[error("child process did not expose a process id")]
    MissingProcessId,
    #[error("child process did not expose its {0} pipe")]
    MissingPipe(&'static str),
    #[error("child process I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("background task failed: {0}")]
    Task(tokio::task::JoinError),
    #[error("could not signal child process group {process_id}: {source}")]
    Signal { process_id: u32, source: io::Error },
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use super::*;
    use crate::{CliKind, ProfileDeclaration, safe_test_profile};

    fn write_script(directory: &Path, source: &str) -> PathBuf {
        let path = directory.join("fake-agent");
        fs::write(&path, source).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn profile(executable: PathBuf) -> ExecutionProfile {
        ProfileDeclaration {
            name: "Fake agent".to_owned(),
            cli: CliKind::Claude,
            execution_platform: crate::ExecutionPlatform::Headless,
            executable: Some(executable.clone()),
            model: None,
            args: Vec::new(),
            identity: Default::default(),
        }
        .resolve(executable)
        .unwrap()
    }

    fn antigravity_profile(executable: PathBuf) -> ExecutionProfile {
        ProfileDeclaration {
            name: "Fake agent".to_owned(),
            cli: CliKind::Antigravity,
            execution_platform: crate::ExecutionPlatform::Headless,
            executable: Some(executable.clone()),
            model: None,
            args: Vec::new(),
            identity: Default::default(),
        }
        .resolve(executable)
        .unwrap()
    }

    fn test_profile(executable: PathBuf) -> ExecutionProfile {
        ProfileDeclaration {
            name: "Test agent".to_owned(),
            cli: CliKind::Test,
            execution_platform: crate::ExecutionPlatform::Headless,
            executable: Some(executable.clone()),
            model: None,
            args: Vec::new(),
            identity: Default::default(),
        }
        .resolve(executable)
        .unwrap()
    }

    fn cli_profile(executable: PathBuf, cli: CliKind, model: Option<&str>) -> ExecutionProfile {
        ProfileDeclaration {
            name: "Fixture agent".to_owned(),
            cli,
            execution_platform: crate::ExecutionPlatform::Headless,
            executable: Some(executable.clone()),
            model: model.map(str::to_owned),
            args: Vec::new(),
            identity: Default::default(),
        }
        .resolve(executable)
        .unwrap()
    }

    struct ChunkedReader {
        chunks: Vec<Vec<u8>>,
        next: usize,
        offset: usize,
    }

    impl tokio::io::AsyncRead for ChunkedReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let reader = self.get_mut();
            if let Some(chunk) = reader.chunks.get(reader.next) {
                let remaining = &chunk[reader.offset..];
                let amount = remaining.len().min(buffer.remaining());
                buffer.put_slice(&remaining[..amount]);
                reader.offset += amount;
                if reader.offset == chunk.len() {
                    reader.next += 1;
                    reader.offset = 0;
                }
            }
            Poll::Ready(Ok(()))
        }
    }

    fn single_output_events(
        bytes: &[u8],
        exceeded: bool,
        raw_output_forwarded: bool,
        cli: CliKind,
        model: Option<&str>,
    ) -> (Vec<ExecutionEvent>, ExecutionReport) {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let report =
            process_single_output(bytes, exceeded, raw_output_forwarded, cli, model, &sender);
        drop(sender);
        let events = drain_events(&mut receiver);
        (events, report)
    }

    fn drain_events(receiver: &mut mpsc::UnboundedReceiver<ExecutionEvent>) -> Vec<ExecutionEvent> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    fn event_output_lines(events: &[ExecutionEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                ExecutionEvent::Output {
                    source: OutputSource::Stdout,
                    line,
                } => Some(line.as_str()),
                _ => None,
            })
            .collect()
    }

    fn event_warnings(events: &[ExecutionEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                ExecutionEvent::Warning { message } => Some(message.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn streams_stdin_stdout_and_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\nIFS= read -r prompt\nprintf 'received:%s\\n' \"$prompt\"\nprintf 'warning\\n' >&2\nexit 7\n",
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_, cancellation) = oneshot::channel();

        let outcome = execute(
            &profile(executable),
            ExecutionRequest {
                prompt: "hello from dock".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(7),
                ..
            }
        ));
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Output { source: OutputSource::Stdout, line }
                if line == "received:hello from dock"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Output { source: OutputSource::Stderr, line }
                if line == "warning"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report }
                if report.report_failure == Some(crate::ReportFailure::ParseFailure)
        )));
        assert!(matches!(
            events.first(),
            Some(ExecutionEvent::Started { .. })
        ));
        assert_eq!(events.last(), Some(&ExecutionEvent::Finished));
    }

    #[tokio::test]
    async fn parse_failure_does_not_change_a_completed_execution_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\nprintf 'not-json\\n'\nexit 7\n",
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_cancellation_sender, cancellation) = oneshot::channel();

        let outcome = execute(
            &profile(executable),
            ExecutionRequest {
                prompt: "outcome".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        let ExecutionOutcome::Completed { exit_code, elapsed } = outcome else {
            panic!("a parse failure must not change the process outcome");
        };
        assert_eq!(exit_code, Some(7));
        assert!(elapsed > Duration::ZERO);
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report }
                if report.report_failure == Some(crate::ReportFailure::ParseFailure)
        )));
    }

    #[tokio::test]
    async fn safe_test_profile_completes_without_starting_an_agent() {
        let directory = tempfile::tempdir().unwrap();
        let (_cancellation_sender, cancellation) = oneshot::channel();

        let outcome = execute(
            &safe_test_profile(),
            ExecutionRequest {
                prompt: "this must not reach an agent".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            |_| {},
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(0),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn observes_the_first_trimmed_version_line_before_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\nprintf ' \tfixture-cli 1.2.3  \r\nignored\\n'\n",
        );

        assert_eq!(
            observe_cli_version(&profile(executable)).await,
            Some("fixture-cli 1.2.3".to_owned())
        );
    }

    #[tokio::test]
    async fn treats_version_spawn_failure_and_empty_output_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = profile(directory.path().join("missing-agent"));
        assert_eq!(observe_cli_version(&missing).await, None);

        let empty = write_script(directory.path(), "#!/bin/sh\nexit 0\n");
        assert_eq!(observe_cli_version(&profile(empty)).await, None);
    }

    #[tokio::test]
    async fn treats_nonzero_version_exit_and_invalid_utf8_output_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let nonzero = write_script(
            directory.path(),
            "#!/bin/sh\nprintf 'fixture-cli 2.0\\n'\nexit 7\n",
        );
        assert_eq!(observe_cli_version(&profile(nonzero)).await, None);

        let invalid_utf8 = write_script(directory.path(), "#!/bin/sh\nprintf '\\377\\n'\n");
        assert_eq!(observe_cli_version(&profile(invalid_utf8)).await, None);
    }

    #[tokio::test]
    async fn times_out_version_observation_without_blocking_forever() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(directory.path(), "#!/bin/sh\nwhile :; do sleep 1; done\n");

        let started = Instant::now();
        assert_eq!(observe_cli_version(&profile(executable)).await, None);
        assert!(started.elapsed() >= CLI_VERSION_TIMEOUT);
    }

    #[tokio::test]
    async fn does_not_observe_a_version_for_the_test_adapter() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("invoked");
        let executable = write_script(
            directory.path(),
            &format!(
                "#!/bin/sh\ntouch {}\nprintf 'fixture\\n'\n",
                marker.display()
            ),
        );

        assert_eq!(observe_cli_version(&test_profile(executable)).await, None);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn truncates_a_long_observed_version_without_splitting_utf8() {
        let directory = tempfile::tempdir().unwrap();
        let version = "版".repeat(MAX_CLI_VERSION_BYTES);
        let executable = write_script(
            directory.path(),
            &format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
        );

        let observed = observe_cli_version(&profile(executable)).await.unwrap();

        assert_eq!(observed.len(), MAX_CLI_VERSION_BYTES - 1);
        assert_eq!(observed, "版".repeat(MAX_CLI_VERSION_BYTES / "版".len()));
    }

    #[tokio::test]
    async fn passes_antigravity_prompt_as_an_argument() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\nprintf '{\"response\":\"flag:%s\\\\nreceived:%s\"}\\n' \"$3\" \"$4\"\n",
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_, cancellation) = oneshot::channel();

        let outcome = execute(
            &antigravity_profile(executable),
            ExecutionRequest {
                prompt: "hello from dock".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(0),
                ..
            }
        ));
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Output { source: OutputSource::Stdout, line }
                if line == "flag:-p"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Output { source: OutputSource::Stdout, line }
                if line == "received:hello from dock"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report } if report.report_failure.is_none()
        )));
    }

    #[test]
    fn converts_decimal_costs_without_floating_point_rounding() {
        for (raw, expected) in [
            ("0", 0),
            ("0.0", 0),
            ("0.0000004", 0),
            ("0.0000005", 1),
            ("1.234567", 1_234_567),
            ("1.2345675", 1_234_568),
            ("1.5e-6", 2),
            ("1e-999999999999999999999999999999999999999999", 0),
            ("1e-9223372036854775808", 0),
            ("18446744073709.551615", u64::MAX),
        ] {
            assert_eq!(decimal_micros(raw), Ok(expected), "raw={raw}");
        }
        for raw in [
            "-0.1",
            "18446744073709.551616",
            "18446744073710",
            "1e999999999999999999999999999999999999999999",
            "NaN",
            "inf",
            "\"NaN\"",
            "1.2.3",
        ] {
            assert!(decimal_micros(raw).is_err(), "raw={raw}");
        }
    }

    #[test]
    fn parses_claude_fixture_and_keeps_missing_usage_items_missing() {
        let fixture = br#"{
            "type":"result",
            "subtype":"success",
            "is_error":false,
            "result":"Claude answer",
            "total_cost_usd":0.0000005,
            "usage":{
                "input_tokens":12,
                "output_tokens":34,
                "cache_read_input_tokens":5,
                "cache_creation_input_tokens":99
            },
            "modelUsage":{"claude-sonnet":{},"claude-opus":{}}
        }"#;
        let parsed = parse_claude_output(fixture).unwrap();
        assert_eq!(parsed.response, Some("Claude answer".to_owned()));
        assert_eq!(
            parsed.report.estimated_cost,
            Some(crate::EstimatedCost {
                currency: "USD".to_owned(),
                amount_micros: 1,
            })
        );
        assert_eq!(
            parsed.report.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(34),
                cached_input_tokens: Some(5),
                reasoning_tokens: None,
            })
        );
        assert_eq!(
            parsed.report.observed_models,
            vec!["claude-sonnet".to_owned(), "claude-opus".to_owned()]
        );

        let duplicate_models = parse_claude_output(
            br#"{"result":"Claude answer","modelUsage":{"claude-sonnet":{},"claude-sonnet":{},"claude-opus":{}}}"#,
        )
        .unwrap();
        assert_eq!(
            duplicate_models.report.observed_models,
            vec!["claude-sonnet".to_owned(), "claude-opus".to_owned()]
        );
    }

    #[test]
    fn handles_claude_errors_zero_cost_missing_result_and_missing_fields() {
        let error = parse_claude_output(
            br#"{"is_error":true,"result":"Claude error","total_cost_usd":0.0}"#,
        )
        .unwrap();
        assert_eq!(error.response, Some("Claude error".to_owned()));
        assert_eq!(
            error.report.estimated_cost,
            Some(crate::EstimatedCost {
                currency: "USD".to_owned(),
                amount_micros: 0,
            })
        );
        assert_eq!(error.report.usage, None);

        let missing = parse_claude_output(br#"{"result":"no report"}"#).unwrap();
        assert_eq!(missing.report, ExecutionReport::default());

        let null_cost = parse_claude_output(
            br#"{"result":"null cost","total_cost_usd":null,"usage":{"input_tokens":9}}"#,
        )
        .unwrap();
        assert_eq!(null_cost.report.estimated_cost, None);
        assert_eq!(
            null_cost.report.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(9),
                output_tokens: None,
                cached_input_tokens: None,
                reasoning_tokens: None,
            })
        );
        assert_eq!(
            null_cost.report.report_failure,
            Some(crate::ReportFailure::ParseFailure)
        );
        assert_eq!(null_cost.warnings.len(), 1);

        assert!(parse_claude_output(br#"{"is_error":false}"#).is_err());
        let error_without_result = parse_claude_output(
            br#"{"is_error":true,"total_cost_usd":0.0,"usage":{"input_tokens":9}}"#,
        )
        .unwrap();
        assert_eq!(error_without_result.response, None);
        assert_eq!(
            error_without_result.report.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(9),
                output_tokens: None,
                cached_input_tokens: None,
                reasoning_tokens: None,
            })
        );
        assert_eq!(error_without_result.warnings.len(), 1);

        let invalid_cost =
            parse_claude_output(br#"{"result":"answer","total_cost_usd":"NaN","usage":{}}"#)
                .unwrap();
        assert_eq!(invalid_cost.report.estimated_cost, None);
        assert_eq!(
            invalid_cost.report.usage,
            Some(crate::TokenUsage::default())
        );
        assert_eq!(
            invalid_cost.report.report_failure,
            Some(crate::ReportFailure::ParseFailure)
        );
        assert_eq!(invalid_cost.warnings.len(), 1);
    }

    #[test]
    fn parses_gemini_fixture_only_for_one_matching_model() {
        let fixture = br#"{
            "response":"Gemini answer",
            "stats":{"models":{"gemini-pro":{"tokens":{
                "prompt":10,"candidates":20,"cached":3,"thoughts":4
            }}}}
        }"#;
        let parsed = parse_gemini_output(fixture, Some("gemini-pro")).unwrap();
        assert_eq!(parsed.response, Some("Gemini answer".to_owned()));
        assert_eq!(
            parsed.report.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                cached_input_tokens: Some(3),
                reasoning_tokens: Some(4),
            })
        );
        assert_eq!(parsed.report.observed_models, vec!["gemini-pro".to_owned()]);
        assert_eq!(parsed.report.report_failure, None);

        let parsed = parse_gemini_output(fixture, None).unwrap();
        assert_eq!(
            parsed.report.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                cached_input_tokens: Some(3),
                reasoning_tokens: Some(4),
            })
        );
        assert_eq!(parsed.report.report_failure, None);

        let mixed = br#"{
            "response":"Gemini answer",
            "stats":{"models":{
                "gemini-pro":{"tokens":{"prompt":10}},
                "gemini-flash":{"tokens":{"candidates":20}}
            }}
        }"#;
        let parsed = parse_gemini_output(mixed, Some("gemini-pro")).unwrap();
        assert_eq!(parsed.report.usage, None);
        assert_eq!(
            parsed.report.observed_models,
            vec!["gemini-pro".to_owned(), "gemini-flash".to_owned()]
        );
        assert_eq!(
            parsed.report.report_failure,
            Some(crate::ReportFailure::ModelMismatch)
        );
        assert_eq!(parsed.warnings.len(), 1);

        let parsed = parse_gemini_output(fixture, Some("other-model")).unwrap();
        assert_eq!(parsed.report.usage, None);
        assert_eq!(
            parsed.report.report_failure,
            Some(crate::ReportFailure::ModelMismatch)
        );
    }

    #[test]
    fn gemini_missing_or_empty_stats_are_unreported_without_a_structural_failure() {
        for profile_model in [None, Some("gemini-pro")] {
            for fixture in [
                br#"{"response":"Gemini answer","stats":{"models":{}}}"#.as_slice(),
                br#"{"response":"Gemini answer"}"#.as_slice(),
            ] {
                let parsed = parse_gemini_output(fixture, profile_model).unwrap();
                assert_eq!(parsed.report.usage, None);
                assert!(parsed.report.observed_models.is_empty());
                assert_eq!(parsed.report.report_failure, None);
            }
        }
    }

    #[test]
    fn parses_antigravity_fixture_and_maps_usage() {
        let parsed = parse_antigravity_output(
            br#"{"response":"Antigravity answer","usage":{
                "input_tokens":11,"output_tokens":22,
                "cache_read_tokens":3,"thinking_tokens":4
            }}"#,
        )
        .unwrap();
        assert_eq!(parsed.response, Some("Antigravity answer".to_owned()));
        assert_eq!(
            parsed.report.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(22),
                cached_input_tokens: Some(3),
                reasoning_tokens: Some(4),
            })
        );
        assert!(parsed.report.observed_models.is_empty());
    }

    #[test]
    fn single_json_failures_fall_back_to_raw_output_and_keep_outcome_independent() {
        for (bytes, exceeded, raw_output_forwarded, failure) in [
            (
                b"not-json".as_slice(),
                false,
                false,
                crate::ReportFailure::ParseFailure,
            ),
            (
                b"".as_slice(),
                false,
                false,
                crate::ReportFailure::ParseFailure,
            ),
            (
                b"{\"response\":\"ok\"}".as_slice(),
                true,
                false,
                crate::ReportFailure::OutputLimitExceeded,
            ),
            (
                b"{\"response\":\"partial".as_slice(),
                false,
                false,
                crate::ReportFailure::ParseFailure,
            ),
        ] {
            let (events, report) =
                single_output_events(bytes, exceeded, raw_output_forwarded, CliKind::Gemini, None);
            assert_eq!(report.report_failure, Some(failure));
            assert!(report.observed_models.is_empty());
            assert!(!event_output_lines(&events).is_empty());
            assert_eq!(event_warnings(&events).len(), 1);
        }
    }

    #[test]
    fn codex_fixture_displays_messages_in_order_and_sums_turn_usage() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"thread"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#,
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"hidden"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":4}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"second"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":20,"cached_input_tokens":5,"output_tokens":6,"reasoning_output_tokens":7}}"#,
        ];
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut report = CodexReportAccumulator::default();
        for line in lines {
            process_codex_line(line.as_bytes(), &mut report, &sender);
        }
        let result = report.finish();
        drop(sender);
        let events = drain_events(&mut receiver);
        assert_eq!(event_output_lines(&events), ["first", "second"]);
        assert!(event_warnings(&events).is_empty());
        assert_eq!(
            result.usage,
            Some(crate::TokenUsage {
                input_tokens: Some(30),
                output_tokens: Some(9),
                cached_input_tokens: Some(7),
                reasoning_tokens: Some(11),
            })
        );
        assert_eq!(result.report_failure, None);
        assert!(result.observed_models.is_empty());
    }

    #[test]
    fn codex_ignores_missing_turn_completion_but_warns_on_errors_and_bad_rows() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut report = CodexReportAccumulator::default();
        process_codex_line(
            br#"{"type":"item.completed","item":{"type":"agent_message","text":"visible"}}"#,
            &mut report,
            &sender,
        );
        process_codex_line(
            br#"{"type":"error","message":"oops"}"#,
            &mut report,
            &sender,
        );
        process_codex_line(
            br#"{"type":"turn.failed","message":"failed"}"#,
            &mut report,
            &sender,
        );
        process_codex_line(b"not-json", &mut report, &sender);
        process_codex_line(
            br#"{"type":"item.completed","item":{"type":"agent_message"}}"#,
            &mut report,
            &sender,
        );
        let result = report.finish();
        drop(sender);
        let events = drain_events(&mut receiver);
        assert_eq!(
            event_output_lines(&events),
            [
                "visible",
                "not-json",
                r#"{"type":"item.completed","item":{"type":"agent_message"}}"#
            ]
        );
        assert_eq!(event_warnings(&events).len(), 4);
        assert_eq!(result.usage, None);
        assert_eq!(
            result.report_failure,
            Some(crate::ReportFailure::ParseFailure)
        );
    }

    #[test]
    fn codex_missing_turn_completion_is_missing_without_a_report_failure() {
        let lines: [&[u8]; 2] = [
            br#"{"type":"thread.started","thread_id":"thread"}"#,
            br#"{"type":"item.completed","item":{"type":"agent_message","text":"visible"}}"#,
        ];
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut report = CodexReportAccumulator::default();
        for line in lines {
            process_codex_line(line, &mut report, &sender);
        }
        let result = report.finish();
        drop(sender);

        assert_eq!(result.usage, None);
        assert_eq!(result.report_failure, None);
        let events = drain_events(&mut receiver);
        assert_eq!(event_output_lines(&events), ["visible"]);
        assert!(event_warnings(&events).is_empty());
    }

    #[test]
    fn codex_missing_usage_fields_stay_missing_in_both_turn_orders() {
        let complete: &[u8] = br#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":4}}"#;
        let missing_input: &[u8] = br#"{"type":"turn.completed","usage":{"cached_input_tokens":5,"output_tokens":6,"reasoning_output_tokens":7}}"#;

        for lines in [[complete, missing_input], [missing_input, complete]] {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let mut report = CodexReportAccumulator::default();
            for line in lines {
                process_codex_line(line, &mut report, &sender);
            }
            let result = report.finish();
            drop(sender);
            let events = drain_events(&mut receiver);

            assert_eq!(
                result.usage,
                Some(crate::TokenUsage {
                    input_tokens: None,
                    output_tokens: Some(9),
                    cached_input_tokens: Some(7),
                    reasoning_tokens: Some(11),
                })
            );
            assert_eq!(result.report_failure, None);
            assert!(event_output_lines(&events).is_empty());
            assert!(event_warnings(&events).is_empty());
        }
    }

    #[test]
    fn codex_ignores_unknown_event_types_without_output_warning_or_parse_failure() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut report = CodexReportAccumulator::default();
        process_codex_line(
            br#"{"type":"future.event","payload":{"anything":"valid"}}"#,
            &mut report,
            &sender,
        );
        let result = report.finish();
        drop(sender);
        let events = drain_events(&mut receiver);

        assert!(event_output_lines(&events).is_empty());
        assert!(event_warnings(&events).is_empty());
        assert_eq!(result.usage, None);
        assert_eq!(result.report_failure, None);
    }

    #[test]
    fn codex_usage_overflow_discards_the_entire_usage_report() {
        let lines = [
            format!(
                r#"{{"type":"turn.completed","usage":{{"input_tokens":{}}}}}"#,
                u64::MAX
            ),
            r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#.to_owned(),
        ];
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut report = CodexReportAccumulator::default();
        for line in &lines {
            process_codex_line(line.as_bytes(), &mut report, &sender);
        }
        let result = report.finish();
        drop(sender);
        let events = drain_events(&mut receiver);

        assert_eq!(result.usage, None);
        assert_eq!(
            result.report_failure,
            Some(crate::ReportFailure::ParseFailure)
        );
        assert_eq!(event_warnings(&events).len(), 1);
        assert!(event_warnings(&events)[0].contains("exceeded u64"));
    }

    #[test]
    fn codex_invalid_turn_usage_discards_the_entire_report_in_any_order() {
        let valid: &[u8] = br#"{"type":"turn.completed","usage":{"input_tokens":10}}"#;
        let invalid_fixtures: [&[u8]; 3] = [
            br#"{"type":"turn.completed","usage":{"input_tokens":"invalid"}}"#,
            br#"{"type":"turn.completed","usage":{"input_tokens":-1}}"#,
            br#"{"type":"turn.completed"}"#,
        ];

        for invalid in invalid_fixtures {
            let cases: [[&[u8]; 2]; 2] = [[valid, invalid], [invalid, valid]];
            for lines in cases {
                let (sender, mut receiver) = mpsc::unbounded_channel();
                let mut report = CodexReportAccumulator::default();
                for line in lines {
                    process_codex_line(line, &mut report, &sender);
                }
                let result = report.finish();
                drop(sender);
                let events = drain_events(&mut receiver);

                assert_eq!(result.usage, None);
                assert_eq!(
                    result.report_failure,
                    Some(crate::ReportFailure::ParseFailure)
                );
                assert_eq!(event_warnings(&events).len(), 1);
            }
        }
    }

    #[test]
    fn codex_error_warnings_use_nested_messages_or_only_the_event_type() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut report = CodexReportAccumulator::default();
        process_codex_line(
            br#"{"type":"error","error":{"message":"nested failure"}}"#,
            &mut report,
            &sender,
        );
        process_codex_line(br#"{"type":"turn.failed"}"#, &mut report, &sender);
        drop(sender);
        let events = drain_events(&mut receiver);

        assert_eq!(
            event_warnings(&events),
            ["Codex error: nested failure", "Codex turn.failed"]
        );
    }

    #[tokio::test]
    async fn test_adapter_passes_raw_lines_and_has_no_report() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\nprintf 'raw line\\n'\nprintf 'another raw line\\n'\n",
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_, cancellation) = oneshot::channel();
        let outcome = execute(
            &test_profile(executable),
            ExecutionRequest {
                prompt: "ignored".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(0),
                ..
            }
        ));
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Output {
                source: OutputSource::Stdout,
                line
            } if line == "raw line"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report } if report == &ExecutionReport::default()
        )));
    }

    #[tokio::test]
    async fn execute_extracts_reports_from_all_machine_readable_cli_fixtures() {
        let directory = tempfile::tempdir().unwrap();
        let fixtures = [
            (
                CliKind::Claude,
                None,
                r#"{"result":"claude","total_cost_usd":0.25,"usage":{"input_tokens":1,"output_tokens":2}}"#,
                Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 250_000,
                }),
                Some(crate::TokenUsage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
            ),
            (
                CliKind::Codex,
                None,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"codex"}}
{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":4}}"#,
                None,
                Some(crate::TokenUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(4),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
            ),
            (
                CliKind::Gemini,
                Some("gemini-pro"),
                r#"{"response":"gemini","stats":{"models":{"gemini-pro":{"tokens":{"prompt":5,"candidates":6}}}}}"#,
                None,
                Some(crate::TokenUsage {
                    input_tokens: Some(5),
                    output_tokens: Some(6),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
            ),
            (
                CliKind::Antigravity,
                None,
                r#"{"response":"antigravity","usage":{"input_tokens":7,"output_tokens":8}}"#,
                None,
                Some(crate::TokenUsage {
                    input_tokens: Some(7),
                    output_tokens: Some(8),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
            ),
        ];

        for (cli, model, fixture, estimated_cost, usage) in fixtures {
            let executable = write_script(
                directory.path(),
                &format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", fixture),
            );
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let (_, cancellation) = oneshot::channel();
            let outcome = execute(
                &cli_profile(executable, cli, model),
                ExecutionRequest {
                    prompt: "fixture prompt".to_owned(),
                    working_directory: directory.path().to_path_buf(),
                },
                cancellation,
                move |event| captured.lock().unwrap().push(event),
            )
            .await
            .unwrap();
            assert!(matches!(
                outcome,
                ExecutionOutcome::Completed {
                    exit_code: Some(0),
                    ..
                }
            ));
            let events = events.lock().unwrap();
            let report = events.iter().find_map(|event| match event {
                ExecutionEvent::Report { report } => Some(report),
                _ => None,
            });
            assert_eq!(
                report.map(|report| &report.estimated_cost),
                Some(&estimated_cost)
            );
            assert_eq!(report.map(|report| &report.usage), Some(&usage));
        }
    }

    #[tokio::test]
    async fn retains_only_one_mib_and_marks_an_overflowing_single_output() {
        let (mut writer, reader) = tokio::io::duplex(MAX_EXECUTION_OUTPUT_BYTES + 1);
        let payload = vec![b'x'; MAX_EXECUTION_OUTPUT_BYTES + 1];
        let payload_len = payload.len();
        let writer_task = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let limited = read_limited(reader, &sender).await.unwrap();
        writer_task.await.unwrap();
        assert_eq!(limited.bytes.len(), 0);
        assert!(limited.exceeded);
        assert!(limited.raw_output_forwarded);
        drop(sender);
        let events = drain_events(&mut receiver);
        let displayed: String = event_output_lines(&events).concat();
        assert_eq!(displayed.len(), payload_len);
    }

    #[tokio::test]
    async fn execute_forwards_all_overflowing_output_and_accepts_the_exact_limit() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = b"{\"result\":\"";
        let suffix = b"\"}";
        let exact_response_len = MAX_EXECUTION_OUTPUT_BYTES - prefix.len() - suffix.len();
        let cases = [
            ("overflow.json", exact_response_len + 1, true),
            ("exact.json", exact_response_len, false),
        ];

        for (payload_name, response_len, exceeded) in cases {
            let mut payload = Vec::with_capacity(prefix.len() + response_len + suffix.len());
            payload.extend_from_slice(prefix);
            payload.extend(std::iter::repeat_n(b'x', response_len));
            payload.extend_from_slice(suffix);
            fs::write(directory.path().join(payload_name), &payload).unwrap();
            let executable = write_script(
                directory.path(),
                &format!("#!/bin/sh\ncat {payload_name}\n"),
            );
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let (_cancellation_sender, cancellation) = oneshot::channel();
            let outcome = execute(
                &profile(executable),
                ExecutionRequest {
                    prompt: "fixture".to_owned(),
                    working_directory: directory.path().to_path_buf(),
                },
                cancellation,
                move |event| captured.lock().unwrap().push(event),
            )
            .await
            .unwrap();
            let ExecutionOutcome::Completed { exit_code, .. } = outcome else {
                panic!("fixture execution should complete");
            };
            assert_eq!(exit_code, Some(0));

            let events = events.lock().unwrap();
            let displayed: String = event_output_lines(&events).concat();
            let report = events
                .iter()
                .find_map(|event| match event {
                    ExecutionEvent::Report { report } => Some(report),
                    _ => None,
                })
                .expect("execution should emit one report");
            if exceeded {
                assert_eq!(displayed.as_bytes(), payload.as_slice());
                assert_eq!(report.estimated_cost, None);
                assert_eq!(report.usage, None);
                assert_eq!(
                    report.report_failure,
                    Some(crate::ReportFailure::OutputLimitExceeded)
                );
            } else {
                assert_eq!(displayed, "x".repeat(response_len));
                assert_eq!(report.estimated_cost, None);
                assert_eq!(report.usage, None);
                assert_eq!(report.report_failure, None);
            }
        }
    }

    #[tokio::test]
    async fn codex_line_limit_excludes_lf_crlf_and_eof_terminators() {
        let prefix = br#"{"type":"thread.started"}"#;
        for (ending_name, ending) in [
            ("LF", b"\n".as_slice()),
            ("CRLF", b"\r\n".as_slice()),
            ("EOF", b"".as_slice()),
        ] {
            for extra_bytes in [0, 1] {
                let mut body = Vec::with_capacity(MAX_EXECUTION_OUTPUT_BYTES + extra_bytes);
                body.extend_from_slice(prefix);
                body.extend(std::iter::repeat_n(
                    b' ',
                    MAX_EXECUTION_OUTPUT_BYTES - prefix.len() + extra_bytes,
                ));
                let mut chunks = vec![body];
                if !ending.is_empty() {
                    chunks.push(ending.to_vec());
                }
                let reader = ChunkedReader {
                    chunks,
                    next: 0,
                    offset: 0,
                };
                let (sender, _) = mpsc::unbounded_channel();

                let report = forward_codex_lines(reader, sender.clone()).await.unwrap();

                let expected_failure =
                    (extra_bytes == 1).then_some(crate::ReportFailure::OutputLimitExceeded);
                assert_eq!(
                    report.report_failure, expected_failure,
                    "unexpected report for {ending_name} with {extra_bytes} extra bytes"
                );
            }
        }

        let mut body = Vec::with_capacity(MAX_EXECUTION_OUTPUT_BYTES + 1);
        body.extend_from_slice(prefix);
        body.extend(std::iter::repeat_n(
            b' ',
            MAX_EXECUTION_OUTPUT_BYTES - prefix.len(),
        ));
        body.push(b'\r');
        let reader = ChunkedReader {
            chunks: vec![body],
            next: 0,
            offset: 0,
        };
        let (sender, _) = mpsc::unbounded_channel();

        let report = forward_codex_lines(reader, sender).await.unwrap();

        assert_eq!(
            report.report_failure,
            Some(crate::ReportFailure::OutputLimitExceeded)
        );
    }

    #[tokio::test]
    async fn overflowing_codex_line_keeps_prior_message_and_raw_forwards_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let prior_message =
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"before overflow"}}"#;
        let prior_usage =
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":4}}"#;
        let oversized_line = "雪".repeat(MAX_EXECUTION_OUTPUT_BYTES / "雪".len() + 1);
        let trailing_line =
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"after overflow"}}"#;
        let mut payload = oversized_line.as_bytes().to_vec();
        payload.push(b'\n');
        payload.extend_from_slice(trailing_line.as_bytes());
        payload.push(b'\n');
        fs::write(directory.path().join("codex-overflow.txt"), payload).unwrap();
        let executable = write_script(
            directory.path(),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' '{prior_message}' '{prior_usage}'\ncat codex-overflow.txt\n"
            ),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_cancellation_sender, cancellation) = oneshot::channel();

        let outcome = execute(
            &cli_profile(executable, CliKind::Codex, None),
            ExecutionRequest {
                prompt: "codex overflow".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(0),
                ..
            }
        ));
        let events = events.lock().unwrap();
        let lines = event_output_lines(&events);
        let displayed: String = lines.concat();
        assert_eq!(
            displayed,
            format!("before overflow{oversized_line}{trailing_line}")
        );
        assert_eq!(lines.first(), Some(&"before overflow"));
        assert!(lines.contains(&trailing_line));
        assert!(
            lines
                .iter()
                .all(|line| line.len() <= RAW_OUTPUT_FORWARD_CHUNK_BYTES)
        );
        assert_eq!(event_warnings(&events).len(), 1);
        let report = events
            .iter()
            .find_map(|event| match event {
                ExecutionEvent::Report { report } => Some(report),
                _ => None,
            })
            .expect("execution should emit one report");
        assert_eq!(report.usage, None);
        assert_eq!(
            report.report_failure,
            Some(crate::ReportFailure::OutputLimitExceeded)
        );
    }

    #[tokio::test]
    async fn raw_overflow_forwarding_preserves_content_and_split_utf8() {
        let mut retained = vec![b'a'; MAX_EXECUTION_OUTPUT_BYTES - 2];
        retained.push(b'\n');
        let reader = ChunkedReader {
            chunks: vec![
                retained,
                vec![0xe9],
                vec![0x9b, 0xaa, b'\n', b't', b'a', b'i', b'l', b'\n'],
            ],
            next: 0,
            offset: 0,
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();

        let limited = read_limited(reader, &sender).await.unwrap();

        assert_eq!(limited.bytes.len(), 0);
        assert!(limited.exceeded);
        assert!(limited.raw_output_forwarded);
        drop(sender);
        let events = drain_events(&mut receiver);
        let lines = event_output_lines(&events);
        let displayed: String = lines.concat();
        let expected = format!("{}雪tail", "a".repeat(MAX_EXECUTION_OUTPUT_BYTES - 2));
        assert_eq!(displayed, expected);
        assert!(lines.contains(&"雪"));
        assert_eq!(lines.last(), Some(&"tail"));
        assert!(
            lines
                .iter()
                .all(|line| line.len() <= RAW_OUTPUT_FORWARD_CHUNK_BYTES)
        );
    }

    #[tokio::test]
    async fn overflowing_child_output_forwards_every_line_after_multiple_reads() {
        let directory = tempfile::tempdir().unwrap();
        let mut payload = Vec::new();
        let mut expected = Vec::new();
        for index in 0..20_000 {
            let line = format!("line-{index:05}-雪-{}", "x".repeat(96));
            payload.extend_from_slice(line.as_bytes());
            payload.push(b'\n');
            expected.push(line);
        }
        assert!(payload.len() > MAX_EXECUTION_OUTPUT_BYTES * 2);
        fs::write(directory.path().join("overflow-lines.txt"), payload).unwrap();
        let executable = write_script(directory.path(), "#!/bin/sh\ncat overflow-lines.txt\n");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_cancellation_sender, cancellation) = oneshot::channel();

        let outcome = execute(
            &profile(executable),
            ExecutionRequest {
                prompt: "overflow lines".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(0),
                ..
            }
        ));
        let events = events.lock().unwrap();
        let lines = event_output_lines(&events);
        assert_eq!(lines.len(), expected.len());
        for (actual, expected) in lines.iter().zip(&expected) {
            assert_eq!(*actual, expected);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report }
                if report.report_failure == Some(crate::ReportFailure::OutputLimitExceeded)
        )));
    }

    #[tokio::test]
    async fn overflowing_child_without_newlines_bounds_chunks_and_preserves_utf8() {
        let directory = tempfile::tempdir().unwrap();
        let payload = "雪".repeat((3 * MAX_EXECUTION_OUTPUT_BYTES) / "雪".len() + 1);
        fs::write(directory.path().join("overflow-no-newline.txt"), &payload).unwrap();
        let executable = write_script(directory.path(), "#!/bin/sh\ncat overflow-no-newline.txt\n");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let (_cancellation_sender, cancellation) = oneshot::channel();

        let outcome = execute(
            &profile(executable),
            ExecutionRequest {
                prompt: "overflow without newline".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            move |event| captured.lock().unwrap().push(event),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed {
                exit_code: Some(0),
                ..
            }
        ));
        let events = events.lock().unwrap();
        let lines = event_output_lines(&events);
        let displayed: String = lines.concat();
        assert_eq!(displayed, payload);
        assert!(lines.len() > 100);
        assert!(
            lines
                .iter()
                .all(|line| line.len() <= RAW_OUTPUT_FORWARD_CHUNK_BYTES)
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report }
                if report.report_failure == Some(crate::ReportFailure::OutputLimitExceeded)
        )));
    }

    #[tokio::test]
    async fn cancels_the_child_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\ntrap 'exit 0' INT\nprintf 'ready\\n' >&2\nwhile :; do sleep 1; done\n",
        );
        let (trigger, cancellation) = oneshot::channel();
        let trigger = Arc::new(Mutex::new(Some(trigger)));

        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let trigger_for_callback = trigger.clone();
        let outcome = timeout(
            Duration::from_secs(5),
            execute(
                &profile(executable),
                ExecutionRequest {
                    prompt: "stop".to_owned(),
                    working_directory: directory.path().to_path_buf(),
                },
                cancellation,
                move |event| {
                    if matches!(
                        &event,
                        ExecutionEvent::Output {
                            source: OutputSource::Stderr,
                            line
                        } if line == "ready"
                    ) && let Some(trigger) = trigger_for_callback.lock().unwrap().take()
                    {
                        let _ = trigger.send(());
                    }
                    captured.lock().unwrap().push(event);
                },
            ),
        )
        .await
        .expect("cancelled execution should not hang")
        .unwrap();

        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(outcome.process_exit_code(), 130);
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Output {
                source: OutputSource::Stderr,
                line
            } if line == "ready"
        )));
        assert!(
            event_warnings(&events)
                .iter()
                .any(|warning| warning.contains("could not parse Claude JSON output"))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report }
                if report.report_failure == Some(crate::ReportFailure::ParseFailure)
        )));
    }

    #[tokio::test]
    async fn cancelled_execution_parses_complete_machine_readable_output() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\ntrap 'exit 0' INT\nprintf '%s\\n' '{\"result\":\"complete\",\"total_cost_usd\":0.25,\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}'\nprintf 'ready\\n' >&2\nwhile :; do sleep 1; done\n",
        );
        let (trigger, cancellation) = oneshot::channel();
        let trigger = Arc::new(Mutex::new(Some(trigger)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let trigger_for_callback = trigger.clone();
        let outcome = timeout(
            Duration::from_secs(5),
            execute(
                &profile(executable),
                ExecutionRequest {
                    prompt: "cancel after output".to_owned(),
                    working_directory: directory.path().to_path_buf(),
                },
                cancellation,
                move |event| {
                    if matches!(
                        &event,
                        ExecutionEvent::Output {
                            source: OutputSource::Stderr,
                            line
                        } if line == "ready"
                    ) && let Some(trigger) = trigger_for_callback.lock().unwrap().take()
                    {
                        let _ = trigger.send(());
                    }
                    captured.lock().unwrap().push(event);
                },
            ),
        )
        .await
        .expect("cancelled execution should not hang")
        .unwrap();

        let ExecutionOutcome::Cancelled { elapsed } = outcome else {
            panic!("fixture execution should be cancelled");
        };
        assert!(elapsed > Duration::ZERO);
        let events = events.lock().unwrap();
        assert!(event_output_lines(&events).contains(&"complete"));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::Report { report }
                if report.estimated_cost == Some(crate::EstimatedCost {
                    currency: "USD".to_owned(),
                    amount_micros: 250_000,
                })
                    && report.usage == Some(crate::TokenUsage {
                        input_tokens: Some(3),
                        output_tokens: Some(4),
                        cached_input_tokens: None,
                        reasoning_tokens: None,
                    })
                    && report.report_failure.is_none()
        )));
    }

    #[tokio::test]
    async fn reports_a_missing_executable_as_a_spawn_error() {
        let directory = tempfile::tempdir().unwrap();
        let (_cancellation_sender, cancellation) = oneshot::channel();
        let result = execute(
            &profile(directory.path().join("does-not-exist")),
            ExecutionRequest {
                prompt: "hello".to_owned(),
                working_directory: directory.path().to_path_buf(),
            },
            cancellation,
            |_| {},
        )
        .await;

        assert!(matches!(result, Err(ExecutionError::Spawn { .. })));
    }
}

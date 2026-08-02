use std::{
    io,
    os::unix::process::CommandExt,
    process::ExitStatus,
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::timeout,
};

use crate::{ExecutionProfile, PromptTransport};

const TERMINATION_GRACE_PERIOD: Duration = Duration::from_secs(3);

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
    Finished,
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
    let stdout_task = tokio::spawn(forward_lines(stdout, OutputSource::Stdout, sender.clone()));
    let stderr_task = tokio::spawn(forward_lines(stderr, OutputSource::Stderr, sender));

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
    stdout_task.await.map_err(ExecutionError::Task)??;
    stderr_task.await.map_err(ExecutionError::Task)??;
    while let Ok(event) = receiver.try_recv() {
        on_event(event);
    }
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
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::AdapterKind;

    fn write_script(directory: &Path, source: &str) -> PathBuf {
        let path = directory.join("fake-agent");
        fs::write(&path, source).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn profile(executable: PathBuf) -> ExecutionProfile {
        ExecutionProfile {
            id: "fake-agent".to_owned(),
            name: "Fake agent".to_owned(),
            adapter: AdapterKind::Claude,
            executable: Some(executable),
            model: None,
            args: Vec::new(),
        }
    }

    fn antigravity_profile(executable: PathBuf) -> ExecutionProfile {
        ExecutionProfile {
            adapter: AdapterKind::Antigravity,
            ..profile(executable)
        }
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
        assert!(matches!(
            events.first(),
            Some(ExecutionEvent::Started { .. })
        ));
        assert_eq!(events.last(), Some(&ExecutionEvent::Finished));
    }

    #[tokio::test]
    async fn passes_antigravity_prompt_as_an_argument() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\nprintf 'flag:%s\\n' \"$1\"\nprintf 'received:%s\\n' \"$2\"\n",
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
    }

    #[tokio::test]
    async fn cancels_the_child_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let executable = write_script(
            directory.path(),
            "#!/bin/sh\ntrap 'exit 0' INT\nprintf 'ready\\n'\nwhile :; do sleep 1; done\n",
        );
        let (trigger, cancellation) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = trigger.send(());
        });

        let outcome = timeout(
            Duration::from_secs(5),
            execute(
                &profile(executable),
                ExecutionRequest {
                    prompt: "stop".to_owned(),
                    working_directory: directory.path().to_path_buf(),
                },
                cancellation,
                |_| {},
            ),
        )
        .await
        .expect("cancelled execution should not hang")
        .unwrap();

        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
        assert_eq!(outcome.process_exit_code(), 130);
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

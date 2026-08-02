pub mod adapter;
pub mod config;
pub mod execution;
pub mod record;

pub use adapter::{AdapterKind, CommandSpec, ExecutionProfile, PromptTransport};
pub use config::{Config, ConfigError, default_config_path};
pub use execution::{
    ExecutionError, ExecutionEvent, ExecutionOutcome, ExecutionRequest, OutputSource, execute,
};
pub use record::{
    EVENT_SCHEMA_VERSION, Event, EventKind, EventLog, FailureKind, ProfileSnapshot, ReadEvents,
    RecordError, RecordedExecutionOutcome, SkippedLine, Verdict, default_events_path,
};

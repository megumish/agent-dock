pub mod adapter;
pub mod config;
pub mod execution;
pub mod judgement;
pub mod record;
pub mod scorecard;

pub use adapter::{
    CliKind, CommandSpec, ExecutionProfile, ProfileDeclaration, PromptTransport,
    safe_test_declaration, safe_test_profile,
};
pub use config::{Config, ConfigError, ConfigRevision, ReplaceConfigOutcome, default_config_path};
pub use execution::{
    ExecutionError, ExecutionEvent, ExecutionOutcome, ExecutionRequest, OutputSource, execute,
};
pub use judgement::{JudgementCandidate, judgement_candidates};
pub use record::{
    AppendJudgementOutcome, BackupEventLogOutcome, EVENT_FORMAT_VERSION, Event, EventKind,
    EventLog, FailureKind, ProfileSnapshot, ReadEvents, RecordError, RecordedExecutionOutcome,
    SkippedLine, SkippedLineReason, Verdict, default_events_path,
};
pub use scorecard::{
    AcceptanceAxis, AxisSummary, CostAxis, DurationAxis, ProfileScorecard, Projection, Segment,
    SegmentScore, escape_terminal, format_acceptance, format_duration, format_recency, project,
    segments_for_tags,
};

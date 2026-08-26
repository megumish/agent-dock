pub mod adapter;
pub mod advice;
pub mod config;
pub mod execution;
pub mod hook;
pub mod judgement;
pub mod observation;
pub mod observe_cli;
pub mod record;
pub mod scorecard;

pub use adapter::{
    CliKind, CommandSpec, ExecutionPlatform, ExecutionProfile, ProfileDeclaration, PromptTransport,
    safe_test_declaration, safe_test_profile,
};
pub use advice::{ADVICE_MIN_JUDGED, Advice, advise};
pub use config::{Config, ConfigError, ConfigRevision, ReplaceConfigOutcome, default_config_path};
pub use execution::{
    ExecutionError, ExecutionEvent, ExecutionOutcome, ExecutionRequest, OutputSource, execute,
};
pub use hook::{
    HookEvent, HookObservation, HookParseError, MAX_HOOK_INPUT_BYTES, Provider,
    parse_hook_observation,
};
pub use judgement::{JudgementCandidate, judgement_candidates};
pub use observation::{
    ObservationCommand, ObservationError, ObservationOutcome, apply_observation,
    configuration_for_profile, resolve_observed_session,
};
pub use observe_cli::{ObserveCliCommand, parse_observe_args};
pub use record::{
    ActualCost, AdviceEvidence, AdviceOutcome, AdviceReason, AdviceSegmentCount,
    AppendBatchOutcome, AppendJudgementOutcome, AttributionFailure, BackupEventLogOutcome,
    ConfigurationBoundary, EVENT_FORMAT_VERSION, Event, EventKind, EventLog, EvidenceAcceptance,
    EvidenceAxisSummary, EvidenceCost, EvidenceDuration, EvidenceSegment, ExecutionOrigin,
    FailureKind, ObservationAnomaly, ObservedConfiguration, ObservedTaskStatus, ObservedValue,
    ProfileAttribution, ProfileSnapshot, Provenance, ReadEvents, RecordError,
    RecordedExecutionOutcome, SessionPhase, SkippedLine, SkippedLineReason, Verdict,
    default_events_path,
};
pub use scorecard::{
    AcceptanceAxis, AxisSummary, CostAxis, DurationAxis, ExclusionReason, ExclusionSummary,
    OriginCounts, ProfileScorecard, Projection, Segment, SegmentScore, escape_terminal,
    format_acceptance, format_duration, format_recency, project, segments_for_tags,
};

pub mod adapter;
pub mod config;
pub mod execution;

pub use adapter::{AdapterKind, CommandSpec, ExecutionProfile, PromptTransport};
pub use config::{
    CONFIG_API_VERSION, Config, ConfigError, ReplaceConfigOutcome, default_config_path,
};
pub use execution::{
    ExecutionError, ExecutionEvent, ExecutionOutcome, ExecutionRequest, OutputSource, execute,
};

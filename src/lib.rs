pub mod adapter;
pub mod config;
pub mod execution;

pub use adapter::{AdapterKind, CommandSpec, ExecutionProfile};
pub use config::{Config, ConfigError, default_config_path};
pub use execution::{
    ExecutionError, ExecutionEvent, ExecutionOutcome, ExecutionRequest, OutputSource, execute,
};

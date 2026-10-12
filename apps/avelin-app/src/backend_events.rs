//! Runtime uses the same payload validation and terminal rules as offline replay.
pub(super) use avelin_core::protocol::{
    BackendEvent, BackendEvents, Progress, ProgressPhase, TaskResult, observation_complete,
};

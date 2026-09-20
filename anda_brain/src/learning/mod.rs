//! Trusted online-learning contracts, independent of the offline eval harness.
//!
//! Native learning records and the persistent `LearningRuntime` are trusted
//! host interfaces. Explicit registration, an actual executor and separately
//! authenticated observations are required; enabling the Cargo feature alone
//! does not start production work or confer Skill standing. Model-facing
//! restrictions remain in `crate::kip`.

// Keep the same structured error type as Nexus's EvaluationRule and host APIs;
// boxing only these adapters would require unboxing at every trait boundary.
#![allow(clippy::result_large_err)]

mod automation;
mod config;
pub use automation::*;
mod execution;
use crate::journal;
pub mod native;
mod paired;
mod plan;
pub(crate) mod recall;
mod runtime;
pub mod workflow_http;
pub(crate) use runtime::{LateOutcome, ObservationRoute};

pub use config::{ExecutorIdentity, LearningConfig};
pub use execution::{AdoptionBasis, AttemptBudget, ExecutionContract, workflow_contract};
pub use paired::{PairedRule, paired_rule_artifact, register_paired_rule};
pub use plan::{PairCase, PairedTrialPlan};
pub use runtime::{
    ApplicationContext, ArchiveStamp, AttemptReport, DispatchState, DispatchTicket, DriveResult,
    JobPage, JobReport, JobStage, LearningCapacity, LearningExecutor, LearningRuntime,
    LearningStoragePolicy, OutcomeMeasurements, OutcomeSubmission, ProcedureStatus,
    ReconcileResult, ReviewPage, ReviewReason, ReviewSchedule, ReviewStatus, SafetyReport,
    SafetySubmission,
};

#[cfg(test)]
mod contracts;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod workflow_fixture;

fn invalid(message: impl Into<String>) -> anda_kip::KipError {
    anda_kip::KipError::constraint_violation(message.into())
}

fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn is_revision(value: &str) -> bool {
    value.strip_prefix("C-").is_some_and(|row| {
        row.parse::<u64>()
            .is_ok_and(|id| id > 0 && id.to_string() == row)
    })
}

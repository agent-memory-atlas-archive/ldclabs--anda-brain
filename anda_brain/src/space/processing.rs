use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingKind {
    Formation,
    Maintenance,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessingReport {
    pub kind: ProcessingKind,
    pub conversation: u64,
    pub state: ProcessingState,
    pub failed_reason: Option<String>,
    /// Persisted provider usage; a failed provider call may report no usage.
    pub usage: Usage,
}

impl ProcessingReport {
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self.state,
            ProcessingState::Queued | ProcessingState::Running
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessingWait {
    pub timed_out: bool,
    pub report: ProcessingReport,
}

impl Space {
    /// Reads the *requested conversation*, never a high-water mark. Failed
    /// formation remains running while its automatic retry is still owned.
    pub async fn processing_report(
        &self,
        kind: ProcessingKind,
        id: u64,
    ) -> Result<ProcessingReport, BoxError> {
        let (conversation, active) = match kind {
            ProcessingKind::Formation => (
                self.memory.get_conversation(id).await?,
                self.formation.processing_id() == id,
            ),
            ProcessingKind::Maintenance => (
                self.maintenance.conversations.get_conversation(id).await?,
                self.maintenance.processing_id() == id,
            ),
        };
        let state = if active {
            ProcessingState::Running
        } else {
            match conversation.status {
                ConversationStatus::Completed => ProcessingState::Completed,
                ConversationStatus::Failed => ProcessingState::Failed,
                ConversationStatus::Cancelled => ProcessingState::Cancelled,
                ConversationStatus::Submitted if !self.engine.is_cancelled() => {
                    ProcessingState::Queued
                }
                _ => ProcessingState::Interrupted,
            }
        };
        Ok(ProcessingReport {
            kind,
            conversation: id,
            state,
            failed_reason: conversation.failed_reason,
            usage: conversation.usage,
        })
    }

    /// A real-time wait. Timeout does not cancel, relabel or forget the work.
    /// Completed means the model workflow ended; it does not attest arbitrary
    /// semantic change coverage or a successful downstream business task.
    pub async fn wait_for_processing(
        &self,
        kind: ProcessingKind,
        id: u64,
        wait: Duration,
    ) -> Result<ProcessingWait, BoxError> {
        let start = tokio::time::Instant::now();
        loop {
            let report = self.processing_report(kind, id).await?;
            if report.is_terminal() {
                return Ok(ProcessingWait {
                    timed_out: false,
                    report,
                });
            }
            if start.elapsed() >= wait {
                return Ok(ProcessingWait {
                    timed_out: true,
                    report,
                });
            }
            tokio::time::sleep(Duration::from_millis(10).min(wait.saturating_sub(start.elapsed())))
                .await;
        }
    }
}

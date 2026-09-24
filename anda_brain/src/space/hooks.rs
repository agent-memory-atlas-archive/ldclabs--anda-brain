//! Agent completion accounting and sequential writer handoff.
use super::*;

pub(super) struct Hooks {
    db: Arc<AndaDB>,
    space: OnceLock<Weak<Space>>,
}

impl Hooks {
    pub(super) fn new(db: Arc<AndaDB>) -> Self {
        Self {
            db,
            space: OnceLock::new(),
        }
    }

    pub(super) fn bind_space(&self, space: Weak<Space>) {
        let _ = self.space.set(space);
    }

    pub(super) fn space(&self) -> Option<Arc<Space>> {
        self.space.get().and_then(Weak::upgrade)
    }
}

// grcov-excl-start: async_trait rewrites this impl into generated futures; behavior is covered by hook and agent scheduling tests.
#[async_trait::async_trait]
impl BrainHook for Hooks {
    fn is_maintenance_processing(&self) -> bool {
        self.space()
            .map(|space| space.maintenance.is_processing())
            .unwrap_or(false)
    }

    async fn memory_predecessor_failed(
        &self,
        intent: &crate::memory_interface::MemoryIntent,
    ) -> Option<String> {
        let space = self.space()?;
        space.memory_predecessors_failed(intent).await
    }

    async fn on_conversation_end(&self, agent_name: &str, conversation: &Conversation) {
        #[cfg(feature = "experiments")]
        if self.space().is_some_and(|space| !space.automatic) {
            use experiments::{CostStage, StageCost};
            let stage = match agent_name {
                "formation_memory" => Some(CostStage::Formation),
                "recall_memory" => Some(CostStage::Recall),
                "maintenance_memory" => Some(CostStage::Maintenance),
                _ => None,
            };
            if let Some(stage) = stage {
                // Each callback is one execution, including a failed attempt
                // before Formation's retry. Do not overwrite it with final usage.
                let known = conversation.usage.requests > 0
                    || conversation.usage.input_tokens > 0
                    || conversation.usage.output_tokens > 0;
                let mut truncated = false;
                self.db
                    .set_extension_from_with("experiment_costs".into(), |value| {
                        let mut rows: Vec<StageCost> = value.unwrap_or_default();
                        if rows.len() >= 10_000 {
                            truncated = true;
                            return Some(rows);
                        }
                        rows.push(StageCost {
                            stage,
                            conversation: Some(conversation._id),
                            failed: conversation.status != ConversationStatus::Completed,
                            requests: known.then_some(conversation.usage.requests),
                            input_tokens: known.then_some(conversation.usage.input_tokens),
                            output_tokens: known.then_some(conversation.usage.output_tokens),
                            elapsed_ms: None,
                            accounting_complete: false,
                        });
                        Some(rows)
                    });
                if truncated {
                    self.db
                        .set_extension_from("experiment_costs_truncated".into(), true);
                }
            }
        }
        match agent_name {
            "recall_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("recall_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // Usage-ledger writeback (plan M1): record which memories
                // this completed recall surfaced. Local collection writes —
                // cheap enough to run inline, which also guarantees a
                // maintenance cycle right after a recall sees its usage.
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                    && let Err(err) = space.record_recall_usage(&conversation.messages).await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "recall usage ledger writeback failed: {err:?}"
                    );
                }
            }
            "maintenance_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("maintenance_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // A completed model call does not attest a consumed change page.
                // Per-Watch progress is retained by Nexus in WatchState.
                // Dream self-test (plan M7): after the sleep cycle ends, probe
                // whether recent memories are actually findable; failures
                // become review SleepTasks for the next cycle.
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                {
                    // Maintenance re-encodes and merges graph memory, so a
                    // probe miss cached before the cycle could now be
                    // answerable (plan M5 invalidation).
                    if let Err(err) = space.miss_cache.clear().await {
                        log::warn!(
                            target: "brain",
                            space_id = space.id;
                            "negative-knowledge cache clear after maintenance failed: {err:?}"
                        );
                    }
                    if space.automatic && !space.engine.is_cancelled() {
                        space.kick_memory_self_test();
                    }
                }
            }
            "formation_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("formation_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // A Memory Interface receipt settles as soon as its pass
                // ends, so a misrecording is repaired without waiting for a
                // reader (MI §5).
                if matches!(
                    conversation.status,
                    ConversationStatus::Completed | ConversationStatus::Cancelled
                ) && let Some(space) = self.space()
                    && let Err(err) = space.settle_memory_conversation(conversation).await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "memory receipt settlement failed: {err}"
                    );
                }
                // New memory can answer any past miss: drop the whole
                // negative-knowledge cache (plan M5 invalidation).
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                    && let Err(err) = space.miss_cache.clear().await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "negative-knowledge cache clear failed: {err:?}"
                    );
                }
            }
            _ => {}
        }
    }

    async fn try_start_formation(&self) {
        let space = match self.space() {
            Some(space) => space,
            None => return,
        };

        if space.engine.is_cancelled() {
            return;
        }
        // A missing marker means nothing was processed yet; resume from the
        // beginning so conversations queued during maintenance are not stuck.
        let id = space.formation.get_processed().unwrap_or_default();
        if let Err(err) = space.restart_formation(SELF_USER_ID, id + 1).await {
            let reason = err.to_string();
            // "No pending ..." simply means no backlog. Anything else is a
            // transient handoff race; no retry — eviction-reload autostart or
            // the next ingest self-heals the queued backlog.
            if !reason.contains("No pending formation conversation") {
                log::warn!(
                    target: "brain",
                    space_id = space.id;
                    "formation resume failed: {reason}"
                );
            }
        }
        // Post-sleep digest: fold freshly committed wiki knowledge into the
        // graph while formation is quiet (PRD §7.3, Daydream cadence).
        #[cfg(feature = "wiki")]
        {
            if space.automatic {
                space.kick_wiki_digest();
                space.kick_wiki_housekeeping();
            }
        }
    }

    async fn try_start_maintenance(&self, formation_id: DocumentId) -> Option<DocumentId> {
        let space = match self.space() {
            Some(space) => space,
            None => return None,
        };

        if !space.automatic || space.engine.is_cancelled() {
            return None;
        }
        let at = space.maintenance.get_processed_at();
        let scope = if formation_id >= at.full + 168 {
            MaintenanceScope::Full
        } else if formation_id >= at.quick.max(at.full) + 42 {
            MaintenanceScope::Quick
        } else if formation_id >= at.daydream.max(at.quick).max(at.full) + 21 {
            MaintenanceScope::Daydream
        } else {
            return None;
        };

        let input = MaintenanceInput {
            trigger: "scheduled".to_string(),
            scope,
            timestamp: Some(rfc3339_datetime_now()),
            parameters: None,
            formation_id,
            // Filled by `Space::maintenance` once the settlement has run.
            assessment: None,
        };
        let handoff = (space.formation.processing_id() != 0).then_some(formation_id);
        let claim = space.maintenance.try_claim_after_formation(handoff)?;
        match space.maintenance_claimed(SELF_USER_ID, input, claim).await {
            Ok(rt) => rt.conversation,
            Err(err) => {
                log::error!(target: "brain", formation_id; "scheduled maintenance failed to start: {}", err);
                None
            }
        }
    }
}
// grcov-excl-stop

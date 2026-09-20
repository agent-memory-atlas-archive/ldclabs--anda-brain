use super::*;
use crate::attention::{AttentionPolicy, AttentionRuntime, AttentionTick};

impl AppState {
    /// Install trusted callbacks before any Space is loaded/shared. No default
    /// executor, recipient, authentication mapping or model provider is inferred.
    pub fn with_action_bindings(
        mut self,
        bindings: crate::action::ActionBindings,
    ) -> Result<Self, BoxError> {
        bindings.validate()?;
        if self
            .memory_runtime_bindings
            .spaces
            .values()
            .any(|s| s.actions.is_some() || s.inbox.is_some())
        {
            return Err("global and per-Space action adapters cannot both be installed".into());
        }
        if Arc::strong_count(&self.spaces) != 1
            || !self
                .spaces
                .try_read()
                .map_err(|_| "host is in use")?
                .is_empty()
        {
            return Err("configure actions before sharing the host or loading a Space".into());
        }
        self.action_bindings = Some(Arc::new(bindings));
        Ok(self)
    }
    pub fn with_attention_policy(mut self, policy: AttentionPolicy) -> Result<Self, BoxError> {
        policy.validate()?;
        if Arc::strong_count(&self.spaces) != 1
            || !self
                .spaces
                .try_read()
                .map_err(|_| "host is in use")?
                .is_empty()
        {
            return Err("configure attention before sharing the host or loading a Space".into());
        }
        self.attention_policy = policy;
        Ok(self)
    }
    /// A bounded durable pass, including separately budgeted action callbacks
    /// when explicitly installed. Learning work runs in a separate owned task
    /// with its own explicit bindings/switches. Disabled hosts are not loaded.
    pub async fn attention_tick(&self) -> Result<AttentionTick, BoxError> {
        if self.attention_directory.shard != self.sharding {
            return Err("attention shard configuration changed after host construction".into());
        }
        if !self.automatic || !self.attention_policy.enabled {
            return Ok(AttentionTick::default());
        }
        let this = self.clone();
        self.attention_directory
            .tasks
            .run(async move { this.attention_tick_inner().await })
            .await
    }
    async fn attention_tick_inner(&self) -> Result<AttentionTick, BoxError> {
        let Ok(_g) = self.attention_directory.tick_gate.try_lock() else {
            return Ok(AttentionTick {
                skipped: 1,
                ..Default::default()
            });
        };
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(self.attention_policy.wall_time_ms);
        let slots = self
            .attention_directory
            .page(self.attention_policy.spaces_per_tick)
            .await?;
        let mut report = AttentionTick::default();
        for slot in slots {
            if tokio::time::Instant::now() >= deadline {
                report.budget_exhausted = true;
                break;
            }
            report.visited += 1;
            match self.attention_directory.read_slot(slot).await {
                Ok(Some(mut row)) => {
                    let now = unix_ms();
                    let r = &row.registration;
                    let due = r.next_check_ms <= now
                        || r.dirty_generation > r.reconciled_generation
                        || now.saturating_sub(row.last_scan_ms)
                            >= self.attention_policy.reconcile_ms;
                    if !r.enabled || !due || (row.failures > 0 && r.next_check_ms > now) {
                        report.skipped += 1;
                    } else {
                        let id = r.scope.space_id.clone();
                        match self.load_space_mode(&id, false, false, false).await {
                            Ok(space) => {
                                report.loaded += 1;
                                #[cfg(feature = "learning")]
                                match space.learning.attention_reviews().await {
                                    Ok(hints) => {
                                        for hint in hints {
                                            if tokio::time::Instant::now() >= deadline {
                                                report.budget_exhausted = true;
                                                break;
                                            }
                                            space.attention.schedule_recheck(hint).await?;
                                        }
                                    }
                                    Err(err) => {
                                        report.failed += 1;
                                        log::warn!(target: "brain", space_id = id; "learning review discovery failed: {err}");
                                    }
                                }
                                match space.attention.scan(deadline).await {
                                    Ok(pass) => {
                                        report.fired += pass.fired;
                                        if pass.error.is_some() {
                                            report.failed += 1;
                                        }
                                    }
                                    Err(err) => {
                                        report.failed += 1;
                                        log::warn!(target: "brain", space_id = id; "attention pass failed: {err}");
                                    }
                                }
                                #[cfg(feature = "learning")]
                                space.learning.kick(
                                    space.memory_runtime().map(|r| r.consequences()),
                                    self.automatic,
                                );
                                space
                                    .utility
                                    .kick(space.memory_runtime().map(|r| r.consequences()));
                                space
                                    .trust
                                    .kick(space.memory_runtime().map(|r| r.consequences()));
                                if let Some(semantic) = space.attention.semantic() {
                                    semantic.kick();
                                }
                                drop(space);
                            }
                            Err(err) => {
                                report.failed += 1;
                                row.failures = row.failures.saturating_add(1);
                                row.registration.next_check_ms =
                                    now + self.attention_policy.blocked_retry_ms;
                                row.last_report.error = Some(err.to_string());
                                row.last_report.scan_complete = false;
                                self.attention_directory.finish(row).await?;
                                log::warn!(target: "brain", space_id = id; "attention Space load failed: {err}");
                            }
                        }
                    }
                }
                Ok(None) => {
                    report.skipped += 1;
                }
                Err(err) => {
                    report.failed += 1;
                    log::warn!(target: "brain", slot; "attention directory entry rejected: {err}");
                }
            }
            self.attention_directory.advance_cursor(slot).await?;
        }
        Ok(report)
    }
}

impl Space {
    pub fn attention(&self) -> Arc<AttentionRuntime> {
        self.attention.clone()
    }
}

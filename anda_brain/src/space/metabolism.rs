//! Deterministic settlement and memory health observations.
use super::*;

impl Space {
    /// The last memory-metabolism settlement report, when one has run.
    pub(super) fn memory_settlement(&self) -> Option<MemorySettlementReport> {
        self.db.get_extension_as("memory_settlement")
    }

    /// What the settlement measured, as the Maintenance prompt receives it.
    ///
    /// Both extensions predate any reader: `audit_schema` and correction
    /// discovery have been writing them since the memory-evolution plan
    /// landed, while nothing downstream ever opened them. `BrainMaintenance.md`
    /// §A.1 has meanwhile told the model that the schema census is in its
    /// input, which it was not.
    ///
    /// A `quick` or `daydream` cycle takes no census of its own, so it reads
    /// the last full cycle's — which is why `audited_at` travels with it.
    pub(crate) async fn maintenance_assessment(
        &self,
        current_settlement_error: Option<&str>,
    ) -> crate::types::MaintenanceAssessment {
        let audit: Option<SchemaAudit> = self.db.get_extension_as("schema_audit");
        let settlement = self.memory_settlement();
        let (exposures, exposures_truncated) = self.exposure_batch().await;
        let settlement_errors =
            settlement_error_messages(settlement.as_ref(), current_settlement_error);
        crate::types::MaintenanceAssessment {
            settlement_errors,
            audited_at: audit.as_ref().map(|audit| audit.audited_at),
            predicates: audit.map(|audit| audit.predicates).unwrap_or_default(),
            source_reliability: self
                .db
                .get_extension_as("source_reliability")
                .unwrap_or_default(),
            space_seq: self.current_space_seq().await,
            armed_watches: settlement::watches_in_status(self, "armed").await,
            fired_watches: settlement::watches_in_status(self, "fired").await,
            consumed_seq: None,
            revised_roots: settlement
                .map(|report| report.revised_roots)
                .unwrap_or_default(),
            exposures,
            exposures_truncated,
        }
    }

    /// The next bounded page of the exposure log, tallied per element, and
    /// whether more remain. The cursor advances once the page is handed to a
    /// cycle: reinforcement is a bounded, best-effort use of this signal, and
    /// a lost page loses nothing but a reinforcement opportunity.
    async fn exposure_batch(&self) -> (Vec<crate::types::ExposureTally>, bool) {
        use anda_cognitive_nexus::{exposure::ExposureQuery, nexus::DEFAULT_SPACE};
        const EXPOSURE_PAGE: usize = 500;
        let cursor: Option<String> = self.db.get_extension_as("exposure_cursor");
        let page = match self
            .memory
            .nexus()
            .system_session()
            .read_exposures(
                DEFAULT_SPACE,
                ExposureQuery {
                    element_id: None,
                    cursor: cursor.clone(),
                    limit: Some(EXPOSURE_PAGE),
                },
            )
            .await
        {
            Ok(page) => page,
            Err(err) => {
                log::warn!(target: "brain", space_id = self.id; "reading the exposure log failed: {err:?}");
                return (vec![], false);
            }
        };
        let mut tally: BTreeMap<String, crate::types::ExposureTally> = BTreeMap::new();
        for record in page["records"].as_array().into_iter().flatten() {
            let Some(id) = record["element_id"].as_str() else {
                continue;
            };
            let entry =
                tally
                    .entry(id.to_string())
                    .or_insert_with(|| crate::types::ExposureTally {
                        element_id: id.to_string(),
                        ..Default::default()
                    });
            match record["exposure"].as_str() {
                Some("used") => entry.used += 1,
                _ => entry.retrieved += 1,
            }
            entry.last_snapshot_seq = entry
                .last_snapshot_seq
                .max(record["snapshot_seq"].as_u64().unwrap_or(0));
        }
        // `cursor` is the position after the last delivered entry, so the
        // next cycle reads only newer entries, full page or not.
        if let Some(position) = page["cursor"].as_str()
            && Some(position) != cursor.as_deref()
        {
            self.db
                .set_extension_from("exposure_cursor".into(), position.to_string());
        }
        (
            tally.into_values().collect(),
            page["next_cursor"].is_string(),
        )
    }

    /// The Space's sequence coordinate right now.
    ///
    /// Read off the Space row rather than derived from a query: it is the
    /// `basis_seq` a refreshed `WorkingState` has to be stamped with, and a
    /// digest that guessed its own basis would be a derived view claiming a
    /// consistency it does not have.
    pub(super) async fn current_space_seq(&self) -> Option<u64> {
        use anda_cognitive_nexus::nexus::DEFAULT_SPACE;

        match self.memory.nexus().store.current_seq(DEFAULT_SPACE).await {
            Ok(seq) => Some(seq),
            Err(err) => {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "reading the Space sequence for the maintenance assessment failed: {err:?}"
                );
                None
            }
        }
    }

    /// Bumps the incrementally-updated observability counters (plan M12).
    /// Writers pay one in-memory extension update; readers never pay a
    /// heavy query.
    pub(super) fn bump_metrics(&self, update: impl FnOnce(&mut MemoryMetrics)) {
        let now_ms = unix_ms();
        let _ = self
            .db
            .set_extension_from_with("memory_metrics".to_string(), |value| {
                let mut metrics: MemoryMetrics = value.unwrap_or_default();
                update(&mut metrics);
                metrics.updated_at = now_ms;
                Some(metrics)
            });
    }

    /// Memory observability snapshot (plan M12): incrementally-maintained
    /// counters, derived rates, graph counts, and the latest module reports.
    pub async fn memory_status(&self) -> MemoryStatus {
        fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
            (denominator > 0).then(|| numerator as f64 / denominator as f64)
        }

        let metrics: MemoryMetrics = self
            .db
            .get_extension_as("memory_metrics")
            .unwrap_or_default();
        // Graph counters come from the settlement-time census (M12: readers
        // never pay heavy queries — the orphan count is a near-full scan,
        // and this endpoint is reachable anonymously on public spaces). A
        // space that has never settled reports the free in-memory counts;
        // `as_of: None` and the omitted scan-backed fields say "not yet
        // censused" without running any scan here.
        let graph = self
            .db
            .get_extension_as::<MemoryGraphCounters>("memory_graph_counters")
            .unwrap_or_else(|| MemoryGraphCounters {
                concepts: self.memory.nexus().store.concepts().len() as u64,
                propositions: self.memory.nexus().store.propositions().len() as u64,
                ..Default::default()
            });
        let maintenance_usage: Usage = self
            .db
            .get_extension_as("maintenance_usage")
            .unwrap_or_default();
        let maintenance_tokens = maintenance_usage
            .input_tokens
            .saturating_add(maintenance_usage.output_tokens);

        MemoryStatus {
            groundability: ratio(metrics.self_test_grounded, metrics.self_test_tested),
            probe_hit_rate: ratio(
                metrics.probe_hits,
                metrics.probe_hits + metrics.probe_misses,
            ),
            correction_rate: ratio(metrics.corrections, metrics.recalls_completed),
            avg_uncertainty: (metrics.uncertainty_reports > 0)
                .then(|| metrics.uncertainty_sum / metrics.uncertainty_reports as f64),
            maintenance_tokens_per_recall: ratio(maintenance_tokens, metrics.recalls_completed),
            metrics,
            graph,
            last_settlement: self.memory_settlement(),
            last_self_test: self.db.get_extension_as("memory_self_test"),
            last_shadow: self.db.get_extension_as("shadow_report"),
            last_schema_audit: self.db.get_extension_as("schema_audit"),
        }
    }

    /// Counts the graph-health numbers `memory_status` reports. Heavy (the
    /// orphan query is a near-full scan), so it runs at settlement time and
    /// the result is cached in the `memory_graph_counters` extension.
    pub(super) async fn census_graph_counters(&self, now_ms: u64) -> MemoryGraphCounters {
        let formation = self.formation_status();
        MemoryGraphCounters {
            concepts: formation.concepts as u64,
            propositions: formation.propositions as u64,
            unconsolidated: assess::kip_count_sum(self, assess::UNCONSOLIDATED_COUNT_KQL).await,
            orphans: assess::orphan_count(self).await,
            predicate_types: self.registered_predicates().await.map(|p| p.len() as u64),
            as_of: Some(now_ms),
        }
    }

    /// Per-predicate link census (plan M8), run by full-scope settlements.
    /// The counts feed the schema-sprawl metric and give the Maintenance
    /// prompt's merge guidance real numbers to look at.
    pub(super) async fn audit_schema(&self, now_ms: u64) -> Result<(), BoxError> {
        let names = self.registered_predicates().await.unwrap_or_default();

        // Serial, bounded to 50 predicates: each count is a scan and this
        // runs while the settlement lock is held, so it must not hammer the
        // graph with parallel scans.
        let mut predicates = BTreeMap::new();
        for name in names.into_iter().take(50) {
            let count = assess::kip_count(
                self,
                &format!(
                    "FIND(COUNT(?link)) WHERE {{ ?link (?s, {}, ?o) }}",
                    kip::string_literal(&name)
                ),
            )
            .await;
            // A failed count (typically the engine's full-scan cap on the
            // busiest predicates) must be *absent*, not zero: reporting the
            // most-used predicate as having zero links would point the
            // Phase-6 merge guidance at exactly the wrong target.
            match count {
                Some(count) => {
                    predicates.insert(name, count);
                }
                None => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "schema census count failed for predicate `{name}`; omitted from audit"
                    );
                }
            }
        }
        self.db.set_extension_from(
            "schema_audit".to_string(),
            SchemaAudit {
                audited_at: now_ms,
                predicates,
            },
        );
        Ok(())
    }

    /// The predicates this Space's Schema Environment declares.
    ///
    /// KIP 1.x read these off `$PropositionType` Concepts, which an ordinary
    /// write could mint; 2.0 resolves predicates from immutable Schema Packages
    /// and answers `LIST PREDICATES` from the active environment. `None` when
    /// the introspection itself failed — an empty vocabulary and an unreachable
    /// one are not the same answer.
    pub(crate) async fn registered_predicates(&self) -> Option<Vec<String>> {
        let response = self
            .execute_kip_readonly(kip::request("LIST PREDICATES LIMIT 500"))
            .await
            .ok()?;
        if !kip::succeeded(&response) {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "listing registered predicates failed: {}",
                kip::error_message(&response)
            );
            return None;
        }
        Some(
            kip::ok_result(&response)
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            entry
                                .get("local_name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    /// Ledger rows corrected after `since_ms` — the scenario-mining signal
    /// (plan M9).
    pub async fn corrected_entities(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<String>, BoxError> {
        Ok(self
            .ledger
            .corrected_since(since_ms, limit)
            .await?
            .into_iter()
            .map(|row| row.entity)
            .collect())
    }

    /// Deterministic memory settlement (plan M2/M3), run before each
    /// maintenance cycle. The passes themselves live in [`crate::settlement`],
    /// behind its `RunKip` port; this is what a Space owes them and what it
    /// does with what they decide:
    ///
    /// 1. **Correction discovery** (every scope): newly superseded links are
    ///    recorded in the ledger and aggregated per asserting actor into the
    ///    `source_reliability` extension. The scan is the settlement's; the
    ///    ledger and the extension are this Space's, so it applies them here.
    /// 2. **Watch expiry** and the **Skill lifecycle** (every scope), then
    ///    **retention expiry and the schema census** (`full` scope).
    ///
    /// There is no disuse sweep. Decay is computed, not written: the engine
    /// derives `MnemonicState.effective_strength` from the stored base, its
    /// anchor and a pinned `strength_policy` when a read is evaluated, and a
    /// missing input leaves strength unknown (Profile §6.1, §18; Spec §59.1).
    /// Nothing here reads the usage ledger back into the graph either: see
    /// the body for why recall does not reinforce what it touched.
    pub(super) async fn settle_memory_metabolism(
        &self,
        scope: MaintenanceScope,
        now_ms: u64,
    ) -> Result<MemorySettlementReport, BoxError> {
        let _guard = self.settlement_lock.lock().await;
        let mut report = MemorySettlementReport {
            settled_at: now_ms,
            ..Default::default()
        };

        // There is deliberately no reinforcement pass here.
        //
        // Until this was removed, every completed recall's touched Concepts
        // were drained out of the usage ledger and their
        // `MnemonicState.memory_strength` raised by `recall_reinforcement`.
        // That is the one thing the reference Recall policy forbids outright:
        // §1 ("Recall MUST NOT ... change memory_strength, increment recall
        // counters"), §32 ("Repeated Recall must not automatically increase
        // memory_strength/confidence/salience"), invariant 2 ("Read does not
        // reinforce memory"). Deferring the write to maintenance did not make
        // reading stop reinforcing; it only moved where the reinforcement was
        // written from.
        //
        // The ledger stays, as instrumentation: it still tells the dream
        // self-test which memories have never been exercised, still feeds
        // `entities_recalled` and the correction rate, and still supplies the
        // scenario miner. What it no longer does is close a loop back into
        // cognitive state. Reading is observed and not rewarded.
        let after = self
            .db
            .get_extension_as::<settlement::CorrectionCursor>("correction_cursor")
            .unwrap_or_else(|| {
                self.db
                    .get_extension_as::<u64>("correction_cursor")
                    .unwrap_or(0)
                    .into()
            });
        let corrections = settlement::scan_corrections(self, after.clone()).await;
        report.correction_scan_error = corrections.error;
        report.correction_scan_incomplete = corrections.incomplete;
        report.correction_scan_through_seq = corrections.watermark;
        // The derivation review's input (§57.5): what each revised root fed,
        // walked here so the cycle is handed a list rather than a guess.
        report.revised_roots = settlement::revised_roots(self, &corrections.rows).await;
        for row in corrections.rows {
            if !self
                .ledger
                .record_correction(&row.assertion, now_ms)
                .await?
            {
                continue;
            }
            report.new_corrections += 1;
            let Some(actor) = row.actor else { continue };
            let _ = self
                .db
                .set_extension_from_with("source_reliability".to_string(), |value| {
                    let mut map: BTreeMap<String, SourceReliability> = value.unwrap_or_default();
                    let entry = map.entry(actor.clone()).or_default();
                    entry.corrections += 1;
                    entry.last_corrected_at = now_ms;
                    Some(map)
                });
        }
        if corrections.cursor != after {
            self.db
                .set_extension_from("correction_cursor".to_string(), &corrections.cursor);
        }

        // Nexus checks generation, element CAS and complete authorized coverage.
        report.watches = settlement::sweep_watches(self).await;
        if let Some(error) = &report.watches.error {
            log::error!(
                target: "brain",
                space_id = self.id;
                "watch expiry failed — silence Watches are NOT firing: {error}"
            );
        }

        // Due Commitments reach attention through a commit (Profile §5.7).
        report.commitments = settlement::raise_due_commitments(self, now_ms).await;
        if let Some(error) = &report.commitments.error {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "commitment review failed — some due Commitments were not raised: {error}"
            );
        }

        report.skills = settlement::skill_settlement();
        #[cfg(feature = "learning")]
        if self.learning.is_configured() {
            match self.learning.runtime_status(self.automatic, false).await {
                Ok(status) => {
                    report.skills.unsupported_reason = Some("learning runs in the independent scheduler; no comparison verdict executed by this maintenance call".into());
                    report.skills.runtime = Some(serde_json::to_value(status)?);
                }
                Err(_) => {
                    report.skills.error = Some("learning scheduler status unavailable".into())
                }
            }
        }

        // Retention expiry, full scope only. Both halves are the host
        // deciding *when* forgetting happens; the engine only ever decided
        // what may be forgotten. They are explicit calls rather than a
        // background timer for the reason the engine declines to run one: a
        // thread that removed memory on its own schedule would act while no
        // request was in flight and no Principal was accountable for it.
        //
        // This is also what makes `SET RETENTION` mean something here. The
        // maintenance policy's retention review tells the model to set expiry
        // on what should stop being kept; until something swept on
        // `expires_at`, that write was recorded and never honoured.
        if scope == MaintenanceScope::Full {
            report.retention = self.sweep_retention().await;
            if let Some(error) = &report.retention.error {
                log::error!(
                    target: "brain",
                    space_id = self.id;
                    "retention expiry failed — lapsed records are NOT being archived: {error}"
                );
            }
        }

        // Full cycles also refresh the per-predicate schema census (plan M8).
        if scope == MaintenanceScope::Full
            && let Err(err) = self.audit_schema(now_ms).await
        {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "schema audit failed: {err:?}"
            );
        }

        self.bump_metrics(|metrics| {
            metrics.corrections += report.new_corrections;
        });
        // Refresh the cached graph counters `memory_status` serves (M12:
        // readers never pay heavy queries).
        let counters = self.census_graph_counters(now_ms).await;
        self.db
            .set_extension_from("memory_graph_counters".to_string(), counters);
        self.db
            .set_extension_from("memory_settlement_at".to_string(), now_ms);
        self.db
            .set_extension_from("memory_settlement".to_string(), report.clone());
        self.db.flush_metadata(now_ms).await.ok();
        Ok(report)
    }

    /// Acts on what this Space's own retention said should stop being kept.
    ///
    /// An element whose `retention.expires_at` has passed is archived: out of
    /// ordinary recall, still readable, still referenced. Tombstone would
    /// withdraw it from use and purge would destroy it, and neither is what an
    /// expiry date asked for. A claim whose `valid_time.until` has passed is a
    /// different clock and is not touched: its expiry is computed at read time
    /// (§14.3), so `FOR TIME` in the past still sees it.
    ///
    /// Purge is deliberately not reachable from here. §19.3 makes erasure
    /// high-impact with its own reference policy, and running it over a set the
    /// caller never enumerated would be the largest irreversible action this
    /// service can take, reached by a scheduled maintenance cycle. A forget
    /// request enumerates its target and purges that.
    ///
    /// Errors are reported rather than propagated: a settlement that could not
    /// sweep is a degraded cycle, not a failed one, and the surrounding passes
    /// have already done work worth keeping.
    pub(super) async fn sweep_retention(&self) -> crate::types::RetentionSettlement {
        use anda_cognitive_nexus::nexus::{DEFAULT_SPACE, RetentionAction};

        let mut report = crate::types::RetentionSettlement::default();
        let session = self.memory.nexus().system_session();
        #[cfg(feature = "experiments")]
        let session = if self.clock.is_manual() {
            match session.with_simulated_lifecycle_time(&kip::timestamp(self.clock.now_ms())) {
                Ok(session) => session,
                Err(error) => {
                    report.error = Some(error.to_string());
                    return report;
                }
            }
        } else {
            session
        };

        // A claim whose `valid_time` closed is not swept: KIP 2.0 computes that
        // at read time (§14.3) and stores no `expired` status. This pass only
        // archives records whose own retention lapsed.
        match session
            .sweep_expired(
                DEFAULT_SPACE,
                RetentionAction::Archive,
                settlement::SETTLEMENT_BATCH_LIMIT,
            )
            .await
        {
            Ok(sweep) => {
                report.archived = sweep.swept.len() as u64;
                report.held = sweep.held as u64;
                report.refused = sweep.refused as u64;
                report.remaining = sweep.remaining as u64;
            }
            Err(err) => report.error = Some(format!("archiving lapsed records: {err}")),
        }
        report
    }
}

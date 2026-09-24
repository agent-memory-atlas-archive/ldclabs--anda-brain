//! Owned utility calibration and replay. Native receipts and Concept updates
//! share one KML transaction; off-graph cursors are only recoverable indexes.
use super::*;
use crate::{
    recall_receipt::{MemoryPin, RecallReceipts, pin, semantic_digest},
    runtime_api::full_read,
};
use anda_cognitive_nexus::{
    CognitiveNexus, content_digest, governance::AuthContext, nexus::DEFAULT_SPACE,
};
use object_store::PutMode;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Mutex;

mod trace;
mod transaction;
pub const FORMAT: &str = "anda-brain:utility-calibration-v1";
const MAX_TARGETS: u64 = 4096;
const MAX_SAMPLES: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UtilityResult {
    pub receipt: CalibrationReceipt,
    pub evidence_ref: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UtilityStatus {
    pub configured: bool,
    pub automatic: bool,
    pub apply: bool,
    pub calibrated: bool,
    pub ranking: bool,
    pub running: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Catalog {
    allocated: u64,
    cursor: u64,
    #[serde(default)]
    source_cursor: u64,
    #[serde(default)]
    pending: Option<Target>,
}
#[derive(Clone, Serialize, Deserialize)]
struct UnitClaim {
    target: String,
    calibration_key: String,
    receipt: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    request: anda_kip::Request,
    result: UtilityResult,
    samples: Vec<String>,
    evidence: BTreeMap<String, String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Target {
    id: String,
    outcomes: BTreeSet<String>,
    consumed: BTreeSet<String>,
    last: Option<UtilityResult>,
    pending: Option<Pending>,
    rank_pin: Option<MemoryPin>,
    #[serde(default)]
    rank_evidence: BTreeMap<String, String>,
    #[serde(default)]
    applied: Option<UtilityResult>,
    #[serde(default)]
    generation: u64,
}

pub struct UtilityRuntime {
    nexus: Arc<CognitiveNexus>,
    directory: Arc<crate::journal::Journal>,
    receipts: Arc<RecallReceipts>,
    config: Option<UtilityConfig>,
    automatic: bool,
    gate: Mutex<()>,
    tasks: crate::runtime::DurableTasks,
    running: AtomicBool,
    last_error: parking_lot::RwLock<Option<String>>,
    consequences: parking_lot::RwLock<Option<std::sync::Weak<ConsequenceRuntime>>>,
    #[cfg(feature = "learning")]
    learning: Option<std::sync::Weak<crate::learning::LearningRuntime>>,
}
impl UtilityRuntime {
    pub(crate) fn new(
        nexus: Arc<CognitiveNexus>,
        store: Arc<dyn object_store::ObjectStore>,
        receipts: Arc<RecallReceipts>,
        config: Option<UtilityConfig>,
        automatic: bool,
        #[cfg(feature = "learning")] learning: Option<
            std::sync::Weak<crate::learning::LearningRuntime>,
        >,
    ) -> Arc<Self> {
        let directory = Arc::new(crate::journal::Journal::new(
            store,
            format!(
                "{}/utility/{}",
                receipts.scope.space_id,
                &receipts.scope.space_instance[7..]
            ),
        ));
        Arc::new(Self {
            nexus,
            directory,
            receipts,
            config,
            automatic,
            gate: Mutex::new(()),
            tasks: Default::default(),
            running: AtomicBool::new(false),
            last_error: Default::default(),
            consequences: Default::default(),
            #[cfg(feature = "learning")]
            learning,
        })
    }
    fn key(&self, lane: &str, id: &str) -> Result<String, BoxError> {
        crate::runtime_api::key(&self.receipts.scope, &format!("utility-{lane}"), id)
    }
    pub fn status(&self) -> UtilityStatus {
        let c = self.config.as_ref();
        UtilityStatus {
            configured: c.is_some(),
            automatic: self.automatic && c.is_some_and(|c| c.automatic),
            apply: c.is_some_and(|c| c.apply),
            calibrated: c.is_some_and(|c| c.calibrated()),
            ranking: c.is_some_and(|c| c.rank),
            running: self.running.load(Ordering::SeqCst),
            reason: if self.last_error.read().is_some() {
                self.last_error.read().clone()
            } else if c.is_none() {
                Some("utility_not_configured".into())
            } else if !c.unwrap().calibrated() {
                Some("utility_proposals_only_without_calibration".into())
            } else {
                None
            },
        }
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.tasks.is_busy()
    }
    pub(crate) async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }
    pub(crate) fn bind_consequences(&self, c: std::sync::Weak<ConsequenceRuntime>) {
        *self.consequences.write() = Some(c);
    }
    async fn correction(&self, id: &str) -> Result<Option<String>, BoxError> {
        let c = self.consequences.read().as_ref().and_then(|c| c.upgrade());
        match c {
            Some(c) => Ok(c.correction(id).await?),
            None => Ok(None),
        }
    }
    async fn save<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), BoxError> {
        let old = self.directory.read::<T>(key).await?;
        self.directory
            .put(
                key,
                value,
                old.map_or(PutMode::Create, |r| PutMode::Update(r.version)),
            )
            .await
    }
    async fn register(&self) -> Result<(), BoxError> {
        let config = self.config.as_ref().ok_or("utility is not configured")?;
        config.validate()?;
        let key = self.key("registration", &content_digest(&json!(config))?)?;
        if let Some(old) = self.directory.read::<UtilityConfig>(&key).await? {
            if content_digest(&json!(old.value))? != content_digest(&json!(config))? {
                return Err(
                    "utility contract is immutable while calibration history exists".into(),
                );
            }
        } else {
            self.directory.put(&key, config, PutMode::Create).await?;
        }
        let catalog_key = self.key("catalog", "root")?;
        if let Some(mut catalog) = self
            .directory
            .read::<Catalog>(&catalog_key)
            .await?
            .map(|r| r.value)
            && let Some(target) = catalog.pending.clone()
        {
            let key = self.key("targets", &target.id)?;
            if self.directory.read::<Target>(&key).await?.is_none() {
                self.directory.create(&key, &target).await?;
            }
            self.directory
                .create(
                    &self.key("slots", &catalog.allocated.to_string())?,
                    &target.id,
                )
                .await?;
            catalog.pending = None;
            self.save(&catalog_key, &catalog).await?;
        }
        Ok(())
    }
    async fn target(&self, id: &str) -> Result<Target, BoxError> {
        Ok(self
            .directory
            .read::<Target>(&self.key("targets", id)?)
            .await?
            .ok_or("utility target not registered")?
            .value)
    }
    /// Explicit trusted-host admission. The native chain and independent
    /// observer are re-read before any score can be updated.
    pub async fn enqueue(self: &Arc<Self>, outcome: String) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.register().await?;
                let sample = this.trace(&outcome).await?;
                let id = sample.target.id.clone();
                let key = this.key("targets", &id)?;
                let old = this.directory.read::<Target>(&key).await?;
                let mut target = old.as_ref().map(|r| r.value.clone()).unwrap_or(Target {
                    id: id.clone(),
                    outcomes: BTreeSet::new(),
                    consumed: BTreeSet::new(),
                    last: None,
                    pending: None,
                    rank_pin: None,
                    rank_evidence: BTreeMap::new(),
                    applied: None,
                    generation: 0,
                });
                if !target.outcomes.contains(&outcome) && target.outcomes.len() >= MAX_SAMPLES {
                    return Err("utility target evidence capacity reached".into());
                }
                target.outcomes.insert(outcome);
                if old.is_none() {
                    let cat_key = this.key("catalog", "root")?;
                    let mut cat = this
                        .directory
                        .read::<Catalog>(&cat_key)
                        .await?
                        .map(|r| r.value)
                        .unwrap_or_default();
                    if cat.allocated >= MAX_TARGETS {
                        return Err("utility target capacity reached".into());
                    }
                    // Publish the complete target before its numeric discovery slot.
                    // Retry repairs the reserved slot through the target locator.
                    cat.allocated += 1;
                    cat.pending = Some(target.clone());
                    this.save(&cat_key, &cat).await?;
                    this.directory
                        .put(
                            &this.key("slots", &cat.allocated.to_string())?,
                            &id,
                            PutMode::Create,
                        )
                        .await?;
                    this.save(&key, &target).await?;
                    cat.pending = None;
                    this.save(&cat_key, &cat).await?;
                } else {
                    this.save(&key, &target).await?;
                }
                Ok(())
            })
            .await
    }
    pub async fn evaluate(self: &Arc<Self>, id: String) -> Result<UtilityResult, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.register().await?;
                this.evaluate_inner(&id).await
            })
            .await
    }
    async fn evaluate_inner(&self, id: &str) -> Result<UtilityResult, BoxError> {
        let c = self.config.as_ref().ok_or("utility is not configured")?;
        let mut target = self.target(id).await?;
        if target.pending.is_some() {
            self.commit(&mut target).await?;
            return Ok(target.last.unwrap());
        }
        if target.outcomes.is_empty() {
            if let Some(applied) = &target.applied {
                target
                    .outcomes
                    .extend(applied.receipt.selected_outcomes.iter().cloned());
            } else if let Some(last) = &target.last {
                return Ok(last.clone());
            }
        }
        let mut excluded = BTreeMap::new();
        let mut samples = Vec::new();
        let mut seen = BTreeSet::new();
        let mut digests = BTreeMap::new();
        for outcome in &target.outcomes {
            match self.trace(outcome).await {
                Ok(sample) => {
                    digests.extend(sample.evidence_digests.clone());
                    if let Some(reason) = &sample.reason {
                        excluded.insert(outcome.clone(), reason.clone());
                    } else if self
                        .directory
                        .read::<UnitClaim>(&self.key("units", &sample.independent_unit)?)
                        .await?
                        .is_some_and(|u| u.value.receipt.is_some())
                    {
                        excluded
                            .insert(outcome.clone(), "already_consumed_independent_unit".into());
                    } else if !seen.insert(sample.independent_unit.clone()) {
                        excluded.insert(outcome.clone(), "duplicate_independent_root".into());
                    } else {
                        samples.push(sample);
                    }
                }
                Err(_) => {
                    excluded.insert(
                        outcome.clone(),
                        "evidence_unavailable_or_invalidated".into(),
                    );
                }
            }
        }
        let row = full_read(&self.nexus.system_session(), id).await.ok();
        let current_pin = row.as_ref().and_then(|r| pin(r).ok());
        samples.retain(|s| {
            let same = current_pin.as_ref().is_some_and(|p| {
                p.id == s.target.id && p.content_digest == s.target.content_digest
            });
            if !same {
                excluded.insert(s.outcome_ref.clone(), "memory_content_changed".into());
            }
            same
        });
        // A predeclared fixed-size group prevents arrival batching from
        // changing how often the step cap is charged. Paired evaluations are
        // already complete frozen groups, so never pool different trials.
        let batch = if c.method == AttributionMethod::PairedRevisionV1 {
            1
        } else {
            c.parameters
                .as_ref()
                .map_or(64, |p| p.minimum_independent_samples)
        };
        samples.truncate(batch);
        digests.retain(|id, _| {
            samples.iter().any(|s| s.evidence_digests.contains_key(id)) || excluded.contains_key(id)
        });
        let old = row
            .as_ref()
            .and_then(|r| r["facets"][profile!("MnemonicState")]["utility"].as_f64());
        let prior = c.parameters.as_ref().and_then(|p| p.initial_utility);
        let n = samples.iter().map(|s| s.independent_samples).sum::<usize>();
        let effect = (!samples.is_empty()).then(|| {
            samples
                .iter()
                .map(|s| s.effect.unwrap() * s.independent_samples as f64)
                .sum::<f64>()
                / n as f64
        });
        let lower = (!samples.is_empty()).then(|| {
            samples
                .iter()
                .map(|s| s.lower_bound.unwrap() * s.independent_samples as f64)
                .sum::<f64>()
                / n as f64
        });
        let upper = (!samples.is_empty()).then(|| {
            samples
                .iter()
                .map(|s| s.upper_bound.unwrap() * s.independent_samples as f64)
                .sum::<f64>()
                / n as f64
        });
        let confidence = (!samples.is_empty()).then(|| {
            (1.0 - samples
                .iter()
                .map(|s| 1.0 - s.confidence.unwrap())
                .sum::<f64>())
            .max(0.0)
        });
        let mut reason = if row.is_none() {
            Some("target_unavailable")
        } else if !id.starts_with("C-") {
            Some("mnemonic_state_requires_concept")
        } else if samples.is_empty() {
            Some("no_new_attributable_evidence")
        } else if c.parameters.is_none() {
            Some("parameters_missing")
        } else if !c.calibrated() {
            Some("calibration_missing")
        } else if !c.apply {
            Some("proposal_only")
        } else if n < c.parameters.as_ref().unwrap().minimum_independent_samples {
            Some("insufficient_independent_samples")
        } else if confidence.is_none_or(|v| v < c.parameters.as_ref().unwrap().minimum_confidence)
            || lower
                .zip(upper)
                .is_none_or(|(lo, hi)| lo <= 0.0 && hi >= 0.0)
        {
            Some("uncertain_effect")
        } else if old.or(prior).is_none() {
            Some("initial_admission_assumption_missing")
        } else {
            None
        };
        if let Some(row) = &row
            && row["schema_ref"] == profile!("SkillRevision")
            && !self.current_revision(row).await?
        {
            reason = Some("revision_is_not_current_or_eligible");
        }
        let delta = effect
            .zip(c.parameters.as_ref())
            .map(|(v, p)| (v * p.gain).clamp(-p.step_cap, p.step_cap));
        let next = if reason.is_none() {
            Some((old.or(prior).unwrap() + delta.unwrap()).clamp(0.0, 1.0))
        } else {
            old
        };
        let selected: Vec<String> = samples.iter().map(|s| s.outcome_ref.clone()).collect();
        let contract = c.contract_digest()?;
        if samples.is_empty()
            && excluded.values().all(|v| {
                v == "already_consumed_independent_unit" || v == "duplicate_independent_root"
            })
            && let Some(applied) = target.applied
        {
            return Ok(applied);
        }
        let key = content_digest(
            &json!({"target":id,"content":current_pin.as_ref().map(|p|&p.content_digest),"contract":contract,"selected":selected,"excluded":excluded,"evidence":digests}),
        )?;
        // Same selection/evidence replay is immutable, even after cursor lag.
        if let Some(previous) = self
            .directory
            .read::<UtilityResult>(&self.key("results", &key)?)
            .await?
        {
            return Ok(previous.value);
        }
        let receipt = CalibrationReceipt {
            method: c.method.clone(),
            parameters: c.parameters.clone(),
            configuration_digest: content_digest(&json!(c))?,
            comparable_scope: json!({"task_family":c.task_family,"metric":c.metric,"window":c.window,"environment_digest":c.environment_digest,"tool_versions":c.tool_versions,"observer":c.observer}),
            format: FORMAT.into(),
            calibration_key: key.clone(),
            contract_digest: contract,
            scope: self.receipts.scope.clone(),
            target: id.into(),
            target_pin: current_pin.clone(),
            selected_outcomes: selected,
            excluded,
            evidence_refs: digests.keys().cloned().collect(),
            uncertainty: json!({"method":c.method,"effect":effect,"lower_bound":lower,"upper_bound":upper,"confidence":confidence,"selection":"independent-roots-v1"}),
            independent_samples: n,
            old_value: old,
            new_value: next,
            initial_assumption: if target.applied.is_none() {
                old.or(prior)
            } else {
                None
            },
            delta,
            status: if reason.is_none() {
                "applied"
            } else {
                "no_update"
            }
            .into(),
            reason: reason.map(str::to_string),
            previous_receipt: target.last.as_ref().and_then(|r| r.evidence_ref.clone()),
        };
        let result = UtilityResult {
            receipt,
            evidence_ref: None,
        };
        let request = self
            .prepare_transaction(
                &result,
                &row,
                digests.keys().cloned().collect(),
                target.generation,
            )
            .await?;
        target.pending = Some(Pending {
            request,
            result,
            samples: if reason.is_none() {
                samples.iter().map(|s| s.independent_unit.clone()).collect()
            } else {
                vec![]
            },
            evidence: digests,
        });
        self.save(&self.key("targets", id)?, &target).await?;
        self.commit(&mut target).await?;
        Ok(target.last.unwrap())
    }
    /// Reads a receipt-backed current signal. Raw/model-written utility and
    /// stale evidence never become ranking hints.
    pub async fn rank(&self, pins: &[MemoryPin]) -> Result<BTreeMap<String, f64>, BoxError> {
        let mut result = BTreeMap::new();
        if pins.len() > 256
            || !self
                .config
                .as_ref()
                .is_some_and(|c| c.rank && c.calibrated())
        {
            return Ok(result);
        }
        for p in pins {
            let Some(target) = self
                .directory
                .read::<Target>(&self.key("targets", &p.id)?)
                .await?
                .map(|r| r.value)
            else {
                continue;
            };
            if target.pending.is_some()
                || !target
                    .rank_pin
                    .as_ref()
                    .is_some_and(|old| old.content_digest == p.content_digest)
            {
                continue;
            }
            let Some(last) = &target.applied else {
                continue;
            };
            if last.receipt.contract_digest != self.config.as_ref().unwrap().contract_digest()? {
                continue;
            }
            let Some(value) = last
                .receipt
                .new_value
                .filter(|_| last.receipt.status == "applied")
            else {
                continue;
            };
            let row = full_read(&self.nexus.system_session(), &p.id).await?;
            if semantic_digest(&row)? != p.content_digest
                || row["facets"][profile!("MnemonicState")]["utility"] != json!(value)
            {
                continue;
            }
            if row["schema_ref"] == profile!("SkillRevision")
                && !self.current_revision(&row).await?
            {
                continue;
            }
            let mut current = true;
            for (id, digest) in &target.rank_evidence {
                if id.starts_with("E-") && self.correction(id).await?.is_some() {
                    current = false;
                    break;
                }
                match full_read(&self.nexus.system_session(), id).await {
                    Ok(r) if semantic_digest(&r)? == *digest && trace::usable(&r) => {}
                    _ => {
                        current = false;
                        break;
                    }
                }
            }
            for outcome in &last.receipt.selected_outcomes {
                if !self.trace(outcome).await.is_ok_and(|s| s.reason.is_none()) {
                    current = false;
                    break;
                }
            }
            if current {
                result.insert(p.id.clone(), value);
            }
        }
        Ok(result)
    }
    pub(crate) fn kick(self: &Arc<Self>, consequences: Option<Arc<ConsequenceRuntime>>) {
        if !self.status().automatic || self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        if self
            .tasks
            .start(async move {
                let result = this.tick(consequences).await;
                *this.last_error.write() = result
                    .as_ref()
                    .err()
                    .map(|_| "utility_processing_requires_recovery".into());
                if let Err(error) = &result {
                    log::warn!(target:"brain","utility pass deferred: {error}");
                }
                this.running.store(false, Ordering::SeqCst);
                result
            })
            .is_err()
        {
            self.running.store(false, Ordering::SeqCst);
        }
    }
    async fn tick(
        self: &Arc<Self>,
        consequences: Option<Arc<ConsequenceRuntime>>,
    ) -> Result<(), BoxError> {
        let key = self.key("catalog", "root")?;
        let mut cat = {
            let _g = self.gate.lock().await;
            self.register().await?;
            self.directory
                .read::<Catalog>(&key)
                .await?
                .map(|r| r.value)
                .unwrap_or_default()
        };
        if let Some(consequences) = consequences {
            let page = consequences
                .utility_page(cat.source_cursor, 8, self.config.as_ref().unwrap())
                .await?;
            for reference in page.0 {
                self.enqueue(reference).await?;
            }
            cat = self
                .directory
                .read::<Catalog>(&key)
                .await?
                .map(|r| r.value)
                .unwrap_or_default();
            cat.source_cursor = page.1;
        }
        if cat.allocated > 0 {
            let next = cat.cursor % cat.allocated + 1;
            let id = self
                .directory
                .read::<String>(&self.key("slots", &next.to_string())?)
                .await?
                .ok_or("utility target slot missing")?
                .value;
            self.evaluate(id).await?;
            cat.cursor = next;
        }
        let _g = self.gate.lock().await;
        let mut current = self
            .directory
            .read::<Catalog>(&key)
            .await?
            .map(|r| r.value)
            .unwrap_or_default();
        current.cursor = cat.cursor;
        current.source_cursor = cat.source_cursor;
        self.save(&key, &current).await
    }
}

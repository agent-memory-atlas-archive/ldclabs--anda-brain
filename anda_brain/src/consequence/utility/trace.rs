use super::*;
use crate::recall_receipt::RecallReceiptRef;
const PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";

pub(super) fn usable(row: &Json) -> bool {
    if row
        .get("lifecycle")
        .and_then(|l| l.get("status"))
        .and_then(Json::as_str)
        .is_some_and(|s| s != "active")
    {
        return false;
    }
    // Historical execution records are evidence, not fresh execution permits.
    // Accessibility changes may expire a Decision's whole-version action pins;
    // exact delivered content and current lifecycle are checked separately.
    if let Some(end) = row["valid_time"]["end"].as_str()
        && end <= anda_cognitive_nexus::time::now().as_str()
    {
        return false;
    }
    true
}
fn refs(row: &Json, key: &str) -> Result<Vec<String>, BoxError> {
    let array = row[key].as_array().ok_or("native reference list missing")?;
    if array.len() > 128 {
        return Err("attribution reference budget exceeded".into());
    }
    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| "canonical native reference required".into())
        })
        .collect()
}
impl UtilityRuntime {
    async fn creation_seq(&self, row: &Json) -> Result<u64, BoxError> {
        Ok(self
            .nexus
            .store
            .find_transaction(
                row["_system"]["created_tx"]
                    .as_str()
                    .ok_or("native creation transaction missing")?,
            )
            .await?
            .ok_or("native creation transaction unavailable")?
            .seq)
    }
    pub(super) async fn author(&self, row: &Json) -> Result<String, BoxError> {
        let tx = row["_system"]["created_tx"]
            .as_str()
            .ok_or("native transaction missing")?;
        let tx = self
            .nexus
            .store
            .find_transaction(tx)
            .await?
            .ok_or("native transaction unavailable")?;
        if tx.space != DEFAULT_SPACE
            || tx.status != "committed"
            || !tx
                .changed_ids
                .iter()
                .any(|id| Some(id.as_str()) == row["id"].as_str())
        {
            return Err("native transaction identity mismatch".into());
        }
        tx.origin["principal_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "native writer identity unavailable".into())
    }
    pub(super) async fn current_revision(&self, row: &Json) -> Result<bool, BoxError> {
        let id = row["id"].as_str().ok_or("revision id unavailable")?;
        let skill = row["structural"][format!("{PROFILE}revision_of")]
            .as_array()
            .and_then(|v| v.first())
            .and_then(|v| v.as_str().or_else(|| v["id"].as_str()))
            .ok_or("revision lineage unavailable")?;
        let skill = full_read(&self.nexus.system_session(), skill).await?;
        Ok(usable(row)
            && row["_system"]["dependency_validity"]["action_eligible"] != false
            && skill["attributes"]["status"] != "revoked"
            && skill["structural"][format!("{PROFILE}current_revision")]
                .as_array()
                .is_some_and(|v| v.len() == 1 && (v[0] == id || v[0]["id"] == id)))
    }
    pub(super) async fn trace(&self, outcome: &str) -> Result<AttributionSample, BoxError> {
        let c = self.config.as_ref().ok_or("utility is not configured")?;
        if c.method == AttributionMethod::PairedRevisionV1 {
            #[cfg(feature = "learning")]
            {
                let rt = self
                    .learning
                    .as_ref()
                    .and_then(|l| l.upgrade())
                    .ok_or("paired learning controller is unavailable")?;
                let mut sample = rt.utility_sample(outcome, c).await?;
                for id in sample.evidence_digests.keys() {
                    if id.starts_with("E-")
                        && let Some(correction) = self.correction(id).await?
                    {
                        sample.reason = Some(format!("outcome_contested:{correction}"));
                        break;
                    }
                }
                return Ok(sample);
            }
            #[cfg(not(feature = "learning"))]
            {
                return Err("paired revision utility requires learning".into());
            }
        }
        let session = self.nexus.system_session();
        let evidence = full_read(&session, outcome).await?;
        let o = &evidence["facets"][format!("{PROFILE}OutcomeRecord")];
        let attempt_ref = o["attempt_ref"]
            .as_str()
            .ok_or("utility requires a native Outcome")?;
        let attempt = full_read(&session, attempt_ref).await?;
        let a = &attempt["facets"][format!("{PROFILE}AttemptRecord")];
        let decision_ref = a["decision_ref"]
            .as_str()
            .ok_or("native Attempt has no Decision")?;
        let decision = full_read(&session, decision_ref).await?;
        let d = &decision["facets"][format!("{PROFILE}DecisionRecord")];
        let used = refs(d, "used_refs")?;
        let applied = refs(d, "applied_revisions")?;
        let body: OutcomeInput = serde_json::from_str(
            evidence["payload"]["inline"]
                .as_str()
                .ok_or("Outcome payload missing")?,
        )?;
        let target_id = used
            .first()
            .or_else(|| applied.first())
            .cloned()
            .unwrap_or_else(|| outcome.into());
        let target = full_read(&session, &target_id).await?;
        let mut sample = AttributionSample {
            outcome_ref: outcome.into(),
            attempt_ref: attempt_ref.into(),
            decision_ref: decision_ref.into(),
            target: pin(&target)?,
            used_refs: used.clone(),
            applied_revisions: applied.clone(),
            recall_receipt: None,
            evidence_refs: vec![outcome.into(), attempt_ref.into(), decision_ref.into()],
            evidence_digests: BTreeMap::new(),
            independent_unit: outcome.into(),
            effect: None,
            lower_bound: None,
            upper_bound: None,
            confidence: None,
            independent_samples: 0,
            reason: None,
        };
        for row in [&evidence, &attempt, &decision] {
            sample
                .evidence_digests
                .insert(row["id"].as_str().unwrap().into(), semantic_digest(row)?);
        }
        if let Some(correction) = self.correction(outcome).await? {
            sample.reason = Some(format!("outcome_contested:{correction}"));
            return Ok(sample);
        }
        let reason = if self.author(&evidence).await? != c.observer.principal_id
            || self.author(&attempt).await? == c.observer.principal_id
            || self.author(&decision).await? == c.observer.principal_id
        {
            Some("observer_is_not_independent")
        } else if !usable(&evidence) || !usable(&attempt) || !usable(&decision) || !usable(&target)
        {
            Some("evidence_invalidated")
        } else if o["terminal"] != true
            || matches!(o["outcome_status"].as_str(), Some("unknown") | None)
        {
            Some("nonterminal_or_unknown_outcome")
        } else if body.correction_of.is_some() {
            Some("corrected_outcome_requires_fresh_attribution")
        } else if d["decision"] != "act" {
            Some("not_an_act_decision")
        } else if used.is_empty() {
            Some("retrieval_is_not_use")
        } else if used.len() != 1 || applied.iter().any(|id| id != &target_id) {
            Some("inseparable_bundle")
        } else if o["task_family"] != c.task_family
            || o["metric"] != c.metric
            || o["window"] != c.window
            || o["observer_config_digest"] != c.observer.configuration_digest
            || a["environment_digest"] != c.environment_digest
            || a["tool_versions"] != json!(c.tool_versions)
        {
            Some("outside_comparable_scope")
        } else {
            None
        };
        if let Some(reason) = reason {
            sample.reason = Some(reason.into());
            return Ok(sample);
        }
        // Current qualification is a host governance read, not reconstruction
        // of a credential and never used to write as the observer.
        let qualification = AuthContext::principal(&c.observer.principal_id);
        if self
            .nexus
            .session(qualification.clone())
            .effective_authority(DEFAULT_SPACE)
            .await?
            .authorize(
                anda_cognitive_nexus::governance::Permission::RecordOutcome,
                &anda_cognitive_nexus::governance::ResourceContext::default(),
                &qualification,
            )
            .into_result()
            .is_err()
        {
            sample.reason = Some("observer_no_longer_qualified".into());
            return Ok(sample);
        }
        let bound = d["rationale"]
            .as_str()
            .and_then(|r| serde_json::from_str::<Json>(r).ok())
            .and_then(|r| {
                serde_json::from_value::<RecallReceiptRef>(r["recall_receipt"].clone()).ok()
            });
        let Some(bound) = bound else {
            sample.reason = Some("decision_has_no_delivery_receipt".into());
            return Ok(sample);
        };
        let receipt = self.receipts.read(&bound).await?;
        sample.recall_receipt = Some(bound.clone());
        if !receipt.complete_inventory
            || !matches!(
                receipt.delivery.as_str(),
                "bounded_packet" | "frozen_revision"
            )
        {
            sample.reason = Some("delivery_is_not_verifiable".into());
            return Ok(sample);
        }
        let Some(delivered) = receipt.pins.iter().find(|p| p.id == target_id) else {
            sample.reason = Some("used_memory_was_not_delivered".into());
            return Ok(sample);
        };
        sample.target = delivered.clone();
        let Some(claim) = body.utility else {
            sample.reason = Some("no_independent_contribution_witness".into());
            return Ok(sample);
        };
        let w = if let (Some(reference), None) = (&claim.witness_ref, &claim.witness) {
            let witness = full_read(&session, reference).await?;
            if !reference.starts_with("E-")
                || self.author(&witness).await? != c.observer.principal_id
                || !usable(&witness)
            {
                sample.reason = Some("contribution_witness_not_qualified".into());
                return Ok(sample);
            }
            let w: ContributionWitness = serde_json::from_str(
                witness["payload"]["inline"]
                    .as_str()
                    .ok_or("contribution witness payload missing")?,
            )?;
            let witness_seq = self.creation_seq(&witness).await?;
            if witness_seq <= self.creation_seq(&attempt).await?
                || witness_seq >= self.creation_seq(&evidence).await?
            {
                sample.reason =
                    Some("witness_does_not_follow_execution_and_precede_outcome".into());
                return Ok(sample);
            }
            sample.evidence_refs.push(reference.clone());
            sample
                .evidence_digests
                .insert(reference.clone(), semantic_digest(&witness)?);
            w
        } else if let (None, Some(witness)) = (claim.witness_ref, claim.witness) {
            witness
        } else {
            sample.reason = Some("exactly_one_contribution_witness_required".into());
            return Ok(sample);
        };
        if w.format != "anda-brain:single-contribution-v1"
            || w.contract_digest != c.contract_digest()?
            || w.attempt_ref != attempt_ref
            || w.decision_ref != decision_ref
            || w.recall_receipt != bound
            || w.target != *delivered
            || !w.isolated_contribution
            || w.sampling_unit.is_empty()
            || w.sampling_unit.len() > 256
            || ![w.effect, w.lower_bound, w.upper_bound, w.confidence]
                .into_iter()
                .all(f64::is_finite)
            || !(-1.0..=1.0).contains(&w.effect)
            || w.lower_bound > w.effect
            || w.upper_bound < w.effect
            || w.lower_bound < -1.0
            || w.upper_bound > 1.0
            || !(0.0..1.0).contains(&w.confidence)
        {
            sample.reason = Some("contribution_witness_contract_mismatch".into());
            return Ok(sample);
        }
        sample.independent_unit = content_digest(&json!([
            c.observer.principal_id,
            c.environment_digest,
            w.sampling_unit
        ]))?;
        sample.effect = Some(w.effect);
        sample.lower_bound = Some(w.lower_bound);
        sample.upper_bound = Some(w.upper_bound);
        sample.confidence = Some(w.confidence);
        sample.independent_samples = 1;
        Ok(sample)
    }
}

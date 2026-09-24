//! Immutable trial activation, fixed-cutoff evaluation and independently
//! authenticated safety withdrawal. No caller supplies a selected score subset.
use super::*;
use anda_cognitive_nexus::{
    governance::Permission,
    store::{Element, eq_field},
};
use anda_db::schema::Fv;
use anda_kip::{
    ElementKind,
    cognitive::{EvaluationInput, EvaluationRule, EvaluationSamples},
};

const SAFETY_WINDOW: &str = "independent-safety-signal-v1";
const MAX_LEDGER_ROWS: usize = 131_072;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeVerdictInput {
    pub plan: PairedTrialPlan,
    pub frozen: FrozenNativePlan,
    pub trial_ref: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeVerdictKind {
    Activation,
    Settlement,
    Safety,
}

/// Persist these complete bytes before execution. A CAS conflict requires a
/// fresh authenticated preparation; an unknown commit must first be recovered.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedVerdict {
    pub space_id: String,
    pub idempotency_key: String,
    pub kind: NativeVerdictKind,
    pub skill_ref: String,
    pub revision_ref: String,
    pub trial_ref: Option<String>,
    /// The Skill's `current_trial` when this verdict was prepared, so a
    /// verdict without a trial can clear the pointer it no longer selects.
    #[serde(default)]
    pub current_trial: Option<String>,
    pub expected_skill_version: u64,
    pub from_status: String,
    pub to_status: String,
    pub cutoff: String,
    pub comparison: Json,
    pub evaluation: Json,
    pub replay: Json,
    pub replay_sources: Vec<String>,
    pub plan: Option<PairedTrialPlan>,
    pub frozen: Option<FrozenNativePlan>,
    pub safety_signal_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeVerdictReceipt {
    pub evaluation_ref: String,
    pub skill_ref: String,
    pub revision_ref: String,
    pub trial_ref: Option<String>,
    pub from_status: String,
    pub to_status: String,
    pub cutoff: String,
    pub comparison: Json,
    pub native: NativeReceipt,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSafetySignalInput {
    pub revision_ref: String,
    pub observation_key: String,
    pub observed_at: String,
    pub journal_digest: String,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSafetySignalReceipt {
    pub evidence_ref: String,
    pub native: NativeReceipt,
}

struct Ledger {
    trial: Json,
    attempts: Json,
    outcomes: Json,
    treatment: Vec<String>,
    selected_outcomes: Vec<String>,
    missing: Vec<String>,
    excluded: Vec<Json>,
}

impl NativeLearning {
    pub async fn prepare_activation(
        &self,
        input: &NativeVerdictInput,
        business_now: &str,
    ) -> Result<Option<PreparedVerdict>, KipError> {
        self.validate_frozen(&input.plan, &input.frozen).await?;
        let (revision, skill) = self
            .settlement_skill(&input.plan.candidate_revision)
            .await?;
        let status = skill["attributes"]["status"].as_str().unwrap_or("");
        if input.plan.execution.review_of.is_some() {
            self.validate_historical_acquisition(&input.plan).await?;
            if status == "adopted" {
                self.validate_review_lineage(&input.plan, &skill).await?;
                return Ok(None);
            }
        }
        if status == "trialed" {
            self.validate_closed_trial_reentry(&skill, &input.trial_ref)
                .await?;
        }
        if !matches!(status, "proposed" | "trialed" | "revoked") {
            return Err(invalid(
                "activation requires unproven or revoked standing; recover an existing activation by key",
            ));
        }
        let cutoff = canonical_time(business_now, "activation time")?;
        let ledger = self.evaluation_ledger(input, &cutoff).await?;
        if !ledger.treatment.is_empty() {
            return Err(invalid("activation must precede every treatment attempt"));
        }
        let comparison = evaluate(&input.plan, &ledger)?;
        if comparison["status"] != "insufficient" {
            return Err(invalid("activation is not an improvement verdict"));
        }
        Ok(Some(self.prepare_record(
            input,
            NativeVerdictKind::Activation,
            &revision,
            &skill,
            cutoff,
            "trialed".into(),
            ledger,
            comparison,
        )?))
    }

    pub async fn prepare_settlement(
        &self,
        input: &NativeVerdictInput,
        business_now: &str,
    ) -> Result<PreparedVerdict, KipError> {
        self.validate_frozen(&input.plan, &input.frozen).await?;
        input
            .plan
            .execution
            .validate_settlement(&input.plan.execution.cutoff, business_now)?;
        let (revision, skill) = self
            .settlement_skill(&input.plan.candidate_revision)
            .await?;
        let from = skill["attributes"]["status"].as_str().unwrap_or("");
        if input.plan.execution.review_of.is_some() {
            self.validate_historical_acquisition(&input.plan).await?;
            if from == "adopted" {
                self.validate_review_lineage(&input.plan, &skill).await?;
            } else if from != "trialed" {
                return Err(invalid("reacquisition requires a newly activated trial"));
            }
        } else if from != "trialed" {
            return Err(invalid(
                "acquisition settlement requires its prior activation",
            ));
        }
        if from == "trialed" && pointer(&skill, "current_trial") != Some(input.trial_ref.as_str()) {
            return Err(invalid(
                "acquisition settlement must use the currently activated trial",
            ));
        }
        let ledger = self
            .evaluation_ledger(input, &input.plan.execution.cutoff)
            .await?;
        let comparison = evaluate(&input.plan, &ledger)?;
        let to = target_status(from, &comparison)?.to_string();
        self.prepare_record(
            input,
            NativeVerdictKind::Settlement,
            &revision,
            &skill,
            input.plan.execution.cutoff.clone(),
            to,
            ledger,
            comparison,
        )
    }

    /// Standing admission for a particular ongoing trial. A safety withdrawal
    /// affects every job using the revision, not only the job receiving it.
    pub async fn validate_active_trial(&self, input: &NativeVerdictInput) -> Result<(), KipError> {
        self.validate_frozen(&input.plan, &input.frozen).await?;
        let (_, skill) = self
            .settlement_skill(&input.plan.candidate_revision)
            .await?;
        let trial = self
            .read_record(&self.writer(None)?, &input.trial_ref, "TrialRecord")
            .await?;
        if trial["record"]["revision_refs"] != json!(input.plan.treatment_revisions())
            || trial["record"]["parameters"] != json!(input.plan.pin()?)
            || trial["record"]["rule"] != json!(input.frozen.rule)
            || trial["record"]["evaluation_policy"] != json!(input.frozen.evaluation_policy)
        {
            return Err(invalid("active trial differs from its frozen binding"));
        }
        match skill["attributes"]["status"].as_str() {
            // `current_trial` is cleared whenever the selected revision
            // changes, and `settlement_skill` already checked the selection.
            Some("trialed")
                if pointer(&skill, "current_trial") == Some(input.trial_ref.as_str()) =>
            {
                Ok(())
            }
            Some("adopted") if input.plan.execution.review_of.is_some() => {
                self.validate_review_lineage(&input.plan, &skill).await
            }
            _ => Err(KipError::not_authorized(
                "trial is not active for the current Skill standing",
            )),
        }
    }

    /// Check whether the exact evidence behind an existing verdict remains
    /// eligible. This compares the current complete fixed-cutoff ledger with
    /// its immutable replay; it neither recomputes a score nor changes standing.
    pub async fn validate_current_verdict_evidence(
        &self,
        input: &NativeVerdictInput,
        evaluation_ref: &str,
    ) -> Result<(), KipError> {
        self.validate_plan(&input.plan)?;
        let before = self.nexus.store.get_space(&self.space_id).await?.seq;
        let evaluation = self
            .read_record(&self.writer(None)?, evaluation_ref, "EvaluationRecord")
            .await?["record"]
            .clone();
        if evaluation["trial_ref"] != input.trial_ref
            || evaluation["revision_refs"] != json!(input.plan.treatment_revisions())
            || evaluation["cutoff"] != input.plan.execution.cutoff
            || evaluation["rule_digest"] != input.frozen.rule.content_digest
            || evaluation["parameters_digest"] != input.frozen.parameters.content_digest
        {
            return Err(invalid(
                "current evidence check does not match the verdict's frozen coordinates",
            ));
        }
        let pin: ArtifactPin = serde_json::from_value(evaluation["replay_artifact"].clone())
            .map_err(|e| invalid(e.to_string()))?;
        let replay = self
            .writer(None)?
            .read_artifact(&self.space_id, &pin)
            .await?;
        let current = self
            .evaluation_ledger(input, &input.plan.execution.cutoff)
            .await?;
        if replay["rule"] != paired_rule_artifact()
            || !same(&replay["parameters"], &input.plan.artifact()?)
            || !same(&replay["trial_record"], &current.trial)
            || !same(&replay["attempts"], &current.attempts)
            || !same(&replay["outcomes"], &current.outcomes)
            || strings(&evaluation["attempt_refs"])? != current.treatment
            || strings(&evaluation["outcome_refs"])? != current.selected_outcomes
        {
            return Err(invalid(
                "verdict evidence was corrected, withdrawn, conflicted or otherwise changed",
            ));
        }
        if before != self.nexus.store.get_space(&self.space_id).await?.seq {
            return Err(KipError::version_conflict(
                "evidence changed during the current verdict read",
            ));
        }
        Ok(())
    }

    pub async fn execute_verdict(
        &self,
        prepared: &PreparedVerdict,
        business_now: &str,
    ) -> Result<NativeVerdictReceipt, KipError> {
        // A real session remains authoritative about future cutoff admissibility.
        self.execute_verdict_with(prepared, business_now, self.writer(None)?)
            .await
    }

    #[cfg(feature = "experiments")]
    pub async fn execute_verdict_simulated(
        &self,
        prepared: &PreparedVerdict,
        business_now: &str,
    ) -> Result<NativeVerdictReceipt, KipError> {
        let session = self
            .writer(None)?
            .with_simulated_evaluation_time(business_now)?;
        self.execute_verdict_with(prepared, business_now, session)
            .await
    }

    async fn execute_verdict_with(
        &self,
        prepared: &PreparedVerdict,
        business_now: &str,
        session: Session,
    ) -> Result<NativeVerdictReceipt, KipError> {
        self.validate_prepared(prepared, business_now).await?;
        session
            .put_artifact(
                &self.space_id,
                prepared.replay.clone(),
                prepared.replay_sources.clone(),
            )
            .await?;
        let request = self.verdict_request(prepared)?;
        let response = anda_kip::execute_request(&session, &request).await;
        let value = successful(&response)?;
        let handles =
            serde_json::from_value(value["handles"].clone()).map_err(|e| invalid(e.to_string()))?;
        self.verdict_receipt(
            prepared,
            NativeReceipt {
                handles,
                frozen_plan: None,
                response: Some(response),
                recovered_transaction: None,
            },
        )
    }

    pub async fn recover_verdict(
        &self,
        prepared: &PreparedVerdict,
    ) -> Result<Option<NativeVerdictReceipt>, KipError> {
        if prepared.space_id != self.space_id {
            return Err(invalid("verdict belongs to another Space"));
        }
        let request = self.verdict_request(prepared)?;
        let Some(native) = self
            .recover_settlement_request(&prepared.idempotency_key, &request, &self.writer(None)?)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(self.verdict_receipt(prepared, native)?))
    }

    // These arguments are the already validated coordinates of one frozen
    // native verdict; keeping them explicit avoids re-reading mutable state.
    #[allow(clippy::too_many_arguments)]
    fn prepare_record(
        &self,
        input: &NativeVerdictInput,
        kind: NativeVerdictKind,
        revision: &Json,
        skill: &Json,
        cutoff: String,
        to_status: String,
        ledger: Ledger,
        comparison: Json,
    ) -> Result<PreparedVerdict, KipError> {
        let from_status = skill["attributes"]["status"]
            .as_str()
            .ok_or_else(|| invalid("Skill standing missing"))?
            .to_string();
        let revision_ref = revision["id"]
            .as_str()
            .ok_or_else(|| invalid("revision identity missing"))?
            .to_string();
        let mut sources = vec![revision_ref.clone(), input.trial_ref.clone()];
        sources.extend(ledger.attempts.as_object().unwrap().keys().cloned());
        sources.extend(ledger.outcomes.as_object().unwrap().keys().cloned());
        let replay = json!({"rule":paired_rule_artifact(),"parameters":input.plan.artifact()?,
            "trial_record":ledger.trial,"attempts":ledger.attempts,"outcomes":ledger.outcomes});
        let evaluation = json!({"trial_ref":input.trial_ref,"revision_refs":[revision_ref],"from_status":from_status,"to_status":to_status,
            "rule_digest":input.frozen.rule.content_digest,"parameters_digest":input.frozen.parameters.content_digest,
            "cutoff":cutoff,"attempt_refs":ledger.treatment,"outcome_refs":ledger.selected_outcomes,
            "excluded_samples":ledger.excluded,"missing_attempt_refs":ledger.missing,"comparison":comparison,
            "replay_artifact":super::super::execution::pin(&replay)?});
        Ok(PreparedVerdict {
            space_id: self.space_id.clone(),
            idempotency_key: input.idempotency_key.clone(),
            kind,
            skill_ref: skill["id"]
                .as_str()
                .ok_or_else(|| invalid("Skill identity missing"))?
                .into(),
            revision_ref,
            trial_ref: Some(input.trial_ref.clone()),
            current_trial: pointer(skill, "current_trial").map(str::to_string),
            expected_skill_version: skill["_system"]["version"]
                .as_u64()
                .ok_or_else(|| invalid("Skill version missing"))?,
            from_status,
            to_status,
            cutoff,
            comparison,
            evaluation,
            replay,
            replay_sources: sources,
            plan: Some(input.plan.clone()),
            frozen: Some(input.frozen.clone()),
            safety_signal_ref: None,
        })
    }

    async fn evaluation_ledger(
        &self,
        input: &NativeVerdictInput,
        cutoff: &str,
    ) -> Result<Ledger, KipError> {
        let before = self.nexus.store.get_space(&self.space_id).await?.seq;
        let trial_row = self
            .read_record(&self.writer(None)?, &input.trial_ref, "TrialRecord")
            .await?;
        let trial = trial_row["record"].clone();
        if trial["revision_refs"] != json!(input.plan.treatment_revisions())
            || trial["parameters"] != json!(input.plan.pin()?)
            || trial["rule"] != json!(input.frozen.rule)
            || trial["evaluation_policy"] != json!(input.frozen.evaluation_policy)
            || !same(
                &trial["comparability"],
                &input
                    .plan
                    .comparability(&input.frozen.observer_control_digest)?,
            )
        {
            return Err(invalid("trial does not bind the frozen plan and policy"));
        }
        let baseline = strings(&trial["baseline_attempt_refs"])?;
        let baseline_outcomes = strings(&trial["baseline_outcome_refs"])?;
        let mut attempts = serde_json::Map::new();
        let mut outcome_rows = BTreeMap::new();
        let mut treatment = vec![];
        let mut selected_outcomes = vec![];
        let mut excluded = vec![];
        for row in self.learning_rows(ElementKind::Activity).await? {
            let view = anda_cognitive_nexus::view::render(&row);
            let Some(record) = view["facets"].get(format!("{PROFILE}AttemptRecord")) else {
                continue;
            };
            let reference = row.id().to_string();
            let is_baseline = baseline.contains(&reference);
            let assigned = record["trial_ref"] == input.trial_ref;
            if !is_baseline && !assigned {
                continue;
            }
            if record["started_at"].as_str().is_none_or(|s| s > cutoff)
                || record["preconditions_satisfied"] != "yes"
            {
                if is_baseline {
                    return Err(invalid("frozen baseline is outside its evaluation window"));
                }
                excluded.push(json!({"ref":reference,"reason":"outside fixed cutoff or native preconditions"}));
                continue;
            }
            let pair = record["context"]["pair_id"]
                .as_str()
                .ok_or_else(|| invalid("enrolled attempt lost pair identity"))?;
            self.validate_attempt(
                &input.plan,
                pair,
                &if is_baseline {
                    NativeArm::Baseline
                } else {
                    NativeArm::Treatment {
                        trial_ref: input.trial_ref.clone(),
                    }
                },
                record,
            )?;
            let wrapped = retained_record(&row, "AttemptRecord")?;
            if !is_baseline {
                treatment.push(reference.clone());
            }
            attempts.insert(reference, wrapped);
        }
        if baseline.len() != input.plan.pairs.len()
            || baseline.iter().any(|r| !attempts.contains_key(r))
        {
            return Err(invalid(
                "complete frozen baseline is unavailable; no comparison may be invented",
            ));
        }
        if treatment.len() > input.plan.pairs.len() {
            return Err(invalid("trial contains extra assigned attempts"));
        }
        let mut per_attempt: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in self.learning_rows(ElementKind::Evidence).await? {
            let view = anda_cognitive_nexus::view::render(&row);
            let Some(record) = view["facets"].get(format!("{PROFILE}OutcomeRecord")) else {
                continue;
            };
            let Some(attempt) = record["attempt_ref"]
                .as_str()
                .filter(|a| attempts.contains_key(*a))
            else {
                continue;
            };
            let reference = row.id().to_string();
            let is_baseline = baseline.contains(&attempt.to_string());
            let Element::Evidence(evidence) = &row else {
                unreachable!()
            };
            let eligible = record["terminal"] == true
                && record["metric"] == trial["comparability"]["metric"]
                && record["window"] == trial["observation_window"]
                && evidence.status != "corrected"
                && evidence.corrected_by.is_empty()
                && evidence.observed_at.as_str() <= cutoff;
            if !eligible {
                if baseline_outcomes.contains(&reference) {
                    return Err(invalid(
                        "frozen baseline outcome was corrected or became ineligible",
                    ));
                }
                if record["terminal"] == true {
                    excluded.push(json!({"ref":reference,"reason":"corrected, late or outside the frozen metric/window"}));
                }
                continue;
            }
            if is_baseline && !baseline_outcomes.contains(&reference) {
                return Err(invalid(
                    "baseline has a conflicting aggregate after it was frozen",
                ));
            }
            let wrapped = retained_record(&row, "OutcomeRecord")?;
            if wrapped["principal_id"] != self.observer.principal_id
                || record["observer_config_digest"] != self.observer.configuration_digest
            {
                return Err(invalid(
                    "eligible outcome lacks the configured independent observer",
                ));
            }
            per_attempt
                .entry(attempt.to_string())
                .or_default()
                .push(reference.clone());
            if !is_baseline {
                selected_outcomes.push(reference.clone());
            }
            outcome_rows.insert(reference, wrapped);
        }
        if per_attempt.values().any(|v| v.len() != 1) {
            return Err(invalid(
                "conflicting terminal aggregates require an independent adjudication rule",
            ));
        }
        if baseline_outcomes
            .iter()
            .any(|r| !outcome_rows.contains_key(r))
        {
            return Err(invalid("frozen baseline outcome is unavailable"));
        }
        let missing = treatment
            .iter()
            .filter(|r| !per_attempt.contains_key(*r))
            .cloned()
            .collect();
        if before != self.nexus.store.get_space(&self.space_id).await?.seq {
            return Err(KipError::version_conflict(
                "learning ledger changed while preparing its snapshot",
            ));
        }
        treatment.sort();
        selected_outcomes.sort();
        Ok(Ledger {
            trial,
            attempts: Json::Object(attempts),
            outcomes: json!(outcome_rows),
            treatment,
            selected_outcomes,
            missing,
            excluded,
        })
    }

    /// Scan every retained row, including withdrawn rows, so lifecycle filtering
    /// cannot silently turn a removed failed record into missing coverage. The
    /// host must have full read/history/origin authority; partial views fail.
    async fn learning_rows(&self, kind: ElementKind) -> Result<Vec<Element>, KipError> {
        let session = self.writer(None)?;
        let authority = session.effective_authority(&self.space_id).await?;
        for permission in [Permission::ReadHistory, Permission::ReadRawOrigin] {
            authority
                .authorize(permission, &ResourceContext::default(), session.auth())
                .into_result()?;
        }
        let ids = self
            .nexus
            .store
            .elements(kind)
            .query_all_ids(eq_field("space", Fv::Text(self.space_id.clone())))
            .await
            .map_err(|e| KipError::internal_error(e.to_string()))?;
        if ids.len() as usize > MAX_LEDGER_ROWS {
            return Err(invalid(
                "complete ledger exceeds the bounded host scan; no partial verdict is permitted",
            ));
        }
        let mut rows = Vec::new();
        for id in ids {
            let row = self
                .nexus
                .store
                .get_element(anda_cognitive_nexus::ElementId::new(kind, id))
                .await?;
            if row.space() != self.space_id
                || !authority
                    .may_read(&row, session.auth())
                    .is_some_and(|v| v.content && v.constraints.fields.is_empty())
            {
                return Err(KipError::not_authorized(
                    "complete learning ledger is not visible",
                ));
            }
            rows.push(row);
        }
        Ok(rows)
    }

    async fn settlement_skill(&self, revision_ref: &str) -> Result<(Json, Json), KipError> {
        let revision = self.settlement_element(revision_ref, "CONCEPT").await?;
        if revision["schema_ref"] != format!("{PROFILE}SkillRevision") {
            return Err(invalid("verdict needs an immutable SkillRevision"));
        }
        let family = revision["structural"][format!("{PROFILE}revision_of")]
            .as_array()
            .filter(|r| r.len() == 1)
            .and_then(|r| reference(&r[0]))
            .ok_or_else(|| invalid("revision family unavailable"))?;
        let skill = self.settlement_element(family, "CONCEPT").await?;
        let selected = skill["structural"][format!("{PROFILE}current_revision")]
            .as_array()
            .filter(|r| r.len() == 1)
            .and_then(|r| reference(&r[0]));
        if skill["schema_ref"] != format!("{PROFILE}Skill") || selected != Some(revision_ref) {
            return Err(KipError::version_conflict(
                "verdict revision is no longer selected",
            ));
        }
        Ok((revision, skill))
    }

    async fn settlement_element(&self, reference: &str, kind: &str) -> Result<Json, KipError> {
        let result = anda_kip::execute_request(
            &self.writer(None)?,
            &self.request(format!(
                "FIND(?element) WHERE {{?element {kind} {{id:{}}}}}",
                literal(reference)
            )),
        )
        .await;
        let values = successful(&result)?
            .as_array()
            .filter(|r| r.len() == 1)
            .ok_or_else(|| {
                KipError::not_found_or_not_visible("complete native settlement element unavailable")
            })?;
        Ok(values[0].clone())
    }

    async fn validate_closed_trial_reentry(
        &self,
        skill: &Json,
        next_trial: &str,
    ) -> Result<(), KipError> {
        let prior_trial = pointer(skill, "current_trial")
            .ok_or_else(|| invalid("previous active trial missing"))?;
        if prior_trial == next_trial {
            return Err(invalid(
                "same trial activation must recover its original receipt",
            ));
        }
        let grade = pointer(skill, "current_evaluation")
            .ok_or_else(|| invalid("previous final verdict missing"))?;
        let evaluation = self
            .read_record(&self.writer(None)?, grade, "EvaluationRecord")
            .await?;
        let trial = self
            .read_record(&self.writer(None)?, prior_trial, "TrialRecord")
            .await?;
        let pin: ArtifactPin = serde_json::from_value(trial["record"]["parameters"].clone())
            .map_err(|e| invalid(e.to_string()))?;
        let plan: PairedTrialPlan = serde_json::from_value(
            self.writer(None)?
                .read_artifact(&self.space_id, &pin)
                .await?,
        )
        .map_err(|e| invalid(e.to_string()))?;
        if evaluation["record"]["from_status"] != "trialed"
            || evaluation["record"]["to_status"] != "trialed"
            || evaluation["record"]["trial_ref"] != prior_trial
            || evaluation["record"]["cutoff"] != plan.execution.cutoff
            || !matches!(
                evaluation["record"]["comparison"]["status"].as_str(),
                Some("not_improved" | "insufficient")
            )
        {
            return Err(invalid(
                "a new trial cannot replace an unclosed active cohort",
            ));
        }
        Ok(())
    }

    async fn validate_historical_acquisition(
        &self,
        plan: &PairedTrialPlan,
    ) -> Result<(), KipError> {
        let acquisition = plan
            .execution
            .review_of
            .as_ref()
            .ok_or_else(|| invalid("acquisition is absent"))?;
        let trial = self
            .read_record(&self.writer(None)?, &acquisition.trial_ref, "TrialRecord")
            .await?;
        let evaluation = self
            .read_record(
                &self.writer(None)?,
                &acquisition.evaluation_ref,
                "EvaluationRecord",
            )
            .await?;
        acquisition.validate_records(&trial["record"], &evaluation["record"])
    }

    async fn validate_review_lineage(
        &self,
        plan: &PairedTrialPlan,
        skill: &Json,
    ) -> Result<(), KipError> {
        let acquisition = plan
            .execution
            .review_of
            .as_ref()
            .ok_or_else(|| invalid("monitoring acquisition is absent"))?;
        let trial = self
            .read_record(&self.writer(None)?, &acquisition.trial_ref, "TrialRecord")
            .await?;
        let evaluation = self
            .read_record(
                &self.writer(None)?,
                &acquisition.evaluation_ref,
                "EvaluationRecord",
            )
            .await?;
        acquisition.validate_records(&trial["record"], &evaluation["record"])?;
        let current_eval = pointer(skill, "current_evaluation")
            .ok_or_else(|| invalid("current acquisition/monitoring grade missing"))?;
        if current_eval != acquisition.evaluation_ref {
            let current = self
                .read_record(&self.writer(None)?, current_eval, "EvaluationRecord")
                .await?;
            if current["record"]["from_status"] != "adopted"
                || current["record"]["to_status"] != "adopted"
                || current["record"]["comparison"]["status"] != "improved"
            {
                return Err(invalid(
                    "monitoring acquisition is no longer the current standing lineage",
                ));
            }
            let current_trial = self
                .read_record(
                    &self.writer(None)?,
                    current["record"]["trial_ref"]
                        .as_str()
                        .ok_or_else(|| invalid("current monitoring trial missing"))?,
                    "TrialRecord",
                )
                .await?;
            let pin: ArtifactPin =
                serde_json::from_value(current_trial["record"]["parameters"].clone())
                    .map_err(|e| invalid(e.to_string()))?;
            let parameters = self
                .writer(None)?
                .read_artifact(&self.space_id, &pin)
                .await?;
            let prior: PairedTrialPlan =
                serde_json::from_value(parameters).map_err(|e| invalid(e.to_string()))?;
            if prior.execution.review_of.as_ref() != Some(acquisition) {
                return Err(invalid(
                    "monitoring cannot replace its original acquisition basis",
                ));
            }
        }
        Ok(())
    }

    async fn validate_prepared(
        &self,
        p: &PreparedVerdict,
        business_now: &str,
    ) -> Result<(), KipError> {
        if p.space_id != self.space_id
            || p.idempotency_key.trim().is_empty()
            || p.idempotency_key.len() > 512
            || p.expected_skill_version == 0
            || !matches!(
                p.from_status.as_str(),
                "proposed" | "trialed" | "adopted" | "revoked"
            )
            || !matches!(p.to_status.as_str(), "trialed" | "adopted" | "revoked")
        {
            return Err(invalid("invalid bounded prepared verdict identity"));
        }
        canonical_time(&p.cutoff, "verdict cutoff")?;
        if p.evaluation["from_status"] != p.from_status
            || p.evaluation["to_status"] != p.to_status
            || p.evaluation["revision_refs"] != json!([p.revision_ref])
            || p.evaluation["trial_ref"] != json!(p.trial_ref)
            || p.evaluation["cutoff"] != p.cutoff
            || !same(&p.evaluation["comparison"], &p.comparison)
            || p.evaluation["replay_artifact"] != json!(super::super::execution::pin(&p.replay)?)
        {
            return Err(invalid(
                "prepared verdict headline differs from immutable record bytes",
            ));
        }
        match p.kind {
            NativeVerdictKind::Safety => {
                let signal_ref = p
                    .safety_signal_ref
                    .as_deref()
                    .ok_or_else(|| invalid("independent safety signal missing"))?;
                let signal = self.validated_safety_signal(signal_ref).await?;
                if signal["payload"]["revision_ref"] != p.revision_ref
                    || !same(&p.replay["signal"], &signal)
                    || p.to_status != "revoked"
                    || p.trial_ref.is_some()
                    || p.comparison != safety_comparison(signal_ref, &signal)
                    || !strings(&p.evaluation["attempt_refs"])?.is_empty()
                    || !strings(&p.evaluation["outcome_refs"])?.is_empty()
                {
                    return Err(invalid(
                        "safety withdrawal must bind its actual independent native signal",
                    ));
                }
            }
            NativeVerdictKind::Activation | NativeVerdictKind::Settlement => {
                let plan = p
                    .plan
                    .as_ref()
                    .ok_or_else(|| invalid("verdict plan missing"))?;
                let frozen = p
                    .frozen
                    .as_ref()
                    .ok_or_else(|| invalid("verdict policy missing"))?;
                self.validate_frozen(plan, frozen).await?;
                if p.revision_ref != plan.candidate_revision
                    || p.evaluation["rule_digest"] != frozen.rule.content_digest
                    || p.evaluation["parameters_digest"] != frozen.parameters.content_digest
                    || !same(&p.replay["parameters"], &plan.artifact()?)
                    || p.replay["rule"] != paired_rule_artifact()
                {
                    return Err(invalid("verdict changed frozen rule/parameters/revision"));
                }
                let expected = super::super::PairedRule.evaluate(&EvaluationInput {
                    rule: paired_rule_artifact(),
                    parameters: plan.artifact()?,
                    trial: p.replay["trial_record"].clone(),
                    attempts: p.replay["attempts"].clone(),
                    outcomes: p.replay["outcomes"].clone(),
                    samples: EvaluationSamples::default(),
                    minimum_independent_attempts: plan.pairs.len() as u64,
                })?;
                if !same(&expected, &p.comparison) {
                    return Err(invalid(
                        "prepared comparison differs from deterministic replay",
                    ));
                }
                if p.kind == NativeVerdictKind::Activation {
                    if p.from_status == "trialed" {
                        let (_, skill) = self.settlement_skill(&p.revision_ref).await?;
                        self.validate_closed_trial_reentry(
                            &skill,
                            p.trial_ref
                                .as_deref()
                                .ok_or_else(|| invalid("activation trial missing"))?,
                        )
                        .await?;
                    }
                    if !matches!(p.from_status.as_str(), "proposed" | "trialed" | "revoked")
                        || p.to_status != "trialed"
                        || p.comparison["status"] != "insufficient"
                        || !strings(&p.evaluation["attempt_refs"])?.is_empty()
                        || canonical_time(business_now, "activation clock")? < p.cutoff
                    {
                        return Err(invalid(
                            "activation must precede treatment and cannot claim improvement",
                        ));
                    }
                } else {
                    plan.execution
                        .validate_settlement(&p.cutoff, business_now)?;
                    if target_status(&p.from_status, &p.comparison)? != p.to_status {
                        return Err(invalid(
                            "settlement changed the fixed adoption/withdrawal policy",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn verdict_request(&self, p: &PreparedVerdict) -> Result<Request, KipError> {
        if p.space_id != self.space_id || p.idempotency_key.is_empty() {
            return Err(invalid("invalid verdict Space/key"));
        }
        // The Skill's pointers select the immutable records; GradingState is
        // computed from `current_evaluation`, never written (Profile §6.2).
        let mut pointers = vec!["(\"current_evaluation\",?evaluation)".to_string()];
        let mut clear = String::new();
        match (&p.trial_ref, &p.current_trial) {
            (Some(trial), _) => pointers.push(format!("(\"current_trial\",{})", literal(trial))),
            (None, Some(old)) => {
                clear = format!("UNSET STRUCTURAL {{(\"current_trial\",{})}}", literal(old))
            }
            (None, None) => {}
        }
        let pointers = pointers.join(" ");
        let mut inputs = vec![p.revision_ref.clone()];
        inputs.extend(p.trial_ref.iter().cloned());
        inputs.extend(p.safety_signal_ref.iter().cloned());
        let edges = inputs
            .iter()
            .map(|r| format!("(\"inputs\",{})", literal(r)))
            .collect::<Vec<_>>()
            .join(" ");
        let mut request = self.request(format!(r#"MUTATE {{
            CREATE ACTIVITY ?evaluation {{SET FIELDS {{activity_class:"lifecycle_verdict",status:"completed"}} SET FACET "EvaluationRecord" {} SET STRUCTURAL {{{edges} ("outputs",{})}}}}
            UPDATE {} SET ATTRIBUTES {{status:{}}} SET STRUCTURAL {{{pointers}}} {clear}
                EXPECT VERSION {}
        }}"#,p.evaluation,literal(&p.skill_ref),literal(&p.skill_ref),literal(&p.to_status),p.expected_skill_version));
        request.operations[0].idempotency_key = Some(p.idempotency_key.clone());
        request.parameters = Some(anda_kip::Map::from_iter([(
            "brain_learning_verdict_digest".into(),
            json!(content_digest(&json!(p))?),
        )]));
        Ok(request)
    }

    fn verdict_receipt(
        &self,
        p: &PreparedVerdict,
        native: NativeReceipt,
    ) -> Result<NativeVerdictReceipt, KipError> {
        Ok(NativeVerdictReceipt {
            evaluation_ref: native
                .handles
                .get("evaluation")
                .ok_or_else(|| invalid("native EvaluationRecord handle missing"))?
                .clone(),
            skill_ref: p.skill_ref.clone(),
            revision_ref: p.revision_ref.clone(),
            trial_ref: p.trial_ref.clone(),
            from_status: p.from_status.clone(),
            to_status: p.to_status.clone(),
            cutoff: p.cutoff.clone(),
            comparison: p.comparison.clone(),
            native,
        })
    }

    pub async fn record_safety_signal(
        &self,
        input: &NativeSafetySignalInput,
        observer_auth: &AuthContext,
    ) -> Result<NativeSafetySignalReceipt, KipError> {
        let session = self.writer(Some(observer_auth))?;
        self.observer_registered().await?;
        session
            .effective_authority(&self.space_id)
            .await?
            .authorize(
                Permission::RecordOutcome,
                &ResourceContext::default(),
                observer_auth,
            )
            .into_result()?;
        let (revision, _) = self.settlement_skill(&input.revision_ref).await?;
        if revision["attributes"]["task_family"] != workflow_contract()["task_family"] {
            return Err(invalid("unsupported safety signal task family"));
        }
        let request = self.safety_request(input)?;
        let response = anda_kip::execute_request(&session, &request).await;
        let value = successful(&response)?;
        let handles =
            serde_json::from_value(value["handles"].clone()).map_err(|e| invalid(e.to_string()))?;
        self.safety_receipt(NativeReceipt {
            handles,
            frozen_plan: None,
            response: Some(response),
            recovered_transaction: None,
        })
    }

    pub async fn recover_safety_signal(
        &self,
        input: &NativeSafetySignalInput,
        observer_auth: &AuthContext,
    ) -> Result<Option<NativeSafetySignalReceipt>, KipError> {
        let session = self.writer(Some(observer_auth))?;
        let request = self.safety_request(input)?;
        self.recover_settlement_request(&input.idempotency_key, &request, &session)
            .await?
            .map(|native| self.safety_receipt(native))
            .transpose()
    }

    fn safety_request(&self, input: &NativeSafetySignalInput) -> Result<Request, KipError> {
        if !super::super::is_revision(&input.revision_ref)
            || !is_digest(&input.journal_digest)
            || input.observation_key.trim().is_empty()
            || input.observation_key.len() > 512
            || input.reason.trim().is_empty()
            || input.reason.len() > 4096
            || input.idempotency_key.is_empty()
            || input.idempotency_key.len() > 512
        {
            return Err(invalid(
                "safety signal requires bounded identity, independent report digest and reason",
            ));
        }
        canonical_time(&input.observed_at, "safety observation time")?;
        let payload = json!({"format":"anda-brain:safety-signal-v1","revision_ref":input.revision_ref,
            "journal_digest":input.journal_digest,"reason":input.reason,"observer_control":self.observer});
        let record = json!({"task_family":workflow_contract()["task_family"],"attempt_ref":null,"metric":"safety",
            "window":SAFETY_WINDOW,"terminal":true,"observation_key":input.observation_key,
            "observer_config_digest":self.observer.configuration_digest,"outcome_status":"failure"});
        let mut request = self.request(format!(r#"MUTATE {{
            CREATE EVIDENCE ?signal {{SET FIELDS {{evidence_class:"outcome",payload:{},observed_at:{}}} SET FACET "OutcomeRecord" {record}}}
            CREATE ACTIVITY ?observation {{SET FIELDS {{activity_class:"outcome_observation",status:"completed"}} SET STRUCTURAL {{("inputs",{}) ("outputs",?signal)}}}}
        }}"#,literal(&payload.to_string()),literal(&input.observed_at),literal(&input.revision_ref)));
        request.operations[0].idempotency_key = Some(input.idempotency_key.clone());
        request.parameters = Some(anda_kip::Map::from_iter([(
            "brain_learning_safety_digest".into(),
            json!(content_digest(
                &json!({"input":input,"observer":self.observer,"space":self.space_id})
            )?),
        )]));
        Ok(request)
    }

    fn safety_receipt(&self, native: NativeReceipt) -> Result<NativeSafetySignalReceipt, KipError> {
        Ok(NativeSafetySignalReceipt {
            evidence_ref: native
                .handles
                .get("signal")
                .ok_or_else(|| invalid("native safety Evidence missing"))?
                .clone(),
            native,
        })
    }

    async fn validated_safety_signal(&self, reference: &str) -> Result<Json, KipError> {
        let row = self.settlement_element(reference, "EVIDENCE").await?;
        let record = &row["facets"][format!("{PROFILE}OutcomeRecord")];
        let payload: Json = serde_json::from_str(
            row["payload"]["inline"]
                .as_str()
                .ok_or_else(|| invalid("safety signal payload unavailable"))?,
        )
        .map_err(|e| invalid(e.to_string()))?;
        if row["_system"]["origin"]["principal_id"] != self.observer.principal_id
            || row["_system"]["origin"].get("import").is_some()
            || record["observer_config_digest"] != self.observer.configuration_digest
            || record["metric"] != "safety"
            || record["window"] != SAFETY_WINDOW
            || record["terminal"] != true
            || !record["attempt_ref"].is_null()
            || record["outcome_status"] != "failure"
            || row["lifecycle"]["status"] != "active"
            || row["lifecycle"]
                .get("corrected_by")
                .and_then(Json::as_array)
                .is_some_and(|r| !r.is_empty())
            || payload["format"] != "anda-brain:safety-signal-v1"
            || payload["observer_control"] != json!(self.observer)
            || payload["journal_digest"]
                .as_str()
                .is_none_or(|s| !is_digest(s))
        {
            return Err(KipError::not_authorized(
                "safety withdrawal needs actual uncorrected configured-observer Evidence",
            ));
        }
        Ok(
            json!({"record":record,"payload":payload,"principal_id":self.observer.principal_id,"observed_at":row["observed_at"]}),
        )
    }

    pub async fn prepare_safety_revocation(
        &self,
        signal_ref: &str,
        idempotency_key: &str,
    ) -> Result<PreparedVerdict, KipError> {
        let signal = self.validated_safety_signal(signal_ref).await?;
        let revision_ref = signal["payload"]["revision_ref"]
            .as_str()
            .ok_or_else(|| invalid("safety revision missing"))?
            .to_string();
        let (_, skill) = self.settlement_skill(&revision_ref).await?;
        let cutoff = anda_cognitive_nexus::time::now();
        if signal["observed_at"]
            .as_str()
            .is_none_or(|t| t > cutoff.as_str())
        {
            return Err(invalid(
                "a future-dated safety report cannot withdraw current standing",
            ));
        }
        let comparison = safety_comparison(signal_ref, &signal);
        let safety_rule = json!({"engine":"anda-brain:independent-safety-withdrawal-v1","observer_control":self.observer});
        let parameters = json!({"revision_ref":revision_ref,"signal_ref":signal_ref});
        let replay = json!({"comparison":comparison,"signal":signal,"rule":safety_rule,"parameters":parameters});
        let from = skill["attributes"]["status"]
            .as_str()
            .ok_or_else(|| invalid("Skill standing missing"))?
            .to_string();
        let evaluation = json!({"trial_ref":null,"revision_refs":[revision_ref],"from_status":from,"to_status":"revoked",
            "rule_digest":content_digest(&safety_rule)?,"parameters_digest":content_digest(&parameters)?,"cutoff":cutoff,
            "attempt_refs":[],"outcome_refs":[],"excluded_samples":[],"missing_attempt_refs":[],"comparison":comparison,
            "replay_artifact":super::super::execution::pin(&replay)?});
        Ok(PreparedVerdict {
            space_id: self.space_id.clone(),
            idempotency_key: idempotency_key.into(),
            kind: NativeVerdictKind::Safety,
            skill_ref: skill["id"].as_str().unwrap_or("").into(),
            revision_ref: revision_ref.clone(),
            trial_ref: None,
            current_trial: pointer(&skill, "current_trial").map(str::to_string),
            expected_skill_version: skill["_system"]["version"]
                .as_u64()
                .ok_or_else(|| invalid("Skill version missing"))?,
            from_status: from,
            to_status: "revoked".into(),
            cutoff,
            comparison,
            evaluation,
            replay,
            replay_sources: vec![revision_ref, signal_ref.into()],
            plan: None,
            frozen: None,
            safety_signal_ref: Some(signal_ref.into()),
        })
    }

    async fn recover_settlement_request(
        &self,
        key: &str,
        request: &Request,
        writer: &Session,
    ) -> Result<Option<NativeReceipt>, KipError> {
        let lookup = anda_kip::execute_request(
            writer,
            &self.request(format!(
                "DESCRIBE TRANSACTION BY IDEMPOTENCY KEY {}",
                literal(key)
            )),
        )
        .await;
        let description = match successful(&lookup) {
            Ok(v) => v.clone(),
            Err(e) if e.code == KipErrorCode::TransactionUnknown => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = self
            .nexus
            .store
            .find_transaction(
                description["tx_id"]
                    .as_str()
                    .ok_or_else(|| invalid("native transaction id missing"))?,
            )
            .await?
            .ok_or_else(|| {
                KipError::outcome_unknown("described verdict journal became unavailable")
            })?;
        if row.space != self.space_id
            || row.origin["principal_id"] != writer.auth().principal_id
            || row.status != "committed"
        {
            return Err(KipError::not_authorized(
                "native verdict receipt belongs to another Space/writer",
            ));
        }
        let statement = anda_kip::parse_kml(
            request.operations[0]
                .command
                .as_deref()
                .ok_or_else(|| invalid("prepared command missing"))?,
        )?;
        if row.request_digest
            != native_digest(&json!({"statement":statement,"parameters":request.parameters}))
        {
            return Err(KipError::new(
                KipErrorCode::IdempotencyConflict,
                "verdict key committed different prepared bytes",
            ));
        }
        writer
            .effective_authority(&self.space_id)
            .await?
            .authorize(
                Permission::ReadHistory,
                &ResourceContext::default(),
                writer.auth(),
            )
            .into_result()?;
        let handles = serde_json::from_value(row.result["handles"].clone())
            .map_err(|e| invalid(e.to_string()))?;
        Ok(Some(NativeReceipt {
            handles,
            frozen_plan: None,
            response: None,
            recovered_transaction: Some(description),
        }))
    }
}

fn reference(value: &Json) -> Option<&str> {
    value.as_str().or_else(|| value["id"].as_str())
}
/// The single target of one of a Skill's structural pointers.
pub(crate) fn pointer<'a>(element: &'a Json, field: &str) -> Option<&'a str> {
    element["structural"][format!("{PROFILE}{field}")]
        .as_array()
        .filter(|refs| refs.len() == 1)
        .and_then(|refs| reference(&refs[0]))
}
fn same(a: &Json, b: &Json) -> bool {
    anda_kip::canonical_json(a) == anda_kip::canonical_json(b)
}
fn canonical_time(value: &str, label: &str) -> Result<String, KipError> {
    let normalized = anda_cognitive_nexus::time::normalize(value, label)?;
    if normalized != value {
        return Err(invalid(format!("{label} must be canonical UTC")));
    }
    Ok(normalized)
}
fn strings(value: &Json) -> Result<Vec<String>, KipError> {
    let values = value
        .as_array()
        .ok_or_else(|| invalid("reference array missing"))?;
    values
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid("non-string reference"))
        })
        .collect()
}
fn target_status(from: &str, comparison: &Json) -> Result<&'static str, KipError> {
    match (from, comparison["status"].as_str()) {
        ("trialed", Some("improved")) | ("adopted", Some("improved")) => Ok("adopted"),
        ("trialed", Some("not_improved" | "insufficient")) => Ok("trialed"),
        ("adopted", Some("not_improved" | "insufficient")) => Ok("revoked"),
        _ => Err(invalid("unsupported lifecycle/comparison settlement")),
    }
}
fn evaluate(plan: &PairedTrialPlan, ledger: &Ledger) -> Result<Json, KipError> {
    super::super::PairedRule.evaluate(&EvaluationInput {
        rule: paired_rule_artifact(),
        parameters: plan.artifact()?,
        trial: ledger.trial.clone(),
        attempts: ledger.attempts.clone(),
        outcomes: ledger.outcomes.clone(),
        samples: EvaluationSamples::default(),
        minimum_independent_attempts: plan.pairs.len() as u64,
    })
}
fn retained_record(row: &Element, name: &str) -> Result<Json, KipError> {
    let view = anda_cognitive_nexus::view::render(row);
    if row.state() != "active" || view["_system"]["origin"].get("import").is_some() {
        return Err(invalid(
            "withdrawn/imported learning material cannot silently disappear from the full ledger",
        ));
    }
    let record = view["facets"]
        .get(format!("{PROFILE}{name}"))
        .ok_or_else(|| invalid("native record facet missing"))?;
    let mut wrapped =
        json!({"record":record,"principal_id":view["_system"]["origin"]["principal_id"]});
    if let Element::Evidence(e) = row {
        wrapped["status"] = json!(e.status);
        wrapped["corrected_by"] = json!(e.corrected_by);
        wrapped["observed_at"] = json!(e.observed_at);
    }
    Ok(wrapped)
}

fn safety_comparison(reference: &str, signal: &Json) -> Json {
    json!({"status":"safety_failure","effect":null,"uncertainty":{"method":"independent-safety-signal-v1","signal_ref":reference,"journal_digest":signal["payload"]["journal_digest"],"reason":signal["payload"]["reason"]}})
}

#[cfg(test)]
mod tests;

//! Space-bound, trusted writes for native learning records.
//!
//! Persist a complete `NativeRequest` before executing it. Recovery repeats the
//! same request/key; Nexus replays its durable receipt and rejects changed bytes.
//! Business dispatch and atomic standing settlement use separate trusted seams. Actual
//! observer authentication is supplied per call, not reconstructed from config.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use anda_cognitive_nexus::{
    CognitiveNexus, content_digest,
    governance::{ANONYMOUS_PRINCIPAL, AuthContext, Permission, ResourceContext, SYSTEM_PRINCIPAL},
    nexus::Session,
};
use anda_kip::{
    Json, KipError, KipErrorCode, Request, Response, SpaceSelector, TopLevelStatus,
    cognitive::{ArtifactPin, EvaluationPolicy, ObserverControl},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    PairedTrialPlan, invalid, is_digest, paired_rule_artifact, register_paired_rule,
    workflow_contract,
};

pub mod settlement;

mod dispatch;
pub use dispatch::{
    NativeContextPin, NativeDispatchAction, NativeDispatchAuthorization, NativeDispatchInput,
    NativeDispatchReconciliation,
};

use crate::PROFILE;

/// Arm membership always becomes the native revision/trial bindings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "arm", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeArm {
    Baseline,
    Treatment { trial_ref: String },
}

impl NativeArm {
    fn revisions(&self, plan: &PairedTrialPlan) -> Vec<String> {
        match self {
            Self::Baseline => vec![],
            Self::Treatment { .. } => plan.treatment_revisions(),
        }
    }

    fn trial_ref(&self) -> Option<&str> {
        match self {
            Self::Baseline => None,
            Self::Treatment { trial_ref } => Some(trial_ref),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativePolicyPin {
    pub id: String,
    pub version: String,
    pub content_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrozenNativePlan {
    pub parameters: ArtifactPin,
    pub workflow: ArtifactPin,
    pub rule: ArtifactPin,
    pub evaluation_policy: NativePolicyPin,
    pub observer_control_digest: String,
    pub policy_control_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallPlanInput {
    pub plan: PairedTrialPlan,
    pub policy_id: String,
    pub policy_version: String,
    pub expected_policy_version: u64,
    /// Preserve explicitly authorized earlier plans during a policy update.
    pub allowed_parameters: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInput {
    pub plan: PairedTrialPlan,
    pub pair_id: String,
    pub arm: NativeArm,
    /// Actual authenticated projection basis, frozen before execution.
    pub basis: Json,
    /// Real host-read context coordinate. Context is not an action premise.
    pub context_pin: NativeContextPin,
    /// Exact revision version read at basis.snapshot_seq. Baseline has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_revision_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<super::EnrollmentOrigin>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptInput {
    pub decision: DecisionInput,
    /// Stable logical attempt identity, distinct from transport/request IDs.
    pub attempt_id: String,
    /// Native observation ordering uses real time, not a simulated task clock.
    pub started_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTrialInput {
    pub plan: PairedTrialPlan,
    pub frozen: FrozenNativePlan,
    pub basis: Json,
    pub baseline_attempt_refs: Vec<String>,
    pub baseline_outcome_refs: Vec<String>,
    /// Frozen by prepare_trial before intent persistence; never re-read on replay.
    pub baseline_attempts: Json,
    pub baseline_outcomes: Json,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeOutcomeStatus {
    Success,
    Failure,
    Unknown,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeInput {
    pub plan: PairedTrialPlan,
    pub frozen: FrozenNativePlan,
    pub pair_id: String,
    pub arm: NativeArm,
    pub decision_ref: String,
    pub attempt_ref: String,
    pub observation_key: String,
    pub observed_at: String,
    pub outcome_status: NativeOutcomeStatus,
    /// Independent verifier receipt; the host never interprets prose as success.
    pub payload: Json,
}

/// A serializable intent containing no credentials and no arbitrary KIP code.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "input", rename_all = "snake_case")]
pub enum NativeOperation {
    InstallPlan(InstallPlanInput),
    Decision(DecisionInput),
    Attempt(AttemptInput),
    OpenTrial(OpenTrialInput),
    Outcome(OutcomeInput),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRequest {
    pub space_id: String,
    pub idempotency_key: String,
    pub operation: NativeOperation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReceipt {
    pub handles: BTreeMap<String, String>,
    pub frozen_plan: Option<FrozenNativePlan>,
    /// Full native receipt, including transaction and replay information.
    pub response: Option<Response>,
    /// Authenticated journal description for read-only recovery. This is not
    /// a fabricated response from re-executing a mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_transaction: Option<Json>,
}

/// Authenticated owner of one Space's record pipeline. ObserverControl is
/// public policy, never a credential or a substitute for call authentication.
#[derive(Clone)]
pub struct NativeLearning {
    nexus: Arc<CognitiveNexus>,
    space_id: String,
    host: AuthContext,
    observer: ObserverControl,
    rule_registered: Arc<Mutex<bool>>,
}

impl NativeLearning {
    pub fn new(
        nexus: Arc<CognitiveNexus>,
        space_id: String,
        host: AuthContext,
        observer: ObserverControl,
    ) -> Result<Self, KipError> {
        if space_id.is_empty()
            || space_id.len() > 1024
            || host.principal_id.is_empty()
            || host.principal_id == ANONYMOUS_PRINCIPAL
            || observer.principal_id.is_empty()
            || observer.principal_id == host.principal_id
            || observer.principal_id == SYSTEM_PRINCIPAL
            || observer.principal_id == ANONYMOUS_PRINCIPAL
            || !is_digest(&observer.configuration_digest)
            || observer.control_domain.trim().is_empty()
            || observer.control_domain.len() > 256
        {
            return Err(invalid(
                "learning requires a Space, an authenticated host and an independent configured observer",
            ));
        }
        Ok(Self {
            nexus,
            space_id,
            host,
            observer,
            rule_registered: Arc::new(Mutex::new(false)),
        })
    }

    /// Restore once for every newly opened Nexus. Never replace a registered
    /// implementation under the same digest; an existing trusted binding stays.
    pub fn restore_rule(&self) -> Result<String, KipError> {
        let mut registered = self
            .rule_registered
            .lock()
            .map_err(|_| KipError::internal_error("learning rule registration lock poisoned"))?;
        let digest = content_digest(&paired_rule_artifact())?;
        if !*registered {
            // No supports-based shortcut: a pre-existing same-digest binding
            // may have been registered by different trusted host code.
            register_paired_rule(&self.nexus)?;
            *registered = true;
        }
        Ok(digest)
    }

    async fn observer_registered(&self) -> Result<(), KipError> {
        let row = self
            .nexus
            .governance()
            .find_principal(&self.observer.principal_id)
            .await?
            .ok_or_else(|| {
                KipError::not_authorized("configured observer Principal is not registered")
            })?;
        if row.status != "active" {
            return Err(KipError::not_authorized(
                "configured observer Principal is not active",
            ));
        }
        // Principal records have no control_domain field. The native policy
        // binds the operator's explicit attestation; names cannot prove physical
        // independence. The coordinator checks executor/controller separation.
        Ok(())
    }

    fn validate_plan(&self, plan: &PairedTrialPlan) -> Result<(), KipError> {
        plan.validate()?;
        if plan.execution.observer_configuration_digest()? != self.observer.configuration_digest {
            return Err(invalid("plan does not bind this observer configuration"));
        }
        Ok(())
    }

    fn writer(&self, observer: Option<&AuthContext>) -> Result<Session, KipError> {
        match observer {
            None => Ok(self.nexus.session(self.host.clone())),
            Some(auth) => {
                if auth.principal_id != self.observer.principal_id
                    || !auth.delegation_chain.is_empty()
                    || auth.auth_strength == "none"
                {
                    return Err(KipError::not_authorized(
                        "outcome requires the directly authenticated configured observer",
                    ));
                }
                Ok(self.nexus.session(auth.clone()))
            }
        }
    }

    /// Execute/replay a previously persisted intent. A lost response is not
    /// permission to dispatch again. All authorization is re-resolved by Nexus.
    pub async fn execute(
        &self,
        intent: &NativeRequest,
        observer_auth: Option<&AuthContext>,
    ) -> Result<NativeReceipt, KipError> {
        if intent.space_id != self.space_id
            || intent.idempotency_key.trim().is_empty()
            || intent.idempotency_key.len() > 512
        {
            return Err(invalid(
                "native learning request has wrong Space or invalid receipt key",
            ));
        }
        let is_outcome = matches!(intent.operation, NativeOperation::Outcome(_));
        if is_outcome != observer_auth.is_some() {
            return Err(KipError::not_authorized(
                "only outcome writes accept observer authentication; it is mandatory for outcomes",
            ));
        }
        let writer = self.writer(observer_auth)?;
        self.observer_registered().await?;
        let command = match &intent.operation {
            NativeOperation::InstallPlan(input) => {
                return self.install_plan(input).await;
            }
            NativeOperation::Decision(input) => self.decision_command(input)?,
            NativeOperation::Attempt(input) => self.attempt_command(input)?,
            NativeOperation::OpenTrial(input) => self.trial_command(input).await?,
            NativeOperation::Outcome(input) => self.outcome_command(input, &writer).await?,
        };
        let request = self.mutation_request(intent, command)?;
        let response = anda_kip::execute_request(&writer, &request).await;
        let value = successful(&response)?;
        let handles = serde_json::from_value(value.get("handles").cloned().unwrap_or(json!({})))
            .map_err(|e| KipError::internal_error(e.to_string()))?;
        Ok(NativeReceipt {
            handles,
            frozen_plan: None,
            response: Some(response),
            recovered_transaction: None,
        })
    }

    /// Read-only lost-commit lookup. None means the authenticated native journal
    /// explicitly returned TransactionUnknown, never a read/permission failure.
    /// Replay `execute` with the same intent to recover its original handles.
    pub async fn reconcile(
        &self,
        intent: &NativeRequest,
        observer_auth: Option<&AuthContext>,
    ) -> Result<Option<Json>, KipError> {
        if intent.space_id != self.space_id || intent.idempotency_key.trim().is_empty() {
            return Err(invalid("reconcile requires the same Space and receipt key"));
        }
        if matches!(intent.operation, NativeOperation::InstallPlan(_)) {
            return Err(invalid(
                "content-addressed artifacts and policy CAS recover by replaying install_plan",
            ));
        }
        if matches!(intent.operation, NativeOperation::Outcome(_)) != observer_auth.is_some() {
            return Err(KipError::not_authorized(
                "reconcile must use the original writer identity",
            ));
        }
        let writer = self.writer(observer_auth)?;
        let response = anda_kip::execute_request(
            &writer,
            &self.request(format!(
                "DESCRIBE TRANSACTION BY IDEMPOTENCY KEY {}",
                literal(&intent.idempotency_key)
            )),
        )
        .await;
        match successful(&response) {
            Ok(value) => Ok(Some(value.clone())),
            Err(error) if error.code == KipErrorCode::TransactionUnknown => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Recover an already committed operation without invoking any write path.
    /// Current write-policy changes cannot erase a past receipt. The caller
    /// still needs fresh direct writer identity, native read_history access
    /// and read access to the elements the transaction changed.
    /// None means the native journal explicitly reports TransactionUnknown.
    pub async fn recover_committed(
        &self,
        intent: &NativeRequest,
        observer_auth: Option<&AuthContext>,
    ) -> Result<Option<NativeReceipt>, KipError> {
        let Some(description) = self.reconcile(intent, observer_auth).await? else {
            return Ok(None);
        };
        let writer = self.writer(observer_auth)?;
        let tx_id = description["tx_id"].as_str().ok_or_else(|| {
            KipError::internal_error("native journal description lacks transaction id")
        })?;
        // DESCRIBE authenticates the Space + principal-scoped idempotency
        // lookup. Its public view omits handles, which remain in this immutable
        // journal row. Do not expose unrelated raw journal content/origin.
        let row = self
            .nexus
            .store
            .find_transaction(tx_id)
            .await?
            .ok_or_else(|| {
                KipError::outcome_unknown("described native transaction became unavailable")
            })?;
        if row.space != self.space_id
            || row.origin["principal_id"] != writer.auth().principal_id
            || row.status != "committed"
            || description["status"] != "committed"
        {
            return Err(KipError::not_authorized(
                "native receipt does not belong to this committed Space/writer operation",
            ));
        }
        let command = match &intent.operation {
            NativeOperation::InstallPlan(_) => {
                return Err(invalid(
                    "policy/artifact installation recovers by content address and CAS",
                ));
            }
            NativeOperation::Decision(input) => self.decision_command(input)?,
            NativeOperation::Attempt(input) => self.attempt_command(input)?,
            NativeOperation::OpenTrial(input) => self.format_trial(input)?,
            NativeOperation::Outcome(input) => self.format_outcome(input)?,
        };
        let request = self.mutation_request(intent, command)?;
        let statement = anda_kip::parse_kml(
            request.operations[0]
                .command
                .as_deref()
                .ok_or_else(|| invalid("native command missing"))?,
        )?;
        // Nexus request_digest is sha3-256 over this exact lowered AST and
        // merged parameter map (kml::request_digest), not artifact SHA-256.
        let expected =
            native_digest(&json!({"statement":statement,"parameters":request.parameters}));
        if row.request_digest.is_empty() || row.request_digest != expected {
            return Err(KipError::new(
                KipErrorCode::IdempotencyConflict,
                "committed native key has different original request bytes",
            ));
        }
        // Recheck at return time as well; a revocation during the separate
        // immutable-row read must not leak a receipt through the host helper.
        writer
            .effective_authority(&self.space_id)
            .await?
            .authorize(
                Permission::ReadHistory,
                &ResourceContext::default(),
                writer.auth(),
            )
            .into_result()?;
        let handles =
            serde_json::from_value(row.result.get("handles").cloned().ok_or_else(|| {
                KipError::internal_error("native commit has no original handles")
            })?)
            .map_err(|e| KipError::internal_error(e.to_string()))?;
        Ok(Some(NativeReceipt {
            handles,
            frozen_plan: None,
            response: None,
            recovered_transaction: Some(description),
        }))
    }

    fn mutation_request(
        &self,
        intent: &NativeRequest,
        command: String,
    ) -> Result<Request, KipError> {
        let mut request = self.request(command);
        request.operations[0].idempotency_key = Some(intent.idempotency_key.clone());
        // Bind the entire persisted typed intent, including host-only pins not
        // repeated in a facet. KIP includes all parameters in request_digest;
        // this is an ordinary unused parameter, not an invented schema field.
        request.parameters = Some(anda_kip::Map::from_iter([(
            "brain_learning_intent_digest".into(),
            json!(content_digest(&json!(intent))?),
        )]));
        Ok(request)
    }

    /// Read-only preflight before the coordinator persists an observer intent.
    /// Native execution still resolves current authority again at commit time.
    pub async fn validate_observer(
        &self,
        auth: &AuthContext,
        frozen: &FrozenNativePlan,
    ) -> Result<(), KipError> {
        let session = self.writer(Some(auth))?;
        self.observer_registered().await?;
        self.validate_policy(frozen).await?;
        session
            .effective_authority(&self.space_id)
            .await?
            .authorize(Permission::RecordOutcome, &ResourceContext::default(), auth)
            .into_result()?;
        Ok(())
    }

    fn request(&self, command: String) -> Request {
        Request {
            space: Some(SpaceSelector {
                id: Some(self.space_id.clone()),
                uri: None,
            }),
            ..Request::single(command)
        }
    }

    async fn install_plan(&self, input: &InstallPlanInput) -> Result<NativeReceipt, KipError> {
        self.validate_plan(&input.plan)?;
        self.restore_rule()?;
        if input.policy_id.trim().is_empty()
            || input.policy_id.len() > 256
            || input.policy_version.is_empty()
            || input.allowed_parameters.len() > 4096
            || input.allowed_parameters.iter().any(|p| !is_digest(p))
        {
            return Err(invalid(
                "invalid bounded evaluation policy identity/allowlist",
            ));
        }
        let session = self.writer(None)?;
        let workflow = session
            .put_artifact(&self.space_id, workflow_contract(), vec![])
            .await?;
        let parameters = session
            .put_artifact(
                &self.space_id,
                input.plan.artifact()?,
                input
                    .plan
                    .execution
                    .review_of
                    .as_ref()
                    .map(super::AdoptionBasis::source_refs)
                    .unwrap_or_default(),
            )
            .await?;
        let rule = session
            .put_artifact(&self.space_id, paired_rule_artifact(), vec![])
            .await?;
        let observer_control_digest = content_digest(&json!([self.observer]))?;
        let mut allowed_parameters = input.allowed_parameters.clone();
        allowed_parameters.push(parameters.content_digest.clone());
        allowed_parameters.sort();
        allowed_parameters.dedup();
        let policy = EvaluationPolicy {
            id: input.policy_id.clone(),
            version: input.policy_version.clone(),
            allowed_rules: vec![rule.content_digest.clone()],
            allowed_parameters,
            observers: vec![self.observer.clone()],
            observer_control_digest: observer_control_digest.clone(),
            minimum_independent_attempts: input.plan.pairs.len() as u64,
            allow_same_principal_observer: false,
            retain_adoption_on_insufficient: false,
        };
        let value = json!(policy);
        let old = session
            .read_control(
                &self.space_id,
                &format!("evaluation_policy/{}", input.policy_id),
                None,
            )
            .await?;
        let control = match old {
            Some(old)
                if old.version == input.expected_policy_version.saturating_add(1)
                    && anda_kip::canonical_json(&old.value) == anda_kip::canonical_json(&value) =>
            {
                old
            }
            _ => {
                session
                    .set_evaluation_policy(&self.space_id, input.expected_policy_version, policy)
                    .await?
            }
        };
        Ok(NativeReceipt {
            handles: BTreeMap::new(),
            response: None,
            recovered_transaction: None,
            frozen_plan: Some(FrozenNativePlan {
                parameters,
                workflow,
                rule,
                evaluation_policy: NativePolicyPin {
                    id: input.policy_id.clone(),
                    version: input.policy_version.clone(),
                    content_digest: content_digest(&control.value)?,
                },
                observer_control_digest,
                policy_control_version: control.version,
            }),
        })
    }

    fn decision_command(&self, input: &DecisionInput) -> Result<String, KipError> {
        self.validate_plan(&input.plan)?;
        input.plan.attempt_context(&input.pair_id)?;
        input.context_pin.validate()?;
        let revisions = input.arm.revisions(&input.plan);
        let mut retrieved = revisions.clone();
        retrieved.push(input.context_pin.id.clone());
        let mut decision = json!({"decision":"act","retrieved_refs":retrieved,"used_refs":revisions,
            "applied_revisions":revisions,"basis":input.basis});
        if let Some(origin) = &input.origin {
            origin.validate().map_err(|e| invalid(e.to_string()))?;
            decision["rationale"] = json!({"host_enrollment":origin}).to_string().into();
        }
        // groups:[] is forbidden by the native schema. A context-only group
        // records the actual basis read without pretending it is a prerequisite.
        let mut pins = vec![json!(input.context_pin)];
        match (&input.arm, input.applied_revision_version) {
            (NativeArm::Baseline, None) => {}
            (NativeArm::Treatment { .. }, Some(version))
                if (1..=9_007_199_254_740_991).contains(&version) =>
            {
                pins.push(json!({"id":input.plan.candidate_revision,"version":version}));
            }
            _ => {
                return Err(invalid(
                    "decision must pin its exact applied revision read, and baseline must not invent one",
                ));
            }
        }
        let dependency = json!({"basis_seq":input.basis["snapshot_seq"],"policy_basis":input.basis,
            "groups":[{"role":"context","pins":pins}]});
        let inputs = retrieved
            .iter()
            .map(|r| format!("(\"inputs\",{})", literal(r)))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!(
            r#"CREATE ACTIVITY ?decision {{SET FIELDS {{activity_class:"action_gate",status:"completed"}} SET FACET "DecisionRecord" {decision} SET FACET "DependencyBasis" {dependency} SET STRUCTURAL {{{inputs}}}}}"#
        ))
    }

    fn attempt_command(&self, input: &AttemptInput) -> Result<String, KipError> {
        let d = &input.decision;
        self.validate_plan(&d.plan)?;
        let context = d.plan.attempt_context(&d.pair_id)?;
        if input.attempt_id.is_empty() || input.attempt_id.len() > 512 {
            return Err(invalid("invalid bounded attempt id"));
        }
        let mut attempt = json!({"attempt_id":input.attempt_id,
            "applied_revisions":d.arm.revisions(&d.plan),"trial_ref":d.arm.trial_ref(),"context":context,
            "environment_digest":d.plan.environment_digest,"tool_versions":d.plan.tool_versions,
            "selection_policy":d.plan.pin()?,"preconditions_satisfied":"yes","started_at":input.started_at}).to_string();
        attempt.pop();
        attempt.push_str(",\"decision_ref\":?decision}");
        let decision = self.decision_command(d)?;
        let summary = literal(&format!(
            "Learning controller dispatch: {}",
            input.attempt_id
        ));
        Ok(format!(
            r#"MUTATE {{ {decision}
            CREATE CONCEPT ?task {{TYPE "SleepTask" SET ATTRIBUTES {{task_class:"review_skill",summary:{summary},status:"pending"}}}}
            CREATE ACTIVITY ?attempt {{SET FIELDS {{activity_class:"action_attempt",status:"completed"}} SET FACET "AttemptRecord" {attempt} SET STRUCTURAL {{("inputs",?decision) ("outputs",?task)}}}}
        }}"#
        ))
    }

    /// Read-only preparation. The coordinator persists this returned value
    /// before execute writes either its replay artifact or native TrialRecord.
    pub async fn prepare_trial(
        &self,
        plan: PairedTrialPlan,
        frozen: FrozenNativePlan,
        basis: Json,
        baseline_attempt_refs: Vec<String>,
        baseline_outcome_refs: Vec<String>,
    ) -> Result<OpenTrialInput, KipError> {
        self.validate_frozen(&plan, &frozen).await?;
        if baseline_attempt_refs.len() != plan.pairs.len()
            || baseline_outcome_refs.len() > plan.pairs.len()
        {
            return Err(invalid(
                "trial requires complete bounded baseline enrollment",
            ));
        }
        let session = self.writer(None)?;
        let mut attempts = serde_json::Map::new();
        let mut outcomes = serde_json::Map::new();
        let mut pairs = std::collections::BTreeSet::new();
        for reference in &baseline_attempt_refs {
            let row = self
                .read_record(&session, reference, "AttemptRecord")
                .await?;
            let pair = row["record"]["context"]["pair_id"]
                .as_str()
                .ok_or_else(|| invalid("missing baseline pair"))?;
            self.validate_attempt(&plan, pair, &NativeArm::Baseline, &row["record"])?;
            if !pairs.insert(pair.to_string()) {
                return Err(invalid("duplicate baseline pair"));
            }
            attempts.insert(reference.clone(), row);
        }
        for reference in &baseline_outcome_refs {
            let row = self
                .read_record(&session, reference, "OutcomeRecord")
                .await?;
            if !baseline_attempt_refs
                .iter()
                .any(|r| row["record"]["attempt_ref"] == *r)
            {
                return Err(invalid("baseline outcome belongs to another attempt"));
            }
            outcomes.insert(reference.clone(), row);
        }
        Ok(OpenTrialInput {
            plan,
            frozen,
            basis,
            baseline_attempt_refs,
            baseline_outcome_refs,
            baseline_attempts: Json::Object(attempts),
            baseline_outcomes: Json::Object(outcomes),
        })
    }

    pub(super) async fn validate_frozen(
        &self,
        plan: &PairedTrialPlan,
        frozen: &FrozenNativePlan,
    ) -> Result<(), KipError> {
        self.validate_plan(plan)?;
        if frozen.parameters != plan.pin()?
            || frozen.workflow != plan.execution.workflow
            || frozen.rule != super::execution::pin(&paired_rule_artifact())?
            || frozen.observer_control_digest != content_digest(&json!([self.observer]))?
        {
            return Err(invalid(
                "native request does not match its frozen plan/rule/observer pins",
            ));
        }
        self.validate_policy(frozen).await
    }

    async fn validate_policy(&self, frozen: &FrozenNativePlan) -> Result<(), KipError> {
        let policy = self
            .writer(None)?
            .read_control(
                &self.space_id,
                &format!("evaluation_policy/{}", frozen.evaluation_policy.id),
                None,
            )
            .await?
            .ok_or_else(|| KipError::not_authorized("frozen evaluation policy is unavailable"))?;
        if policy.version != frozen.policy_control_version
            || policy.value["version"] != frozen.evaluation_policy.version
            || content_digest(&policy.value)? != frozen.evaluation_policy.content_digest
            || policy.value["observer_control_digest"] != content_digest(&json!([self.observer]))?
            || !policy.value["allowed_parameters"]
                .as_array()
                .is_some_and(|p| p.contains(&json!(frozen.parameters.content_digest)))
            || !policy.value["allowed_rules"]
                .as_array()
                .is_some_and(|p| p.contains(&json!(frozen.rule.content_digest)))
        {
            return Err(KipError::version_conflict(
                "current evaluation policy differs from frozen policy",
            ));
        }
        Ok(())
    }

    async fn trial_command(&self, input: &OpenTrialInput) -> Result<String, KipError> {
        self.validate_frozen(&input.plan, &input.frozen).await?;
        if input.baseline_attempt_refs.len() != input.plan.pairs.len()
            || input.baseline_outcome_refs.len() > input.plan.pairs.len()
            || !input.baseline_attempts.is_object()
            || !input.baseline_outcomes.is_object()
        {
            return Err(invalid(
                "trial does not match its complete frozen enrollment",
            ));
        }
        let session = self.writer(None)?;
        let mut sources = input.plan.treatment_revisions();
        sources.extend(input.baseline_attempt_refs.clone());
        sources.extend(input.baseline_outcome_refs.clone());
        session
            .put_artifact(&self.space_id, trial_replay(input)?, sources)
            .await?;
        self.format_trial(input)
    }

    fn format_trial(&self, input: &OpenTrialInput) -> Result<String, KipError> {
        self.validate_plan(&input.plan)?;
        let replay = super::execution::pin(&trial_replay(input)?)?;
        let trial = json!({"revision_refs":input.plan.treatment_revisions(),"basis":input.basis,
            "rule":input.frozen.rule,"parameters":input.frozen.parameters,
            "baseline_attempt_refs":input.baseline_attempt_refs,"baseline_outcome_refs":input.baseline_outcome_refs,
            "comparability":input.plan.comparability(&input.frozen.observer_control_digest)?,"quota":input.plan.pairs.len(),
            "observation_window":input.plan.observation_window,"replay_artifact":replay,
            "evaluation_policy":input.frozen.evaluation_policy});
        Ok(format!(
            r#"CREATE ACTIVITY ?trial {{SET FIELDS {{activity_class:"trial_open",status:"completed"}} SET FACET "TrialRecord" {trial} SET STRUCTURAL {{("inputs",{})}}}}"#,
            literal(&input.plan.candidate_revision)
        ))
    }

    fn validate_attempt(
        &self,
        plan: &PairedTrialPlan,
        pair: &str,
        arm: &NativeArm,
        record: &Json,
    ) -> Result<(), KipError> {
        if record["context"] != plan.attempt_context(pair)?
            || record["selection_policy"] != json!(plan.pin()?)
            || record["environment_digest"] != plan.environment_digest
            || record["tool_versions"] != json!(plan.tool_versions)
            || record["applied_revisions"] != json!(arm.revisions(plan))
            || record["trial_ref"] != json!(arm.trial_ref())
        {
            return Err(invalid(
                "native attempt is outside the frozen pair/arm/revision/Space contract",
            ));
        }
        Ok(())
    }

    async fn outcome_command(
        &self,
        input: &OutcomeInput,
        observer: &Session,
    ) -> Result<String, KipError> {
        self.validate_frozen(&input.plan, &input.frozen).await?;
        let row = self
            .read_record(observer, &input.attempt_ref, "AttemptRecord")
            .await?;
        self.validate_attempt(&input.plan, &input.pair_id, &input.arm, &row["record"])?;
        if row["record"]["decision_ref"] != input.decision_ref
            || input.observation_key.is_empty()
            || input.observation_key.len() > 512
            || serde_json::to_vec(&input.payload)
                .map_err(|e| invalid(e.to_string()))?
                .len()
                > 1024 * 1024
        {
            return Err(invalid(
                "outcome receipt has wrong decision or invalid bounded identity/payload",
            ));
        }
        self.format_outcome(input)
    }

    fn format_outcome(&self, input: &OutcomeInput) -> Result<String, KipError> {
        self.validate_plan(&input.plan)?;
        let outcome = json!({"task_family":input.plan.task_family,"attempt_ref":input.attempt_ref,
            "metric":"success","window":input.plan.observation_window,"terminal":true,
            "observation_key":input.observation_key,"observer_config_digest":self.observer.configuration_digest,
            "outcome_status":input.outcome_status});
        // Inline Evidence payload is a string in KIP Core; retain exact JSON
        // verifier bytes inside it without inventing OutcomeRecord cost fields.
        let payload =
            literal(&serde_json::to_string(&input.payload).map_err(|e| invalid(e.to_string()))?);
        Ok(format!(
            r#"MUTATE {{
            CREATE EVIDENCE ?outcome {{SET FIELDS {{evidence_class:"outcome",payload:{payload},observed_at:{}}} SET FACET "OutcomeRecord" {outcome}}}
            CREATE ACTIVITY ?observation {{SET FIELDS {{activity_class:"outcome_observation",status:"completed"}} SET STRUCTURAL {{("inputs",{}) ("inputs",{}) ("outputs",?outcome)}}}}
        }}"#,
            literal(&input.observed_at),
            literal(&input.decision_ref),
            literal(&input.attempt_ref)
        ))
    }

    /// Reads through the authenticated KQL boundary, retaining exactly the
    /// engine's replay shape and never trusting a caller-supplied record view.
    async fn read_record(
        &self,
        session: &Session,
        reference: &str,
        facet: &str,
    ) -> Result<Json, KipError> {
        let kind = if facet == "OutcomeRecord" {
            "EVIDENCE"
        } else {
            "ACTIVITY"
        };
        let response = anda_kip::execute_request(
            session,
            &self.request(format!(
                "FIND(?record) WHERE {{ ?record {kind} {{id:{}}} }}",
                literal(reference)
            )),
        )
        .await;
        let result = successful(&response)?;
        let row = result.as_array().and_then(|r| r.first()).ok_or_else(|| {
            KipError::not_found_or_not_visible("learning record unavailable in this Space")
        })?;
        let value = row["facets"][format!("{PROFILE}{facet}")].clone();
        if value.is_null() {
            return Err(invalid("referenced record lacks its native learning facet"));
        }
        let mut wrapped =
            json!({"record":value,"principal_id":row["_system"]["origin"]["principal_id"]});
        if facet == "OutcomeRecord" {
            wrapped["status"] = row["lifecycle"]["status"].clone();
            wrapped["corrected_by"] = row["lifecycle"]
                .get("corrected_by")
                .cloned()
                .unwrap_or(json!([]));
            wrapped["observed_at"] = row["observed_at"].clone();
        }
        Ok(wrapped)
    }
}

fn trial_replay(input: &OpenTrialInput) -> Result<Json, KipError> {
    Ok(
        json!({"rule":paired_rule_artifact(),"parameters":input.plan.artifact()?,"basis":input.basis,
        "baseline_attempts":input.baseline_attempts,"baseline_outcomes":input.baseline_outcomes}),
    )
}

fn native_digest(value: &Json) -> String {
    let bytes = ic_cose_types::cose::sha3_256(anda_kip::canonical_json(value).as_bytes());
    format!(
        "sha3-256:{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn literal(value: &str) -> String {
    // JSON string encoding is also a KIP string literal; it safely handles
    // quotes, line breaks and control bytes in authenticated receipt material.
    serde_json::to_string(value).expect("strings serialize")
}

fn successful(response: &Response) -> Result<&Json, KipError> {
    if response.status == TopLevelStatus::OutcomeUnknown {
        return Err(KipError::outcome_unknown(
            "native learning commit is uncertain; reconcile the persisted key before dispatch",
        ));
    }
    if !crate::kip::succeeded(response) {
        if let Some(error) = crate::kip::error_of(response) {
            return Err(KipError::new(
                error.code.parse().unwrap_or(KipErrorCode::InternalError),
                error.message.clone(),
            )
            .with_details(serde_json::to_value(response).unwrap_or(Json::Null)));
        }
        return Err(KipError::internal_error(
            "native learning operation did not return success",
        ));
    }
    response
        .first_result()
        .ok_or_else(|| KipError::internal_error("native learning receipt is missing"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::dispatch_tests::approve_revision as approve_revision_for_test;

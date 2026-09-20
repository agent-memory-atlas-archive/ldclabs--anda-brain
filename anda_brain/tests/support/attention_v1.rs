//! Versioned executable wire contract, compiled only by integration tests.
//!
//! This test-only oracle fixes serialization, identity, state and recovery rules.
//! It is not a queue implementation or authorization API. Production enforcement
//! is exercised by nexus_watch_handoff and the Space/runtime integration tests.
//! Field semantics require explicit format migration; the fixture is attention-v1.json.
use anda_cognitive_nexus::{ElementId, content_digest};
use anda_kip::ElementKind;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_COUNTER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Format {
    #[serde(rename = "anda-brain:attention-v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    InvalidRecord,
    UnsupportedVersion,
    ScopeMismatch,
    VersionConflict,
    GenerationConflict,
    LeaseLost,
    NotAuthorized,
    NotReady,
    BasisChanged,
    HistoryGap,
    BudgetExhausted,
    BindingUnavailable,
    SemanticUnknown,
    IdempotencyConflict,
    OutcomeUnknown,
}

type Result<T = ()> = std::result::Result<T, Code>;

fn require(ok: bool, code: Code) -> Result {
    if ok { Ok(()) } else { Err(code) }
}

fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.trim() == value
}

fn counter(value: u64) -> bool {
    (1..=MAX_COUNTER).contains(&value)
}

fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn graph_ref(value: &str, kind: ElementKind) -> bool {
    ElementId::parse_kind(value, kind).is_ok_and(|id| counter(id.seq) && id.to_string() == value)
}

pub fn runtime_ref(value: &str, kind: &str) -> bool {
    value
        .strip_prefix(&format!("{kind}/v1/"))
        .is_some_and(|hash| digest(&format!("sha256:{hash}")))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub space_id: String,
    pub space_instance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub id: String,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    pub policy: Pin,
    pub evaluator: Option<Pin>,
    pub binding: Option<Pin>,
}

impl Pins {
    pub fn validate(&self) -> Result {
        for pin in std::iter::once(&self.policy)
            .chain(self.evaluator.iter())
            .chain(self.binding.iter())
        {
            require(bounded(&pin.id) && digest(&pin.digest), Code::InvalidRecord)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    Delta { matched_seq: u64 },
    Silence { due_at: String, due_seq: u64 },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Fire {
    pub watch_ref: String,
    pub arm_generation: u64,
    pub trigger: Trigger,
}

impl Fire {
    pub fn key(&self) -> Result<String> {
        require(
            graph_ref(&self.watch_ref, ElementKind::Concept) && counter(self.arm_generation),
            Code::InvalidRecord,
        )?;
        let suffix = match &self.trigger {
            Trigger::Delta { matched_seq } => {
                require(counter(*matched_seq), Code::InvalidRecord)?;
                matched_seq.to_string()
            }
            Trigger::Silence { due_at, due_seq } => {
                require(*due_seq <= MAX_COUNTER, Code::InvalidRecord)?;
                let normalized = anda_cognitive_nexus::time::normalize(due_at, "due_at")
                    .map_err(|_| Code::InvalidRecord)?;
                // Persist one spelling; callers normalize before creating keys.
                require(&normalized == due_at, Code::InvalidRecord)?;
                format!("silence:{due_at}")
            }
        };
        Ok(format!(
            "watch_fire:{}:{}:{suffix}",
            self.watch_ref, self.arm_generation
        ))
    }

    pub fn wake_ref(&self, scope: &Scope) -> Result<String> {
        require(
            bounded(&scope.space_id) && bounded(&scope.space_instance),
            Code::InvalidRecord,
        )?;
        let hash = content_digest(
            &json!({"domain":"anda-brain:wake-v1","scope":scope,"fire_key":self.key()?}),
        )
        .map_err(|_| Code::InvalidRecord)?;
        Ok(format!("wake/v1/{}", &hash[7..]))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resume {
    At { not_before_ms: u64 },
    OnChange { condition_digest: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    pub reason: Code,
    pub resume: Resume,
}

impl Retry {
    pub fn validate(&self) -> Result {
        require(
            matches!(
                self.reason,
                Code::BasisChanged
                    | Code::HistoryGap
                    | Code::BudgetExhausted
                    | Code::BindingUnavailable
                    | Code::SemanticUnknown
                    | Code::OutcomeUnknown
            ),
            Code::InvalidRecord,
        )?;
        require(
            match &self.resume {
                Resume::At { not_before_ms } => {
                    self.reason != Code::OutcomeUnknown && counter(*not_before_ms)
                }
                Resume::OnChange { condition_digest } => digest(condition_digest),
            },
            Code::InvalidRecord,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub owner: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
pub enum WakeState {
    Pending { not_before_ms: u64 },
    Running { lease: Lease },
    Blocked { retry: Retry },
    Completed { receipt_ref: String },
    Cancelled { receipt_ref: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WakeRecord {
    pub format: Format,
    pub scope: Scope,
    pub wake_ref: String,
    pub fire: Fire,
    pub fire_activity_ref: String,
    pub pins: Pins,
    pub version: u64,
    // Retained even after a worker releases its lease: fences never reset.
    pub fence: u64,
    pub state: WakeState,
}

impl WakeRecord {
    pub fn validate(&self) -> Result {
        require(
            self.wake_ref == self.fire.wake_ref(&self.scope)?,
            Code::InvalidRecord,
        )?;
        require(
            graph_ref(&self.fire_activity_ref, ElementKind::Activity)
                && counter(self.version)
                && self.fence <= MAX_COUNTER,
            Code::InvalidRecord,
        )?;
        self.pins.validate()?;
        match &self.state {
            WakeState::Pending { not_before_ms } => {
                require(*not_before_ms <= MAX_COUNTER, Code::InvalidRecord)
            }
            WakeState::Running { lease } => require(
                counter(self.fence)
                    && lease.owner.starts_with("kip:principal:")
                    && bounded(&lease.owner)
                    && lease.owner.len() > "kip:principal:".len()
                    && counter(lease.expires_at_ms),
                Code::InvalidRecord,
            ),
            WakeState::Blocked { retry } => retry.validate(),
            WakeState::Completed { receipt_ref } | WakeState::Cancelled { receipt_ref } => {
                require(runtime_ref(receipt_ref, "receipt"), Code::InvalidRecord)
            }
        }
    }
}

/// Unsigned text cannot populate these facts. Production must obtain them from
/// current authenticated native state under the same commit lock, not JSON.
pub struct Guard<'a> {
    pub scope: &'a Scope,
    pub expected_version: u64,
    pub expected_fence: u64,
    pub current_generation: u64,
    pub principal: &'a str,
    pub now_ms: u64,
    pub authorized: bool,
    pub basis_current: bool,
    pub resume_verified: bool,
}

/// Pure transition oracle. Success here alone never acquires a lease or grants
/// action authority; Nexus must atomically compare against the persisted record.
pub fn transition(old: &WakeRecord, new: &WakeRecord, guard: Guard<'_>) -> Result {
    old.validate()?;
    new.validate()?;
    require(guard.authorized, Code::NotAuthorized)?;
    require(
        guard.scope == &old.scope && new.scope == old.scope,
        Code::ScopeMismatch,
    )?;
    require(
        guard.expected_version == old.version && new.version == old.version + 1,
        Code::VersionConflict,
    )?;
    require(guard.expected_fence == old.fence, Code::LeaseLost)?;
    require(
        old.fire == new.fire
            && old.wake_ref == new.wake_ref
            && old.fire_activity_ref == new.fire_activity_ref
            && old.pins == new.pins,
        Code::IdempotencyConflict,
    )?;
    require(
        !matches!(
            old.state,
            WakeState::Completed { .. } | WakeState::Cancelled { .. }
        ),
        Code::VersionConflict,
    )?;
    // Host cancellation fences even a live worker. Its receipt must separately
    // retain any unresolved dispatch; cancellation is not evidence of no effect.
    if matches!(new.state, WakeState::Cancelled { .. }) {
        return require(new.fence == old.fence + 1, Code::LeaseLost);
    }
    // Recording why work cannot continue must remain possible after its basis
    // changes. This never produces action outputs or renews the old lease.
    if let (WakeState::Running { lease }, WakeState::Blocked { .. }) = (&old.state, &new.state) {
        return require(
            lease.owner == guard.principal
                && lease.expires_at_ms > guard.now_ms
                && new.fence == old.fence,
            Code::LeaseLost,
        );
    }
    require(guard.basis_current, Code::BasisChanged)?;
    require(
        guard.current_generation == old.fire.arm_generation,
        Code::GenerationConflict,
    )?;
    match (&old.state, &new.state) {
        (WakeState::Pending { not_before_ms }, WakeState::Running { lease }) => {
            require(guard.now_ms >= *not_before_ms, Code::NotReady)?;
            require(
                lease.owner == guard.principal
                    && lease.expires_at_ms > guard.now_ms
                    && new.fence == old.fence + 1,
                Code::LeaseLost,
            )
        }
        (WakeState::Running { lease: before }, WakeState::Running { lease: after }) => {
            let renewing = before.expires_at_ms > guard.now_ms;
            require(
                after.owner == guard.principal
                    && after.expires_at_ms > guard.now_ms
                    && if renewing {
                        before.owner == guard.principal
                            && after.expires_at_ms >= before.expires_at_ms
                            && new.fence == old.fence
                    } else {
                        new.fence == old.fence + 1
                    },
                Code::LeaseLost,
            )
        }
        (WakeState::Running { lease }, WakeState::Completed { .. }) => require(
            lease.owner == guard.principal
                && lease.expires_at_ms > guard.now_ms
                && new.fence == old.fence,
            Code::LeaseLost,
        ),
        (WakeState::Blocked { retry }, WakeState::Pending { .. }) => {
            require(
                match retry.resume {
                    Resume::At { not_before_ms } => guard.now_ms >= not_before_ms,
                    Resume::OnChange { .. } => guard.resume_verified,
                },
                Code::NotReady,
            )?;
            require(new.fence == old.fence, Code::LeaseLost)
        }
        _ => Err(Code::InvalidRecord),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub format: Format,
    pub scope: Scope,
    pub pins: Pins,
    pub version: u64,
    pub shard: u32,
    pub enabled: bool,
    pub dirty_generation: u64,
    pub reconciled_generation: u64,
    pub next_check_ms: u64,
}

impl Registration {
    pub fn validate(&self) -> Result {
        self.pins.validate()?;
        require(
            bounded(&self.scope.space_id)
                && bounded(&self.scope.space_instance)
                && counter(self.version)
                && counter(self.dirty_generation)
                && self.reconciled_generation <= self.dirty_generation
                && self.next_check_ms <= MAX_COUNTER,
            Code::InvalidRecord,
        )
    }

    pub fn check_scan_ack(&self, scope: &Scope, version: u64, through: u64) -> Result {
        self.validate()?;
        require(scope == &self.scope, Code::ScopeMismatch)?;
        require(version == self.version, Code::VersionConflict)?;
        require(through == self.dirty_generation, Code::GenerationConflict)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationIdentity {
    pub scope: Scope,
    pub operation_key: String,
    pub request_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    ClaimWake,
    RenewWake,
    FinishWake,
    CancelWake,
    Gate,
    Dispatch,
}

pub fn operation_identity(
    scope: &Scope,
    wake_ref: &str,
    operation: Operation,
    step: u64,
    request: &Value,
    pins: &Pins,
) -> Result<OperationIdentity> {
    require(
        bounded(&scope.space_id)
            && bounded(&scope.space_instance)
            && runtime_ref(wake_ref, "wake")
            && counter(step),
        Code::InvalidRecord,
    )?;
    pins.validate()?;
    let key = content_digest(&json!({"domain":"anda-brain:operation-v1","scope":scope,"wake_ref":wake_ref,"operation":operation,"step":step}))
        .map_err(|_| Code::InvalidRecord)?;
    Ok(OperationIdentity {
        scope: scope.clone(),
        operation_key: format!("operation/v1/{}", &key[7..]),
        request_digest: content_digest(&json!({"request":request,"pins":pins}))
            .map_err(|_| Code::InvalidRecord)?,
    })
}

impl OperationIdentity {
    pub fn validate(&self) -> Result {
        require(
            bounded(&self.scope.space_id)
                && bounded(&self.scope.space_instance)
                && runtime_ref(&self.operation_key, "operation")
                && digest(&self.request_digest),
            Code::InvalidRecord,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiptState {
    Committed {
        commit_seq: u64,
        outputs: Vec<String>,
    },
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationReceipt {
    pub format: Format,
    pub identity: OperationIdentity,
    pub pins: Pins,
    pub state: ReceiptState,
}

impl OperationReceipt {
    pub fn validate(&self) -> Result {
        self.identity.validate()?;
        self.pins.validate()?;
        if let ReceiptState::Committed {
            commit_seq,
            outputs,
        } = &self.state
        {
            require(
                counter(*commit_seq) && outputs.len() <= 128,
                Code::InvalidRecord,
            )?;
            let unique: std::collections::BTreeSet<_> = outputs.iter().collect();
            require(unique.len() == outputs.len(), Code::InvalidRecord)?;
            for output in outputs {
                require(
                    output
                        .parse::<ElementId>()
                        .is_ok_and(|id| counter(id.seq) && id.to_string() == *output)
                        || runtime_ref(output, "wake")
                        || runtime_ref(output, "dispatch"),
                    Code::InvalidRecord,
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Readback {
    ReplayCommitted,
    ReconcileSameIdentity,
}

pub fn readback(
    expected: &OperationIdentity,
    pins: &Pins,
    receipt: Option<&OperationReceipt>,
) -> Result<Readback> {
    expected.validate()?;
    pins.validate()?;
    let Some(receipt) = receipt else {
        return Ok(Readback::ReconcileSameIdentity);
    };
    receipt.validate()?;
    require(
        receipt.identity.scope == expected.scope,
        Code::ScopeMismatch,
    )?;
    require(
        receipt.identity == *expected && receipt.pins == *pins,
        Code::IdempotencyConflict,
    )?;
    Ok(match receipt.state {
        ReceiptState::Committed { .. } => Readback::ReplayCommitted,
        ReceiptState::OutcomeUnknown => Readback::ReconcileSameIdentity,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum GateResult {
    Act {
        decision_ref: String,
        attempt_ref: String,
        dispatch_ref: String,
    },
    Ask {
        decision_ref: String,
        clarification_wake: String,
    },
    Defer {
        decision_ref: String,
        retry: Retry,
    },
    Silence {
        decision_ref: String,
        reason: String,
    },
}

impl GateResult {
    pub fn validate(&self, pins: &Pins) -> Result {
        pins.validate()?;
        let decision = match self {
            Self::Act {
                decision_ref,
                attempt_ref,
                dispatch_ref,
            } => {
                require(pins.binding.is_some(), Code::BindingUnavailable)?;
                require(
                    graph_ref(attempt_ref, ElementKind::Activity)
                        && runtime_ref(dispatch_ref, "dispatch"),
                    Code::InvalidRecord,
                )?;
                decision_ref
            }
            Self::Ask {
                decision_ref,
                clarification_wake,
            } => {
                require(pins.binding.is_some(), Code::BindingUnavailable)?;
                require(runtime_ref(clarification_wake, "wake"), Code::InvalidRecord)?;
                decision_ref
            }
            Self::Defer {
                decision_ref,
                retry,
            } => {
                retry.validate()?;
                decision_ref
            }
            Self::Silence {
                decision_ref,
                reason,
            } => {
                require(bounded(reason), Code::InvalidRecord)?;
                decision_ref
            }
        };
        require(
            graph_ref(decision, ElementKind::Activity),
            Code::InvalidRecord,
        )
    }
}

pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    require(
        value["format"] == "anda-brain:attention-v1",
        Code::UnsupportedVersion,
    )?;
    serde_json::from_value(value).map_err(|_| Code::InvalidRecord)
}

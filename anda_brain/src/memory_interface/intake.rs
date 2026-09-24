//! Intake: idempotency keys, receipts and progress (MI §3, §5), and the
//! Formation-side state an observe or revise conversation carries.
//!
//! A mutation's key is scoped to `(caller, Space, intent)`; its semantic
//! digest covers the intent, the resolved scope, the input and the source's
//! identity and digest, never the transport `request_id` or a budget. The same
//! key with the same meaning replays the original receipt; a different
//! meaning is `IdempotencyConflict`.
//!
//! observe and revise run as ordinary Formation conversations, so their
//! progress is read from the conversation: queued or retrying work is
//! `recorded`, a completed pass is `available` (the Nexus indexes
//! synchronously, so processed work is recallable at its own commit), and a
//! cancelled one — a suppressed source or a failed predecessor — is `failed`.
//! The disposition comes from the trace the pass recorded as it wrote: formed
//! memory, Evidence only, or nothing. The first terminal progress is written
//! back into the receipt, so progress never moves backwards.
use super::*;
use crate::agents::SELF_USER_ID;
use anda_engine::memory::{Conversation, ConversationStatus};
use anda_kip::{Command, MutationClause, MutationValue};
use object_store::PutMode;
use serde_json::Map;
use std::time::Duration;

/// The conversation `extra` member carrying a [`MemoryIntent`].
pub(crate) const INTENT_KEY: &str = "memory_intent";
/// The conversation `extra` member carrying a Formation pass's [`TraceData`].
pub(crate) const TRACE_KEY: &str = "memory_trace";
/// The most element refs a trace keeps.
const MAX_TRACE_REFS: usize = 128;
/// How often a waiting request re-reads progress.
const POLL: Duration = Duration::from_millis(100);

/// What an observe or revise conversation was admitted for. Host-written at
/// intake; the model reads a rendering of it, never this value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryIntent {
    pub receipt_ref: String,
    pub operation: Operation,
    pub source_ref: String,
    pub scope: ResolvedScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_kind: Option<wire::ChangeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
    /// A misrecording's original source: the Evidence the repaired
    /// extraction cites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_observed_at: Option<String>,
    /// Receipts that must reach a terminal disposition first (MI §5.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predecessors: Vec<String>,
}

impl MemoryIntent {
    pub(crate) fn of(conversation: &Conversation) -> Option<Self> {
        conversation
            .extra
            .as_ref()
            .and_then(|extra| extra.get(INTENT_KEY))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    fn misrecorded(&self) -> bool {
        self.change_kind == Some(wire::ChangeKind::Misrecorded)
    }

    /// Whether the pass may write this lifecycle move. An observation is
    /// ordinary Formation, where a speaker may correct themselves; a revise
    /// names its history, so only a correction revises the actor's own claim
    /// and every other kind only adds claims, never superseding on a guess
    /// (MI §4).
    fn allows_transition(&self, state: Option<&str>) -> bool {
        match self.operation {
            Operation::Revise => {
                self.change_kind == Some(wire::ChangeKind::Correction)
                    && matches!(state, Some("superseded" | "corrected" | "retracted"))
            }
            _ => true,
        }
    }

    /// The parameters the host binds on every write of this pass.
    pub(crate) fn bindings(&self) -> Map<String, Json> {
        let mut parameters = Map::from_iter([
            ("contexts".to_string(), json!(self.scope.contexts)),
            ("scope_task".to_string(), json!(self.scope.task)),
        ]);
        if let Some(original) = &self.original_evidence {
            parameters.insert("orig".into(), json!(original));
        }
        parameters
    }

    /// Why this pass may not write `request`, if it may not: scoped intake
    /// must write every claim in its context set, and only a correction may
    /// move another claim's lifecycle.
    pub(crate) fn refusal(&self, request: &anda_kip::Request) -> Option<String> {
        let commands = request.parse_operations().ok()?;
        for command in &commands {
            let Command::Kml(statement) = command else {
                continue;
            };
            for clause in &statement.clauses {
                match clause {
                    MutationClause::CreateAssertion(record) if !self.scope.contexts.is_empty() => {
                        let context = record
                            .set_fields
                            .iter()
                            .flatten()
                            .find(|(field, _)| field == "context_refs")
                            .map(|(_, value)| value);
                        if !matches!(context, Some(MutationValue::Param(name)) if name == "contexts")
                        {
                            return Some(
                                "this observation is scoped: write every ASSERT with \
                                 `context: :contexts` so the claim stays in its task and \
                                 contexts (MI §3, Profile §20.3)"
                                    .into(),
                            );
                        }
                    }
                    MutationClause::Transition(transition)
                        if !transition_is_activity(transition)
                            && !self.allows_transition(transition.state()) =>
                    {
                        return Some(match (self.operation, self.change_kind) {
                            (_, Some(wire::ChangeKind::Misrecorded)) => "a misrecording is \
                                repaired by the host; do not supersede, correct or retract any \
                                claim — write only what the original source actually said"
                                .into(),
                            (_, Some(wire::ChangeKind::WorldChange)) => "a world change is one \
                                new Assertion from the change; temporal succession ends the old \
                                value, so do not supersede or retract it (Spec §25.4)"
                                .into(),
                            (Operation::Revise, _) => "change_kind is unspecified: record the \
                                revision as new claims; never supersede or retract on a guess \
                                (MI §4)"
                                .into(),
                            _ => "this lifecycle move is not allowed for this intent".into(),
                        });
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Adds the MemoryScope Facet to the Evidence a scoped pass captures.
    pub(crate) fn scope_evidence(&self, ingest: &mut anda_kip::IngestContext) {
        if self.scope.contexts.is_empty() {
            return;
        }
        for entry in &mut ingest.evidence {
            entry.facets.insert(
                "MemoryScope".into(),
                Map::from_iter([
                    ("task_ref".to_string(), json!(self.scope.task)),
                    ("context_refs".to_string(), json!(self.scope.contexts)),
                ]),
            );
        }
    }

    /// The directive section the Formation prompt carries for this pass.
    pub(crate) fn directive(&self) -> String {
        let scope = if self.scope.contexts.is_empty() {
            "General (no task or context scope).".to_string()
        } else {
            format!(
                "Task {} with context set {:?}. Write every ASSERT with `context: :contexts`, and \
                 set the MemoryScope Facet (`SET FACET \"MemoryScope\" {{task_ref: :scope_task, \
                 context_refs: :contexts}}`) on every \
                 Event, Insight, Experience or Commitment you create from this source. A \
                 task-scoped instruction is not a global preference.",
                self.scope.task.as_deref().unwrap_or("none"),
                self.scope.contexts
            )
        };
        let intent = match (self.operation, self.change_kind) {
            (Operation::Observe, _) => "observe: encode what this source says. If it holds \
                nothing worth remembering, write nothing; that is an honest `skipped`."
                .to_string(),
            (_, Some(wire::ChangeKind::Correction)) => format!(
                "revise (correction): the speaker says their earlier claim was wrong. Write the \
                 corrected claim as a new ASSERT that SUPERSEDING the wrong one{}, keeping the \
                 corrected world interval. Never retract a different actor's claim.",
                self.target_ref
                    .as_deref()
                    .map(|target| format!(" (the caller named {target})"))
                    .unwrap_or_default()
            ),
            (_, Some(wire::ChangeKind::WorldChange)) => "revise (world change): the world moved \
                on. Write one new ASSERT from the time of the change (`valid: {from: …}` when \
                the source says when, else `at:` the source time). Temporal succession ends the \
                old value; do not supersede or retract it."
                .to_string(),
            (_, Some(wire::ChangeKind::Misrecorded)) => format!(
                "revise (misrecorded): the host is repairing extraction {} recorded from Evidence \
                 {} (bound as `:orig`, observed_at {}). The actor never made that claim. Do not \
                 supersede, correct or retract anything. Read `:orig`; if it states a claim the \
                 extraction should have been, write that claim citing `evidence: :orig` with \
                 `at:` {} exactly. Otherwise write nothing.",
                self.target_ref.as_deref().unwrap_or("unknown"),
                self.original_evidence.as_deref().unwrap_or("unknown"),
                self.original_observed_at.as_deref().unwrap_or("unknown"),
                self.original_observed_at.as_deref().unwrap_or("unknown"),
            ),
            (_, _) => "revise (unspecified): record what this source says as new claims. \
                Do not supersede or retract anything; the host will disclose that the revision \
                was recorded without revising an earlier claim."
                .to_string(),
        };
        format!(
            "Receipt {}. Intent — {intent}\nScope — {scope}",
            self.receipt_ref
        )
    }
}

fn transition_is_activity(transition: &anda_kip::Transition) -> bool {
    matches!(
        transition.state(),
        Some("completed" | "failed" | "cancelled" | "running")
    )
}

/// The intent a Formation pass runs under, for the write gate.
#[derive(Clone)]
pub(crate) struct IntentState(pub Option<Arc<MemoryIntent>>);

/// What a Formation pass committed, as the host saw it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TraceData {
    /// The newest Space sequence a write of the pass committed at.
    pub max_seq: Option<u64>,
    /// Elements other than Evidence the pass created or changed.
    pub formed: Vec<String>,
    /// Evidence the pass captured.
    pub evidence: Vec<String>,
    /// Assertions the pass created.
    pub assertions: Vec<String>,
    /// Lifecycle moves the pass wrote on existing claims.
    pub revisions: u64,
}

impl TraceData {
    fn push(list: &mut Vec<String>, id: &str) {
        if list.len() < MAX_TRACE_REFS && !list.iter().any(|v| v == id) {
            list.push(id.to_string());
        }
    }

    /// Folds one successful write response into the trace.
    pub fn record(&mut self, request: &anda_kip::Request, response: &anda_kip::Response) {
        let mut committed = false;
        let receipts = response
            .receipt
            .iter()
            .chain(response.results.iter().filter_map(|r| r.receipt.as_ref()));
        for receipt in receipts {
            if receipt.status == anda_kip::ReceiptStatus::Committed
                && let Some(seq) = receipt.space_seq
            {
                committed = true;
                self.max_seq = Some(self.max_seq.map_or(seq, |max| max.max(seq)));
            }
        }
        if !committed {
            return;
        }
        for result in &response.results {
            if result.status != anda_kip::OperationStatus::Succeeded {
                continue;
            }
            for change in result
                .result
                .as_ref()
                .and_then(|r| r.get("changes"))
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                let (Some(id), Some(kind), Some(op)) = (
                    change["id"].as_str(),
                    change["kind"].as_str(),
                    change["op"].as_str(),
                ) else {
                    continue;
                };
                if op == "noop" {
                    continue;
                }
                match kind {
                    "evidence" => Self::push(&mut self.evidence, id),
                    "assertion" if op == "create" => {
                        Self::push(&mut self.assertions, id);
                        Self::push(&mut self.formed, id);
                    }
                    _ => Self::push(&mut self.formed, id),
                }
            }
        }
        if let Ok(commands) = request.parse_operations() {
            for command in commands {
                if let Command::Kml(statement) = command {
                    self.revisions += statement
                        .clauses
                        .iter()
                        .filter(|clause| {
                            matches!(clause, MutationClause::Transition(t) if !transition_is_activity(t))
                        })
                        .count() as u64;
                }
            }
        }
    }

    fn disposition(&self) -> wire::Disposition {
        if !self.formed.is_empty() {
            wire::Disposition::Formed
        } else if !self.evidence.is_empty() {
            wire::Disposition::EvidenceOnly
        } else {
            wire::Disposition::Skipped
        }
    }
}

/// The trace a running pass appends to; shared between the pass's tool calls
/// and the conversation snapshot that persists it.
#[derive(Clone, Default)]
pub(crate) struct FormationTrace(pub Arc<parking_lot::Mutex<TraceData>>);

impl FormationTrace {
    /// Continues the trace an earlier attempt of the same conversation left.
    pub fn resume(conversation: &Conversation) -> Self {
        let data = conversation
            .extra
            .as_ref()
            .and_then(|extra| extra.get(TRACE_KEY))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        Self(Arc::new(parking_lot::Mutex::new(data)))
    }

    /// Copies the trace into the conversation, so it persists with the
    /// snapshot that completes it.
    pub fn persist_into(&self, conversation: &mut Conversation) {
        let data = self.0.lock().clone();
        if data == TraceData::default() {
            return;
        }
        let extra = conversation.extra.get_or_insert_with(|| json!({}));
        if let Some(extra) = extra.as_object_mut() {
            extra.insert(TRACE_KEY.into(), json!(data));
        }
    }
}

/// A key's binding to its receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyRecord {
    receipt_ref: String,
    intent_digest: String,
}

/// One intake: the immutable acknowledgement and what it is waiting on.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntakeRecord {
    pub receipt: wire::Receipt,
    pub namespace: String,
    pub intent_digest: String,
    pub scope: ResolvedScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<MemoryIntent>,
    /// The first terminal progress; progress never moves back from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<wire::Progress>,
    /// The operation's result once known: a FormationResult or ForgetResult.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Json>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub created_at: u64,
}

fn digest(value: &Json) -> Result<String, KipError> {
    anda_cognitive_nexus::content_digest(value)
}

/// The journal id of a key: a digest of its full scope, so a key never
/// collides across callers, Spaces or intents.
fn key_id(
    namespace: &str,
    space: &str,
    operation: Operation,
    key: &str,
) -> Result<String, KipError> {
    Ok(digest(&json!([namespace, space, operation, key]))?[7..47].to_string())
}

/// A receipt ref is opaque and names nothing it does not own.
pub(crate) fn receipt_path(receipt_ref: &str) -> Result<String, KipError> {
    let id = receipt_ref
        .strip_prefix("rcpt-")
        .filter(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| KipError::not_found_or_not_visible("receipt not found"))?;
    Ok(format!("receipts/{id}"))
}

fn internal(error: BoxError) -> KipError {
    kip_error(error)
}

impl Space {
    /// A receipt the caller owns in this Space.
    pub(crate) async fn intake_record(
        &self,
        namespace: &str,
        receipt_ref: &str,
    ) -> Result<IntakeRecord, KipError> {
        let path = receipt_path(receipt_ref)?;
        let record = self
            .memory_interface
            .journal
            .read::<IntakeRecord>(&path)
            .await
            .map_err(internal)?
            .ok_or_else(|| KipError::not_found_or_not_visible("receipt not found"))?
            .value;
        if record.namespace != namespace || record.receipt.space_id != self.id() {
            return Err(KipError::not_found_or_not_visible("receipt not found"));
        }
        Ok(record)
    }

    /// Writes the first terminal progress and result into the receipt.
    async fn settle_record(
        &self,
        receipt_ref: &str,
        progress: wire::Progress,
        result: Option<Json>,
        warnings: Vec<String>,
    ) -> Result<IntakeRecord, KipError> {
        let path = receipt_path(receipt_ref)?;
        let journal = &self.memory_interface.journal;
        loop {
            let stored = journal
                .read::<IntakeRecord>(&path)
                .await
                .map_err(internal)?
                .ok_or_else(|| KipError::not_found_or_not_visible("receipt not found"))?;
            if stored.value.terminal.is_some() {
                return Ok(stored.value);
            }
            let mut record = stored.value;
            record.terminal = Some(progress.clone());
            if result.is_some() {
                record.result = result.clone();
            }
            for warning in &warnings {
                if !record.warnings.contains(warning) {
                    record.warnings.push(warning.clone());
                }
            }
            if journal
                .put(&path, &record, PutMode::Update(stored.version))
                .await
                .is_ok()
            {
                return Ok(record);
            }
        }
    }

    /// Current progress of a receipt (MI §5). Reading never moves it back.
    pub(crate) async fn progress_of(
        self: &Arc<Self>,
        record: &IntakeRecord,
    ) -> Result<wire::Progress, KipError> {
        Ok(self.settled(record).await?.0)
    }

    /// Current progress plus the record carrying any settled result.
    pub(crate) async fn settled(
        self: &Arc<Self>,
        record: &IntakeRecord,
    ) -> Result<(wire::Progress, IntakeRecord), KipError> {
        if let Some(progress) = &record.terminal {
            return Ok((progress.clone(), record.clone()));
        }
        let receipt_ref = record.receipt.receipt_ref.clone();
        let recorded = |reason: Option<String>| wire::Progress {
            receipt_ref: receipt_ref.clone(),
            phase: wire::Phase::Recorded,
            disposition: None,
            resolved_seq: None,
            available_seq: None,
            reason,
            error: None,
        };
        let Some(id) = record.conversation else {
            return Ok((
                recorded(Some("intake is being recorded".into())),
                record.clone(),
            ));
        };
        let conversation = match self.memory.get_conversation(id).await {
            Ok(conversation) => conversation,
            Err(_) => {
                return Err(KipError::new(
                    KipErrorCode::ArtifactUnavailable,
                    "the processing record behind this receipt is no longer retained",
                ));
            }
        };
        match conversation.status {
            ConversationStatus::Completed => {
                let record = self.finish_formation(record, &conversation).await?;
                let progress = record
                    .terminal
                    .clone()
                    .unwrap_or_else(|| recorded(Some("finishing formation".into())));
                Ok((progress, record))
            }
            ConversationStatus::Cancelled => {
                let reason = conversation
                    .failed_reason
                    .clone()
                    .unwrap_or_else(|| "processing was cancelled".into());
                let error = if reason.contains("outcome_unknown") {
                    KipError::new(
                        KipErrorCode::OutcomeUnknown,
                        format!("processing was interrupted and its outcome is unknown: {reason}"),
                    )
                } else if reason.starts_with("predecessor_failed") {
                    KipError::precondition_failed(format!(
                        "a predecessor receipt failed, so this revision was not formed: {reason}"
                    ))
                } else if reason == "source_suppressed" {
                    KipError::precondition_failed(
                        "the source was excluded from memory before it was processed",
                    )
                } else {
                    KipError::internal_error(reason.clone())
                };
                let progress = wire::Progress {
                    receipt_ref: receipt_ref.clone(),
                    phase: wire::Phase::Failed,
                    disposition: None,
                    resolved_seq: None,
                    available_seq: None,
                    reason: Some(reason),
                    error: Some(error_object(&error)),
                };
                let record = self
                    .settle_record(&receipt_ref, progress, None, vec![])
                    .await?;
                Ok((record.terminal.clone().unwrap(), record))
            }
            // A failed pass is retried when the Formation queue resumes, so
            // it is not terminal: it stays recorded, with its reason.
            ConversationStatus::Failed => Ok((
                recorded(Some(format!(
                    "formation failed and will be retried: {}",
                    conversation
                        .failed_reason
                        .as_deref()
                        .unwrap_or("unknown failure")
                ))),
                record.clone(),
            )),
            _ => Ok((
                recorded(
                    (!record
                        .intent
                        .as_ref()
                        .is_none_or(|intent| intent.predecessors.is_empty()))
                    .then(|| "waiting for predecessor receipts".to_string()),
                ),
                record.clone(),
            )),
        }
    }

    /// Settles a completed Formation conversation: its disposition from the
    /// pass's trace, and for a misrecording the host's recording repair.
    async fn finish_formation(
        self: &Arc<Self>,
        record: &IntakeRecord,
        conversation: &Conversation,
    ) -> Result<IntakeRecord, KipError> {
        let trace: TraceData = conversation
            .extra
            .as_ref()
            .and_then(|extra| extra.get(TRACE_KEY))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        let receipt_ref = record.receipt.receipt_ref.clone();
        let accepted = record.receipt.accepted_seq;
        let mut warnings = Vec::new();
        let mut refs = trace.formed.clone();
        let mut disposition = trace.disposition();
        let mut resolved = trace.max_seq.unwrap_or(accepted).max(accepted);
        let intent = record.intent.clone();
        if let Some(intent) = intent.as_ref().filter(|intent| intent.misrecorded()) {
            match self.repair_misrecording(intent, &trace).await {
                Ok((repair_ref, seq)) => {
                    refs.insert(0, repair_ref);
                    disposition = wire::Disposition::Formed;
                    resolved = resolved.max(seq);
                }
                Err(error) => {
                    let progress = wire::Progress {
                        receipt_ref: receipt_ref.clone(),
                        phase: wire::Phase::Failed,
                        disposition: None,
                        resolved_seq: None,
                        available_seq: None,
                        reason: Some("recording repair was refused".into()),
                        error: Some(error_object(&error)),
                    };
                    return self
                        .settle_record(&receipt_ref, progress, None, vec![])
                        .await;
                }
            }
        } else if let Some(intent) = &intent
            && intent.operation == Operation::Revise
            && intent
                .change_kind
                .is_none_or(|kind| kind == wire::ChangeKind::Unspecified)
        {
            warnings.push(
                "change_kind was unspecified: the revision was recorded as new claims; no \
                 earlier claim was superseded or retracted"
                    .to_string(),
            );
        }
        if disposition == wire::Disposition::Skipped {
            warnings.push(
                "formation found nothing to remember in this source; it was skipped, not learned"
                    .into(),
            );
        }
        let summary = match disposition {
            wire::Disposition::Formed => {
                format!("Formed {} memory element(s) from the source.", refs.len())
            }
            wire::Disposition::EvidenceOnly => {
                "Preserved the source as Evidence; no claim was formed from it.".to_string()
            }
            _ => "Processed the source; nothing was formed from it.".to_string(),
        };
        refs.truncate(MAX_TRACE_REFS);
        let result = json!(wire::FormationResult {
            summary,
            memory_refs: refs,
        });
        let progress = wire::Progress {
            receipt_ref: receipt_ref.clone(),
            phase: wire::Phase::Available,
            disposition: Some(disposition),
            resolved_seq: Some(resolved),
            // Search indexes are synchronous: a committed pass is recallable
            // at its own sequence.
            available_seq: Some(resolved),
            reason: None,
            error: None,
        };
        self.settle_record(&receipt_ref, progress, Some(result), warnings)
            .await
    }

    /// The recording repair a misrecorded revise asks for (Spec §57.8): the
    /// target extraction is invalidated, and every Assertion the pass wrote
    /// from the same original source replaces it.
    async fn repair_misrecording(
        &self,
        intent: &MemoryIntent,
        trace: &TraceData,
    ) -> Result<(String, u64), KipError> {
        let (Some(target), Some(original)) = (&intent.target_ref, &intent.original_evidence) else {
            return Err(KipError::precondition_failed(
                "a misrecording repair names its target extraction",
            ));
        };
        let nexus = self.memory.nexus();
        let target_id: ElementId = target.parse()?;
        let Element::Assertion(row) = nexus.store.get_element(target_id).await? else {
            return Err(KipError::not_found_or_not_visible(
                "repair target not found",
            ));
        };
        let Element::Evidence(source) = nexus.store.get_element(original.parse()?).await? else {
            return Err(KipError::not_found_or_not_visible(
                "repair source not found",
            ));
        };
        let mut replacements = Vec::new();
        for id in &trace.assertions {
            if let Ok(Element::Assertion(candidate)) = nexus.store.get_element(id.parse()?).await
                && candidate.evidence_ids.iter().any(|e| e == original)
                && candidate.state == "active"
            {
                replacements.push(id.clone());
            }
        }
        let locator = if source.payload_inline.get("content").is_some() {
            "/content".to_string()
        } else {
            let length = match &source.payload_inline {
                Json::String(text) => text.len(),
                other => anda_kip::try_canonical_json(other)
                    .map(|text| text.len())
                    .unwrap_or(0),
            };
            format!("bytes=0-{}", length.saturating_sub(1))
        };
        let repair = anda_kip::cognitive::RecordingRepair {
            source_ref: original.clone(),
            source_digest: anda_cognitive_nexus::repair::source_digest(&source).ok_or_else(
                || KipError::precondition_failed("the repair source's bytes are gone"),
            )?,
            source_locator: locator,
            invalidated_refs: vec![target.clone()],
            replacement_refs: replacements,
            reason: anda_kip::cognitive::RepairReason::ExtractionError,
            expected_versions: [(target.clone(), row.version)].into(),
        };
        let result = nexus
            .system_session()
            .repair_recording(DEFAULT_SPACE, repair)
            .await?;
        let repair_ref = result["repair_ref"]
            .as_str()
            .ok_or_else(|| KipError::internal_error("repair returned no record"))?
            .to_string();
        let seq = self.memory_seq().await?;
        Ok((repair_ref, seq))
    }

    /// The receipt view `GET .../memory/receipts/{ref}` serves: the
    /// immutable acknowledgement, current progress, and the result once known.
    pub async fn memory_receipt_view(
        self: &Arc<Self>,
        namespace: &str,
        receipt_ref: &str,
    ) -> Result<Json, KipError> {
        let record = self.intake_record(namespace, receipt_ref).await?;
        let (progress, record) = self.settled(&record).await?;
        Ok(json!({
            "receipt": record.receipt,
            "progress": progress,
            "result": record.result,
            "warnings": record.warnings,
        }))
    }

    /// A receipt's current progress and the Formation conversation it
    /// started, for a trusted embedding host that links its own records to
    /// the native conversation.
    pub async fn memory_receipt_state(
        self: &Arc<Self>,
        namespace: &str,
        receipt_ref: &str,
    ) -> Result<(wire::Progress, Option<u64>), KipError> {
        let record = self.intake_record(namespace, receipt_ref).await?;
        let (progress, record) = self.settled(&record).await?;
        Ok((progress, record.conversation))
    }

    /// Settles the receipt a finished Formation conversation carries.
    pub(crate) async fn settle_memory_conversation(
        self: &Arc<Self>,
        conversation: &Conversation,
    ) -> Result<(), KipError> {
        let Some(intent) = MemoryIntent::of(conversation) else {
            return Ok(());
        };
        let path = receipt_path(&intent.receipt_ref)?;
        let Some(record) = self
            .memory_interface
            .journal
            .read::<IntakeRecord>(&path)
            .await
            .map_err(internal)?
        else {
            return Ok(());
        };
        self.settled(&record.value).await.map(|_| ())
    }

    /// Waits until a receipt leaves `recorded` or the deadline passes.
    pub(crate) async fn wait_settled(
        self: &Arc<Self>,
        record: &IntakeRecord,
        deadline_ms: u64,
    ) -> Result<(wire::Progress, IntakeRecord), KipError> {
        let until = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
        loop {
            let (progress, record) = self.settled(record).await?;
            if progress.phase != wire::Phase::Recorded || tokio::time::Instant::now() >= until {
                return Ok((progress, record));
            }
            tokio::time::sleep(POLL.min(until - tokio::time::Instant::now())).await;
        }
    }

    /// Whether every predecessor a Formation conversation names reached a
    /// disposition that lets it proceed. A failed predecessor blocks its
    /// successors (MI §5.1).
    pub(crate) async fn memory_predecessors_failed(
        self: &Arc<Self>,
        intent: &MemoryIntent,
    ) -> Option<String> {
        for predecessor in &intent.predecessors {
            let Ok(path) = receipt_path(predecessor) else {
                return Some(predecessor.clone());
            };
            let record = match self
                .memory_interface
                .journal
                .read::<IntakeRecord>(&path)
                .await
            {
                Ok(Some(record)) => record.value,
                _ => return Some(predecessor.clone()),
            };
            match self.progress_of(&record).await {
                Ok(progress) if progress.phase == wire::Phase::Failed => {
                    return Some(predecessor.clone());
                }
                Err(_) => return Some(predecessor.clone()),
                _ => {}
            }
        }
        None
    }

    /// Admits one mutation intent (observe, revise, feedback) under its key.
    pub(crate) async fn memory_intake(
        self: &Arc<Self>,
        namespace: &str,
        request: &Request,
        intent: wire::Intent,
    ) -> Result<Response, KipError> {
        let key = request
            .idempotency_key
            .as_deref()
            .ok_or_else(|| KipError::invalid_request_envelope("mutations need a key"))?;
        let source_ref = match &intent {
            wire::Intent::Observe(input) => input.source_ref.clone(),
            wire::Intent::Revise(input) => input.source_ref.clone(),
            wire::Intent::Feedback(input) => input.source_ref.clone(),
            _ => return Err(KipError::internal_error("not an intake intent")),
        };
        let key_path = format!(
            "keys/{}",
            key_id(namespace, self.id(), request.operation, key)?
        );
        let requested_scope = request
            .scope
            .as_ref()
            .map(wire::Scope::canonical)
            .unwrap_or_default();
        // The source's identity and digest join the key's meaning, so a key
        // reused with different bytes behind the same handle conflicts.
        let source = self.resolve_source(namespace, &source_ref).await;
        let journal = &self.memory_interface.journal;
        let _gate = self.memory_interface.gate.lock().await;
        if let Some(bound) = journal
            .read::<KeyRecord>(&key_path)
            .await
            .map_err(internal)?
        {
            let record = self
                .intake_record(namespace, &bound.value.receipt_ref)
                .await?;
            let meaning = digest(&json!({
                "operation": request.operation,
                "scope": requested_scope,
                "input": request.input,
                "source": {"ref": source_ref, "digest": record.source_digest},
            }))?;
            if meaning != record.intent_digest
                || source
                    .as_ref()
                    .is_ok_and(|source| Some(&source.digest) != record.source_digest.as_ref())
            {
                return Err(KipError::new(
                    KipErrorCode::IdempotencyConflict,
                    "this idempotency key was used for a different request",
                ));
            }
            drop(_gate);
            return self.intake_response(request, record).await;
        }
        let source = source?;
        let meaning = digest(&json!({
            "operation": request.operation,
            "scope": requested_scope,
            "input": request.input,
            "source": {"ref": source_ref, "digest": source.digest},
        }))?;
        let scope = self.resolve_scope(request.scope.as_ref(), true).await?;
        let receipt_ref = format!("rcpt-{}", &key_path[5..]);
        let predecessors = match &source.order {
            Some(order) => {
                for predecessor in &order.predecessor_receipts {
                    self.intake_record(namespace, predecessor).await?;
                }
                order.predecessor_receipts.clone()
            }
            None => vec![],
        };
        let mut record = IntakeRecord {
            receipt: wire::Receipt {
                receipt_ref: receipt_ref.clone(),
                operation: request.operation,
                space_id: self.id().to_string(),
                accepted_seq: self.memory_seq().await?,
            },
            namespace: namespace.to_string(),
            intent_digest: meaning,
            scope: scope.clone(),
            source_ref: Some(source_ref.clone()),
            source_digest: Some(source.digest.clone()),
            conversation: None,
            intent: None,
            terminal: None,
            result: None,
            warnings: vec![],
            created_at: anda_engine::unix_ms(),
        };
        let receipt_path = receipt_path(&receipt_ref)?;
        journal
            .create(&receipt_path, &record)
            .await
            .map_err(internal)?;
        journal
            .create(
                &key_path,
                &KeyRecord {
                    receipt_ref: receipt_ref.clone(),
                    intent_digest: record.intent_digest.clone(),
                },
            )
            .await
            .map_err(internal)?;
        let outcome = match &intent {
            wire::Intent::Feedback(input) => {
                self.capture_feedback(&mut record, &source, input).await
            }
            wire::Intent::Revise(input) => {
                self.start_revision(&mut record, &source, input, predecessors)
                    .await
            }
            _ => {
                let intent = MemoryIntent {
                    receipt_ref: receipt_ref.clone(),
                    operation: Operation::Observe,
                    source_ref: source_ref.clone(),
                    scope,
                    change_kind: None,
                    target_ref: None,
                    original_evidence: None,
                    original_observed_at: None,
                    predecessors,
                };
                self.start_formation(&mut record, &source, intent).await
            }
        };
        // Persist what intake started, even when it then failed: the key is
        // bound and a retry must replay this, never run it again.
        let stored = journal
            .read::<IntakeRecord>(&receipt_path)
            .await
            .map_err(internal)?
            .ok_or_else(|| KipError::internal_error("receipt vanished"))?;
        if let Err(error) = &outcome {
            record.terminal.get_or_insert_with(|| wire::Progress {
                receipt_ref: receipt_ref.clone(),
                phase: wire::Phase::Failed,
                disposition: None,
                resolved_seq: None,
                available_seq: None,
                reason: Some("intake could not start processing".into()),
                error: Some(error_object(error)),
            });
        }
        journal
            .put(&receipt_path, &record, PutMode::Update(stored.version))
            .await
            .map_err(internal)?;
        drop(_gate);
        outcome?;
        self.intake_response(request, record).await
    }

    async fn intake_response(
        self: &Arc<Self>,
        request: &Request,
        record: IntakeRecord,
    ) -> Result<Response, KipError> {
        let wait = request
            .budget
            .as_ref()
            .and_then(|budget| budget.deadline_ms)
            .unwrap_or(0);
        let (progress, record) = if wait > 0 {
            self.wait_settled(&record, wait).await?
        } else {
            self.settled(&record).await?
        };
        let result = record.result.clone().unwrap_or_else(|| {
            json!(wire::FormationResult {
                summary: "Recorded; processing is pending. Pass this receipt in recall.after \
                          to wait for it."
                    .into(),
                memory_refs: vec![],
            })
        });
        let partial = record.warnings.iter().any(|w| w.starts_with("partial:"));
        let response = mutation_response(
            request,
            record.receipt.clone(),
            progress,
            result,
            record.warnings.clone(),
        );
        Ok(if partial && response.status == Status::Succeeded {
            with_status(response, Status::Partial)
        } else {
            response
        })
    }

    /// Queues a Formation conversation for an observe or revise intent. The
    /// conversation's input is the staged source, and its source identity is
    /// what a later forget suppresses.
    async fn start_formation(
        self: &Arc<Self>,
        record: &mut IntakeRecord,
        source: &sources::ResolvedSource,
        intent: MemoryIntent,
    ) -> Result<(), KipError> {
        let input = crate::types::FormationInput {
            messages: source.messages.clone(),
            context: source.host.as_ref().and_then(|host| host.context.clone()),
            timestamp: Some(source.observed_at.clone()),
        };
        let output = self
            .ingest_memory_intent(SELF_USER_ID, input, source.identity(), &intent)
            .await
            .map_err(kip_error)?;
        record.conversation = output.conversation;
        record.intent = Some(intent);
        Ok(())
    }

    /// revise: correction, world change and unspecified run as Formation; a
    /// misrecording needs its target and recovers the original source, and a
    /// deployment without recording repair would refuse it (MI §4).
    async fn start_revision(
        self: &Arc<Self>,
        record: &mut IntakeRecord,
        source: &sources::ResolvedSource,
        input: &wire::ReviseInput,
        predecessors: Vec<String>,
    ) -> Result<(), KipError> {
        let mut intent = MemoryIntent {
            receipt_ref: record.receipt.receipt_ref.clone(),
            operation: Operation::Revise,
            source_ref: source.source_ref.clone(),
            scope: record.scope.clone(),
            change_kind: input.change_kind,
            target_ref: input.target_ref.clone(),
            original_evidence: None,
            original_observed_at: None,
            predecessors,
        };
        if let Some(target) = &input.target_ref {
            // The target must be memory this Space holds; a handle grants
            // nothing, and a missing one is existence-neutral.
            let id: ElementId = target
                .parse()
                .map_err(|_| KipError::not_found_or_not_visible("revise target not found"))?;
            match self.memory.nexus().store.get_element(id).await {
                Ok(element) if element.space() == DEFAULT_SPACE && element.state() == "active" => {
                    if intent.misrecorded() {
                        let Element::Assertion(row) = element else {
                            return Err(KipError::constraint_violation(
                                "a misrecording repairs an Assertion",
                            ));
                        };
                        let original = row.evidence_ids.first().cloned().ok_or_else(|| {
                            KipError::precondition_failed(
                                "the extraction cites no source to recover the claim from",
                            )
                        })?;
                        if let Ok(Element::Evidence(evidence)) = self
                            .memory
                            .nexus()
                            .store
                            .get_element(original.parse()?)
                            .await
                        {
                            intent.original_observed_at = Some(evidence.observed_at.clone());
                        }
                        intent.original_evidence = Some(original);
                    }
                }
                _ => {
                    return Err(KipError::not_found_or_not_visible(
                        "revise target not found",
                    ));
                }
            }
        } else if intent.misrecorded() {
            // An unclear target stays explicit: the report is preserved and
            // the gap reported, never guessed at (MI §4).
            record.warnings.push(
                "partial: a misrecording names the extraction to repair in target_ref; the \
                 report was preserved as Evidence and nothing was repaired"
                    .into(),
            );
            return self
                .capture_evidence(record, source, "revise-report", None)
                .await;
        }
        self.start_formation(record, source, intent).await
    }

    /// feedback: the source becomes attributed Evidence with its actual
    /// origin — an agent's self-report is `agent_statement`, a person's is
    /// `user_statement` — and never an Outcome or a grade (MI §4).
    async fn capture_feedback(
        self: &Arc<Self>,
        record: &mut IntakeRecord,
        source: &sources::ResolvedSource,
        input: &wire::FeedbackInput,
    ) -> Result<(), KipError> {
        for reference in input.decision_ref.iter().chain(&input.attempt_ref) {
            let id: ElementId = reference
                .parse()
                .map_err(|_| KipError::not_found_or_not_visible("feedback reference not found"))?;
            match self.memory.nexus().store.get_element(id).await {
                Ok(element) if element.space() == DEFAULT_SPACE && element.state() != "purged" => {}
                _ => {
                    return Err(KipError::not_found_or_not_visible(
                        "feedback reference not found",
                    ));
                }
            }
        }
        let about = json!({"decision_ref": input.decision_ref, "attempt_ref": input.attempt_ref});
        self.capture_evidence(record, source, "feedback", Some(about))
            .await
    }

    /// Captures a source as Evidence without a model: one Evidence per
    /// message, classed by the role the host captured, keyed so a retry
    /// resolves to the same elements.
    async fn capture_evidence(
        self: &Arc<Self>,
        record: &mut IntakeRecord,
        source: &sources::ResolvedSource,
        purpose: &str,
        about: Option<Json>,
    ) -> Result<(), KipError> {
        let receipt_ref = record.receipt.receipt_ref.clone();
        let mut evidence = Vec::new();
        let mut max_seq = record.receipt.accepted_seq;
        if let Some(existing) = &source.evidence {
            // An already captured source is cited, not captured again.
            evidence.push(existing.clone());
        } else {
            for (index, message) in source.messages.iter().enumerate() {
                let class = sources::evidence_class(&message.role);
                let mut payload = serde_json::to_value(message)
                    .map_err(|e| KipError::internal_error(e.to_string()))?;
                if let (Some(about), Some(payload)) = (&about, payload.as_object_mut()) {
                    payload.insert("memory_feedback".into(), about.clone());
                }
                let request = crate::kip::request_with(
                    r#"MUTATE {
                        CREATE EVIDENCE ?e { CLIENT KEY :key SET FIELDS {
                            evidence_class: :class, payload: :payload, observed_at: :at
                        } }
                    }"#,
                    Map::from_iter([
                        (
                            "key".into(),
                            json!(format!("memory-{purpose}:{receipt_ref}:{}", index + 1)),
                        ),
                        ("class".into(), json!(class)),
                        ("payload".into(), payload),
                        (
                            "at".into(),
                            json!(crate::kip::message_observed_at(
                                message,
                                &source.observed_at
                            )),
                        ),
                    ]),
                );
                let nexus = self.memory.nexus();
                let response =
                    self.memory_interface
                        .tasks
                        .run(async move {
                            Ok(anda_kip::execute_request(nexus.as_ref(), &request).await)
                        })
                        .await
                        .map_err(kip_error)?;
                let Some(result) = crate::kip::ok_result(&response) else {
                    return Err(crate::kip::error_of(&response)
                        .map(|error| {
                            KipError::new(
                                KipErrorCode::from_name(&error.code)
                                    .unwrap_or(KipErrorCode::InternalError),
                                error.message.clone(),
                            )
                        })
                        .unwrap_or_else(|| KipError::internal_error("Evidence capture failed")));
                };
                if let Some(id) = result["handles"]["e"].as_str() {
                    evidence.push(id.to_string());
                }
                if let Some(seq) = response.receipt.as_ref().and_then(|r| r.space_seq) {
                    max_seq = max_seq.max(seq);
                }
            }
        }
        let summary = match purpose {
            "feedback" => format!(
                "Preserved the feedback as {} attributed Evidence; it is not a grade.",
                evidence.len()
            ),
            _ => format!("Preserved the source as {} Evidence.", evidence.len()),
        };
        record.result = Some(json!(wire::FormationResult {
            summary,
            memory_refs: evidence,
        }));
        record.terminal = Some(wire::Progress {
            receipt_ref,
            phase: wire::Phase::Available,
            disposition: Some(wire::Disposition::EvidenceOnly),
            resolved_seq: Some(max_seq),
            available_seq: Some(max_seq),
            reason: None,
            error: None,
        });
        Ok(())
    }
}

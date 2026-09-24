//! recall (MI §4–§6): a bounded briefing, an evidence expansion, or the
//! attention raised since a cursor.
//!
//! The host owns everything but relevance. It waits on the `after` barrier,
//! runs one Recall pass for the question (the model finds what bears on it),
//! and then builds the briefing itself: each cited claim's final belief is
//! re-read through `BELIEF` under the request's scope and world time,
//! out-of-scope memory is dropped, and the seven channels come from host
//! reads — constraints and commitments exactly, experiences, failures and
//! skills by a bounded search, dependencies from the returned items' own
//! validity. Required constraints and warnings are never dropped for the
//! output budget; what does not fit makes coverage incomplete. The basis,
//! coverage, plans and pinned element versions are retained behind
//! `basis_ref`, so `detail: "evidence"` reads what produced the result and
//! never a newer version. Recall writes no memory: returned items go to the
//! exposure log only (Spec §66.8), which is not cognition.
use super::*;
use crate::{
    kip,
    payload::StringOr,
    types::{AttentionRecallInput, RecallInput as BrainRecallInput},
};
use anda_kip::memory::binding::{
    AttentionItem, AttentionKind, Briefing, ChannelState, Channels, Coverage, Details,
    EpistemicStatus, ItemRole, MemoryItem, RecallInput, RecallMode, Standing,
};
use object_store::PutMode;
use serde_json::Map;
use std::time::Duration;

/// The most rows one channel read returns; one more marks it truncated.
const CHANNEL_LIMIT: usize = 32;
/// The most search candidates an approximate channel considers.
const SEARCH_LIMIT: usize = 8;
/// The most items a briefing carries (schema bound).
const MAX_ITEMS: usize = 128;
/// The attention page a briefing carries.
const ATTENTION_PAGE: usize = 20;
/// The output budget an expansion gets when it names none.
const EXPANSION_OUTPUT_TOKENS: u64 = 65_536;

/// A briefing as retained behind its `basis_ref`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedRecall {
    namespace: String,
    snapshot_seq: u64,
    scope: ResolvedScope,
    /// The ProjectionBasis the briefing's beliefs were read under.
    basis: Json,
    /// The RecallCoverage, with its per-channel RecallPlans.
    coverage: Json,
    /// Each item's ref and the element versions it was built from.
    items: Vec<(String, Vec<(String, u64)>)>,
    created_at: u64,
}

/// One candidate before budgeting: the wire item and the elements behind it.
struct Candidate {
    item: MemoryItem,
    pins: Vec<(String, u64)>,
    /// Required constraints and warnings are kept whatever the budget.
    required: bool,
}

/// The per-channel outcome of the host's reads.
#[derive(Clone, Copy)]
struct Plan {
    exact: bool,
    complete: bool,
    truncation: Option<&'static str>,
}

impl Plan {
    fn exact(truncated: bool) -> Self {
        Self {
            exact: true,
            complete: !truncated,
            truncation: truncated.then_some("page_limit"),
        }
    }
    fn approximate(ok: bool) -> Self {
        Self {
            exact: false,
            complete: ok,
            truncation: (!ok).then_some("unsupported"),
        }
    }
    fn failed(reason: &'static str) -> Self {
        Self {
            exact: true,
            complete: false,
            truncation: Some(reason),
        }
    }
    fn state(&self) -> ChannelState {
        if self.complete {
            ChannelState::Complete
        } else {
            ChannelState::Incomplete
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn nonblank(text: String, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.to_string()
    } else {
        truncate(&text, 4096)
    }
}

fn status_of(value: &str) -> EpistemicStatus {
    match value {
        "accepted" => EpistemicStatus::Accepted,
        "rejected" => EpistemicStatus::Rejected,
        "contested" => EpistemicStatus::Contested,
        "uncertain" => EpistemicStatus::Uncertain,
        _ => EpistemicStatus::Insufficient,
    }
}

fn local(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

/// The scope a Concept was formed in, from its MemoryScope Facet.
fn concept_scope(row: &Json) -> (Option<String>, Vec<String>) {
    let facets = row.get("facets").and_then(Json::as_object);
    let scope = facets.and_then(|facets| {
        facets
            .iter()
            .find(|(name, _)| local(name) == "MemoryScope")
            .map(|(_, value)| value)
    });
    let Some(scope) = scope else {
        return (None, vec![]);
    };
    let task = scope["task_ref"].as_str().map(str::to_string);
    let contexts = scope["context_refs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect();
    (task, contexts)
}

fn reference_id(value: &Json) -> Option<&str> {
    value.as_str().or_else(|| value["id"].as_str())
}

fn version_of(row: &Json) -> u64 {
    row["_system"]["version"].as_u64().unwrap_or(0)
}

/// Why a derived artifact needs a caveat, if it does (Spec §57.6).
fn dependency_caveat(row: &Json) -> Option<String> {
    let validity = &row["_system"]["dependency_validity"];
    let status = validity
        .get("status")
        .and_then(Json::as_str)
        .or_else(|| validity.as_str())?;
    matches!(status, "needs_review" | "unverifiable").then(|| status.to_string())
}

impl Space {
    async fn rows(&self, request: anda_kip::Request) -> Result<Vec<Json>, KipError> {
        let response = self
            .execute_kip_readonly(request)
            .await
            .map_err(kip_error)?;
        if !kip::succeeded(&response) {
            return Err(kip::error_of(&response)
                .map(|error| {
                    KipError::new(
                        KipErrorCode::from_name(&error.code).unwrap_or(KipErrorCode::InternalError),
                        error.message.clone(),
                    )
                })
                .unwrap_or_else(|| KipError::internal_error("read failed")));
        }
        Ok(kip::ok_result(&response)
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// One element's current row, when this Space holds it.
    async fn element_row(&self, id: &str, as_of: Option<u64>) -> Option<Json> {
        let kind = id.parse::<ElementId>().ok()?.kind;
        let pattern = match kind {
            anda_kip::ElementKind::Concept => "?e CONCEPT {id: :id}",
            anda_kip::ElementKind::Proposition => "?e PROPOSITION (id: :id)",
            anda_kip::ElementKind::Assertion => "?e ASSERTION {id: :id}",
            anda_kip::ElementKind::Evidence => "?e EVIDENCE {id: :id}",
            anda_kip::ElementKind::Activity => "?e ACTIVITY {id: :id}",
        };
        let command = match as_of {
            Some(seq) => format!("FIND(?e) WHERE {{ {pattern} }} AS OF SEQ {seq} LIMIT 1"),
            None => format!("FIND(?e) WHERE {{ {pattern} }} LIMIT 1"),
        };
        self.rows(kip::request_with(command, kip::param("id", id)))
            .await
            .ok()?
            .into_iter()
            .next()
    }

    async fn endpoint_text(&self, value: &Json, as_of: Option<u64>) -> String {
        if let Some(id) = reference_id(value)
            && let Some(row) = self.element_row(id, as_of).await
            && let Some(name) = row["name"].as_str()
        {
            return name.to_string();
        }
        value.to_string()
    }

    /// Final belief in a Proposition under the request's scope and time.
    async fn belief(
        &self,
        proposition: &str,
        scope: &ResolvedScope,
        valid_at: Option<&str>,
        as_of: Option<u64>,
    ) -> Result<Json, KipError> {
        let mut command = String::from("FIND(?b) WHERE { ?b BELIEF (id: :id) }");
        let mut parameters = Map::from_iter([
            ("id".to_string(), json!(proposition)),
            ("contexts".to_string(), json!(scope.contexts)),
        ]);
        if let Some(seq) = as_of {
            command.push_str(&format!(" AS OF SEQ {seq}"));
        }
        if let Some(at) = valid_at {
            command.push_str(" FOR TIME :valid_at");
            parameters.insert("valid_at".into(), json!(at));
        }
        command.push_str(
            " WITH EPISTEMIC {purpose: \"memory_recall\", risk: \"low\", context_refs: :contexts}",
        );
        self.rows(kip::request_with(command, parameters))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| KipError::internal_error("BELIEF returned no projection"))
    }

    /// A recall (see the module docs).
    pub(crate) async fn memory_recall(
        self: &Arc<Self>,
        namespace: &str,
        request: &Request,
        input: RecallInput,
    ) -> Result<Response, KipError> {
        let budget = request.budget.clone().unwrap_or_default();
        let deadline_ms = budget.deadline_ms.unwrap_or(DEFAULT_DEADLINE_MS);
        let max_tokens = budget.max_output_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS);
        let started = tokio::time::Instant::now();
        let until = started + Duration::from_millis(deadline_ms);
        if let Some(target) = &input.target_ref {
            let briefing = self
                .expand(
                    namespace,
                    target,
                    input.detail == Some(wire::RecallDetail::Evidence),
                )
                .await?;
            // An expansion has its own budget (MI §6).
            let max_tokens = budget.max_output_tokens.unwrap_or(EXPANSION_OUTPUT_TOKENS);
            return self.briefing_response(request, briefing, max_tokens, vec![]);
        }
        let as_of = input.time.as_ref().and_then(|time| time.as_of_seq);
        let valid_at = match input
            .time
            .as_ref()
            .and_then(|time| time.valid_at.as_deref())
        {
            Some(at) => Some(
                anda_kip::timestamp::parse(at, "valid_at")
                    .map(|_| at.to_string())
                    .map_err(|_| {
                        KipError::invalid_request_envelope("valid_at is a KIP timestamp")
                    })?,
            ),
            None => None,
        };
        let scope = self.resolve_scope(request.scope.as_ref(), false).await?;

        // The processing barrier (MI §5): every receipt must be the caller's
        // own; wait until each is available or the deadline passes.
        let mut records = Vec::new();
        for receipt in &input.after {
            records.push(self.intake_record(namespace, receipt).await?);
        }
        let mut after = Vec::new();
        for record in &records {
            let remaining = until.saturating_duration_since(tokio::time::Instant::now());
            let (progress, _) = self
                .wait_settled(record, remaining.as_millis() as u64)
                .await?;
            after.push(progress);
        }
        if let Some(seq) = as_of
            && after
                .iter()
                .any(|p| p.available_seq.is_some_and(|available| available > seq))
        {
            return Err(KipError::precondition_failed(
                "an after receipt is newer than the fixed as_of_seq",
            ));
        }
        let snapshot_seq = match as_of {
            Some(seq) => seq,
            None => self.memory_seq().await?,
        };
        let pending: Vec<String> = after
            .iter()
            .filter(|p| !p.satisfies(snapshot_seq))
            .map(|p| p.receipt_ref.clone())
            .collect();
        let mut uncertainties: Vec<String> = after
            .iter()
            .filter(|p| p.phase == wire::Phase::Failed)
            .map(|p| {
                format!(
                    "receipt {} failed processing; its source is not in memory: {}",
                    p.receipt_ref,
                    p.reason.as_deref().unwrap_or("no reason recorded")
                )
            })
            .collect();
        for p in after.iter().filter(|p| p.phase == wire::Phase::Recorded) {
            uncertainties.push(format!(
                "receipt {} is still being processed; an answer may not reflect it",
                p.receipt_ref
            ));
        }
        let mode = input.mode();

        if mode == RecallMode::Attention {
            let page = self
                .scoped_attention(&scope, input.attention_cursor.clone())
                .await?;
            let coverage = Coverage::new(
                scope.requested.clone(),
                Channels::all(ChannelState::NotApplicable),
                pending.clone(),
                vec![],
            );
            let basis_ref = self
                .retain(
                    namespace,
                    snapshot_seq,
                    &scope,
                    json!(null),
                    &coverage,
                    &[],
                    &after,
                )
                .await?;
            let briefing = Briefing {
                summary: format!(
                    "{} attention item(s) raised since the cursor. An item is a prompt to think, \
                     never permission to act.",
                    page.0.len()
                ),
                items: vec![],
                uncertainties,
                basis_ref,
                coverage,
                after,
                continuation_ref: None,
                details: None,
                attention: Some(page.0),
                attention_cursor: Some(page.1),
            };
            return self.briefing_response(request, briefing, max_tokens, vec![]);
        }

        // All host reads and retained pins use this same snapshot. Search is
        // permitted only while the live index still corresponds to it.
        let as_of = Some(snapshot_seq);

        // The Recall pass: the model finds what bears on the question. Its
        // prose is used only when every memory it cited is in scope.
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut warnings = Vec::new();
        let mut evidence_plan = Plan::approximate(true);
        let mut summary = String::new();
        let mut dropped = false;
        if let Some(query) = input.query.as_deref().filter(|q| !q.trim().is_empty()) {
            let remaining = until.saturating_duration_since(tokio::time::Instant::now());
            let prompt = recall_prompt(query, mode, &scope, valid_at.as_deref(), as_of, &input);
            let pass = tokio::time::timeout(
                remaining,
                self.query_structured(
                    crate::agents::SELF_USER_ID,
                    StringOr::Value(BrainRecallInput {
                        query: prompt,
                        context: None,
                        budget: None,
                    }),
                ),
            )
            .await;
            match pass {
                Ok(Ok(output)) if output.failed_reason.is_none() => {
                    // The public memory citations intentionally omit Evidence;
                    // scope and summary checks must inspect the full read trace.
                    let mut reads = std::collections::BTreeMap::new();
                    if let Some(id) = output.conversation {
                        let conversation = self
                            .recall
                            .conversations
                            .get_conversation(id)
                            .await
                            .map_err(|e| kip_error(e.into()))?;
                        let messages = conversation
                            .messages
                            .into_iter()
                            .filter_map(|m| serde_json::from_value(m).ok())
                            .collect::<Vec<anda_core::Message>>();
                        let trace = crate::assess::RecallTrace::from_messages(&messages);
                        for tool in trace.tools.iter().filter(|t| t.is_error != Some(true)) {
                            if let Some(value) = &tool.output {
                                crate::assess::collect_element_objects(value, &mut |id, row| {
                                    reads.insert(id.to_string(), version_of(&json!(row)));
                                });
                            }
                        }
                    }
                    for (id, version) in reads {
                        if version > 0
                            && self
                                .element_row(&id, as_of)
                                .await
                                .as_ref()
                                .is_none_or(|row| version_of(row) != version)
                        {
                            dropped = true;
                            continue;
                        }
                        match self
                            .cited_item(&id, &scope, valid_at.as_deref(), as_of)
                            .await
                        {
                            Some(Some(candidate)) => candidates.push(candidate),
                            Some(None) => dropped = true,
                            None => {}
                        }
                    }
                    if !dropped {
                        summary = output.answer;
                    }
                }
                Ok(Ok(output)) => {
                    evidence_plan = Plan::failed("unsupported");
                    uncertainties.push(format!(
                        "the recall pass did not complete: {}",
                        output.failed_reason.unwrap_or_default()
                    ));
                }
                Ok(Err(error)) => {
                    evidence_plan = Plan::failed("unsupported");
                    uncertainties.push(format!("the recall pass failed: {error}"));
                }
                Err(_) => {
                    evidence_plan = Plan::failed("deadline");
                    uncertainties.push("the recall pass reached the deadline".into());
                }
            }
        } else {
            evidence_plan = Plan::approximate(false);
        }
        if dropped {
            warnings.push(
                "the recall pass read unavailable, invalidated or out-of-scope memory; its \
                 summary was replaced with verified items"
                    .to_string(),
            );
        }

        // Host channels.
        let query = input.query.clone().unwrap_or_default();
        let (constraint_items, constraints_plan) = self
            .exact_channel(
                "FIND(?c) WHERE { ?c {type: \"Insight\"} FILTER(?c.attributes.insight_class == \"constraint\") }",
                &scope,
                as_of,
                ItemRole::Constraint,
            )
            .await;
        let (commitment_items, commitments_plan) = self
            .exact_channel(
                "FIND(?c) WHERE { ?c {type: \"Commitment\"} FILTER(IN(?c.attributes.status, [\"pending\", \"blocked\"])) }",
                &scope,
                as_of,
                ItemRole::Constraint,
            )
            .await;
        let (failure_items, failures_plan) = self
            .search_channel(&query, "Experience", Some(true), &scope, as_of)
            .await;
        let (experience_items, experiences_plan) = self
            .search_channel(&query, "Experience", Some(false), &scope, as_of)
            .await;
        let (skill_items, skills_plan) = self
            .search_channel(&query, "Skill", None, &scope, as_of)
            .await;
        for channel in [
            constraint_items,
            commitment_items,
            failure_items,
            experience_items,
            skill_items,
        ] {
            for candidate in channel {
                if !candidates.iter().any(|c| c.pins == candidate.pins) {
                    candidates.push(candidate);
                }
            }
        }

        // Dependencies: every returned derived item's own validity.
        let mut unverified = Vec::new();
        let mut dependency_warnings = Vec::new();
        for candidate in &candidates {
            for (id, _) in &candidate.pins {
                if let Some(row) = self.element_row(id, as_of).await
                    && let Some(status) = dependency_caveat(&row)
                {
                    let note = format!(
                        "{id} depends on memory that changed ({status}); review before relying on it"
                    );
                    unverified.push(note.clone());
                    dependency_warnings.push(Candidate {
                        item: MemoryItem {
                            reference: String::new(),
                            text: note,
                            role: ItemRole::Warning,
                            epistemic_status: EpistemicStatus::Uncertain,
                            evidence_refs: vec![id.clone()],
                            action_eligible: false,
                            standing: None,
                        },
                        pins: vec![(id.clone(), version_of(&row))],
                        required: true,
                    });
                }
            }
        }
        candidates.extend(dependency_warnings);
        if mode == RecallMode::Action {
            for candidate in &candidates {
                if matches!(
                    candidate.item.epistemic_status,
                    EpistemicStatus::Contested | EpistemicStatus::Uncertain
                ) && matches!(candidate.item.role, ItemRole::Fact | ItemRole::Constraint)
                {
                    unverified.push(format!(
                        "{} is {:?}, not accepted",
                        candidate
                            .pins
                            .first()
                            .map(|p| p.0.as_str())
                            .unwrap_or("an item"),
                        candidate.item.epistemic_status
                    ));
                }
            }
        }
        if candidates.is_empty() && input.query.is_some() {
            uncertainties.push(
                "no recorded memory answers this; the basis is insufficient, which is not a no"
                    .into(),
            );
        }

        // resume: the scoped attention since the task's last cursor.
        let attention = if mode == RecallMode::Resume {
            Some(
                self.scoped_attention(&scope, input.attention_cursor.clone())
                    .await?,
            )
        } else {
            None
        };

        let mut plans = [
            ("constraints", constraints_plan),
            ("commitments", commitments_plan),
            ("dependencies", Plan::exact(false)),
            ("failures", failures_plan),
            ("experiences", experiences_plan),
            ("skills", skills_plan),
            ("evidence", evidence_plan),
        ];
        // Budget: required items stay; optional ones go lowest-value first.
        candidates.sort_by_key(|c| (!c.required, role_rank(c.item.role)));
        candidates.truncate(MAX_ITEMS);
        let basis = self
            .basis_for(&candidates, &scope, valid_at.as_deref(), as_of)
            .await;
        let summary_fallback = || {
            let mut lines: Vec<String> = candidates
                .iter()
                .take(12)
                .map(|c| format!("- {}", c.item.text))
                .collect();
            if lines.is_empty() {
                lines.push("No recorded memory bears on this in scope.".into());
            }
            lines.join("\n")
        };
        let summary = nonblank(
            if summary.trim().is_empty() {
                summary_fallback()
            } else {
                summary
            },
            "No recorded memory bears on this in scope.",
        );
        loop {
            let channels = channels_of(&plans);
            let coverage = Coverage::new(
                scope.requested.clone(),
                channels,
                pending.clone(),
                unverified.clone(),
            );
            let mut items: Vec<MemoryItem> = Vec::new();
            let basis_id = format!("basis-{}", hex_id());
            for (index, candidate) in candidates.iter().enumerate() {
                let mut item = candidate.item.clone();
                item.reference = format!("{basis_id}:{index}");
                item.action_eligible = coverage.action_eligible
                    && item.epistemic_status == EpistemicStatus::Accepted
                    && matches!(item.role, ItemRole::Fact | ItemRole::Constraint);
                items.push(item);
            }
            let mut briefing = Briefing {
                summary: summary.clone(),
                items,
                uncertainties: dedup(uncertainties.clone()),
                basis_ref: basis_id.clone(),
                coverage: coverage.clone(),
                after: after.clone(),
                continuation_ref: None,
                details: None,
                attention: attention.as_ref().map(|page| page.0.clone()),
                attention_cursor: attention.as_ref().map(|page| page.1.clone()),
            };
            let tokens = count_tokens(&briefing)?;
            if tokens <= max_tokens as usize {
                let pins: Vec<(String, Vec<(String, u64)>)> = briefing
                    .items
                    .iter()
                    .zip(&candidates)
                    .map(|(item, c)| (item.reference.clone(), c.pins.clone()))
                    .collect();
                let basis_ref = self
                    .retain_as(
                        &basis_id,
                        namespace,
                        snapshot_seq,
                        &scope,
                        basis.clone(),
                        &coverage,
                        &pins,
                        &plans,
                    )
                    .await?;
                briefing.basis_ref = basis_ref;
                self.record_exposure(&briefing, &candidates, snapshot_seq)
                    .await;
                return self.briefing_response(request, briefing, max_tokens, warnings);
            }
            // Drop the lowest-priority optional item; its channel is now
            // truncated by the budget. Required items never go.
            match candidates.iter().rposition(|c| !c.required) {
                Some(index) => {
                    let removed = candidates.remove(index);
                    let channel = match removed.item.role {
                        ItemRole::Experience => "experiences",
                        ItemRole::Procedure => "skills",
                        _ => "evidence",
                    };
                    for (name, plan) in plans.iter_mut() {
                        if *name == channel {
                            plan.complete = false;
                            plan.truncation = Some("budget");
                        }
                    }
                }
                None => {
                    return Err(KipError::result_limit_exceeded(format!(
                        "the required constraints, warnings and coverage need more than {max_tokens} \
                         tokens under {}",
                        crate::recall_budget::TOKENIZER
                    )));
                }
            }
        }
    }

    /// Turns one cited element into a briefing item. `None` when it is not
    /// memory a briefing reports; `Some(None)` when it is out of scope.
    async fn cited_item(
        &self,
        id: &str,
        scope: &ResolvedScope,
        valid_at: Option<&str>,
        as_of: Option<u64>,
    ) -> Option<Option<Candidate>> {
        let parsed: ElementId = id.parse().ok()?;
        let Some(row) = self.element_row(id, as_of).await else {
            return Some(None);
        };
        if row["_system"]["state"] != "active" {
            return Some(None);
        }
        match parsed.kind {
            anda_kip::ElementKind::Assertion => {
                if row["_system"]["recording_validity"]["status"] == "invalidated" {
                    return Some(None);
                }
                let contexts: Vec<String> = row["context_refs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(reference_id)
                    .map(str::to_string)
                    .collect();
                if !scope.admits(None, &contexts) {
                    return Some(None);
                }
                let proposition =
                    reference_id(&row["proposition"]).or_else(|| row["proposition_id"].as_str())?;
                let prop = self.element_row(proposition, as_of).await?;
                let belief = self
                    .belief(proposition, scope, valid_at, as_of)
                    .await
                    .ok()?;
                let actor = self.endpoint_text(&row["asserted_by"], as_of).await;
                let subject = self.endpoint_text(&prop["subject"], as_of).await;
                let object = self.endpoint_text(&prop["object"], as_of).await;
                let predicate = prop["predicate_ref"].as_str().map(local).unwrap_or("?");
                Some(Some(Candidate {
                    item: MemoryItem {
                        reference: String::new(),
                        text: truncate(
                            &format!("{subject} · {predicate} · {object} (claimed by {actor})"),
                            4096,
                        ),
                        role: ItemRole::Fact,
                        epistemic_status: status_of(belief["status"].as_str().unwrap_or("")),
                        evidence_refs: row["evidence"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(reference_id)
                            .take(32)
                            .map(str::to_string)
                            .collect(),
                        action_eligible: false,
                        standing: None,
                    },
                    pins: vec![
                        (id.to_string(), version_of(&row)),
                        (proposition.to_string(), version_of(&prop)),
                    ],
                    required: false,
                }))
            }
            anda_kip::ElementKind::Proposition => {
                // A tuple is content too: it belongs to this scope only when
                // one of its claims does.
                let claims = self
                    .rows(kip::request_with(
                        format!("FIND(?a) WHERE {{ ?p PROPOSITION (id: :id) ?a ASSERTION {{proposition: ?p}} }}{} LIMIT 64", as_of.map(|seq| format!(" AS OF SEQ {seq}")).unwrap_or_default()),
                        kip::param("id", id),
                    ))
                    .await
                    .ok()?;
                let in_scope = claims.iter().any(|claim| {
                    if claim["_system"]["state"] != "active"
                        || claim["_system"]["recording_validity"]["status"] == "invalidated"
                    {
                        return false;
                    }
                    let contexts: Vec<String> = claim["context_refs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.as_str().or_else(|| r["id"].as_str()))
                        .map(str::to_string)
                        .collect();
                    scope.admits(None, &contexts)
                });
                if !in_scope {
                    return Some(None);
                }
                let belief = self.belief(id, scope, valid_at, as_of).await.ok()?;
                let status = status_of(belief["status"].as_str().unwrap_or(""));
                let evidence: Vec<String> = belief["support"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["assertion_id"].as_str().or_else(|| a["id"].as_str()))
                    .map(str::to_string)
                    .take(32)
                    .collect();
                let subject = row["subject"].to_string();
                let object = row["object"].to_string();
                let predicate = row["predicate_ref"].as_str().map(local).unwrap_or("?");
                Some(Some(Candidate {
                    item: MemoryItem {
                        reference: String::new(),
                        text: truncate(&format!("{subject} · {predicate} · {object}"), 4096),
                        role: ItemRole::Fact,
                        epistemic_status: status,
                        evidence_refs: evidence,
                        action_eligible: false,
                        standing: None,
                    },
                    pins: vec![(id.to_string(), version_of(&row))],
                    required: false,
                }))
            }
            anda_kip::ElementKind::Concept => {
                let (task, contexts) = concept_scope(&row);
                if !scope.admits(task.as_deref(), &contexts) {
                    return Some(None);
                }
                concept_item(&row, false).map(Some)
            }
            anda_kip::ElementKind::Evidence => {
                let (task, contexts) = concept_scope(&row);
                if !scope.admits(task.as_deref(), &contexts) {
                    return Some(None);
                }
                Some(Some(Candidate {
                    item: MemoryItem {
                        reference: String::new(),
                        text: truncate(&format!("Source: {}", row["payload"]), 4096),
                        role: ItemRole::Source,
                        epistemic_status: EpistemicStatus::NotApplicable,
                        evidence_refs: vec![id.to_string()],
                        action_eligible: false,
                        standing: None,
                    },
                    pins: vec![(id.to_string(), version_of(&row))],
                    required: false,
                }))
            }
            anda_kip::ElementKind::Activity => None,
        }
    }

    /// An exact channel: every matching Concept in scope, up to the page.
    async fn exact_channel(
        &self,
        command: &str,
        scope: &ResolvedScope,
        as_of: Option<u64>,
        _role: ItemRole,
    ) -> (Vec<Candidate>, Plan) {
        let command = match as_of {
            Some(seq) => format!("{command} AS OF SEQ {seq} LIMIT {}", CHANNEL_LIMIT + 1),
            None => format!("{command} LIMIT {}", CHANNEL_LIMIT + 1),
        };
        let rows = match self.rows(kip::request(command)).await {
            Ok(rows) => rows,
            Err(_) => return (vec![], Plan::failed("unsupported")),
        };
        let truncated = rows.len() > CHANNEL_LIMIT;
        let mut items = Vec::new();
        for row in rows.into_iter().take(CHANNEL_LIMIT) {
            let (task, contexts) = concept_scope(&row);
            if !scope.admits(task.as_deref(), &contexts) {
                continue;
            }
            if let Some(candidate) = concept_item(&row, true) {
                items.push(candidate);
            }
        }
        (items, Plan::exact(truncated))
    }

    /// An approximate channel: a bounded search for the question. A declared
    /// bounded plan may complete; it never claims semantic exhaustiveness.
    async fn search_channel(
        &self,
        query: &str,
        type_name: &str,
        failures: Option<bool>,
        scope: &ResolvedScope,
        as_of: Option<u64>,
    ) -> (Vec<Candidate>, Plan) {
        let filter = match failures {
            Some(true) => " FILTER(IN(?c.attributes.outcome_status, [\"failure\", \"aborted\"]))",
            Some(false) => {
                " FILTER(IN(?c.attributes.outcome_status, [\"success\", \"partial\", \"unknown\"]))"
            }
            None => "",
        };
        let (command, parameters) = if query.trim().is_empty() {
            (
                format!(
                    "FIND(?c) WHERE {{ ?c {{type: \"{type_name}\"}}{filter} }} ORDER BY ?c._system.updated_at DESC LIMIT {}",
                    SEARCH_LIMIT + 1
                ),
                Map::new(),
            )
        } else {
            (
                format!(
                    "FIND(?c) WHERE {{ ?c SEARCH CONCEPT :query WITH TYPE \"{type_name}\" LIMIT {}{filter} }} LIMIT {SEARCH_LIMIT}",
                    SEARCH_LIMIT * 2
                ),
                kip::param("query", query),
            )
        };
        let command = match as_of {
            Some(seq) if query.trim().is_empty() => {
                command.replace(" ORDER BY", &format!(" AS OF SEQ {seq} ORDER BY"))
            }
            // Search indexes serve the current snapshot only.
            Some(seq) if self.memory_seq().await.ok() != Some(seq) => {
                return (vec![], Plan::failed("unsupported"));
            }
            Some(_) => command,
            None => command,
        };
        let rows = match self.rows(kip::request_with(command, parameters)).await {
            Ok(rows) => rows,
            Err(_) => return (vec![], Plan::failed("unsupported")),
        };
        let truncated = query.trim().is_empty() && rows.len() > SEARCH_LIMIT;
        let mut items = Vec::new();
        for row in rows.into_iter().take(SEARCH_LIMIT) {
            let Some(row) = self
                .element_row(row["id"].as_str().unwrap_or(""), as_of)
                .await
            else {
                continue;
            };
            let (task, contexts) = concept_scope(&row);
            if !scope.admits(task.as_deref(), &contexts) {
                continue;
            }
            if let Some(candidate) = concept_item(&row, false) {
                items.push(candidate);
            }
        }
        (
            items,
            if as_of.is_some() && self.memory_seq().await.ok() != as_of {
                Plan::failed("unsupported")
            } else if truncated {
                Plan::exact(true)
            } else {
                Plan::approximate(true)
            },
        )
    }

    /// The ProjectionBasis a briefing's beliefs were read under.
    async fn basis_for(
        &self,
        candidates: &[Candidate],
        scope: &ResolvedScope,
        valid_at: Option<&str>,
        as_of: Option<u64>,
    ) -> Json {
        for candidate in candidates {
            for (id, _) in &candidate.pins {
                if id.starts_with("P-")
                    && let Ok(belief) = self.belief(id, scope, valid_at, as_of).await
                    && belief.get("basis").is_some()
                {
                    return belief["basis"].clone();
                }
            }
        }
        // No belief was read: project a tuple no one stored, which answers
        // `insufficient` with the basis it was read under.
        let mut command = String::from(
            "FIND(?b) WHERE { ?s {type: \"Event\", key: \"memory_scope:__basis__\"} ?b BELIEF (?s, \"same_as\", ?s) }",
        );
        if let Some(seq) = as_of {
            command.push_str(&format!(" AS OF SEQ {seq}"));
        }
        self.rows(kip::request(command))
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|belief| belief["basis"].clone())
            .unwrap_or(Json::Null)
    }

    async fn scoped_attention(
        &self,
        scope: &ResolvedScope,
        cursor: Option<String>,
    ) -> Result<(Vec<AttentionItem>, String), KipError> {
        let page = self
            .recall_attention(AttentionRecallInput {
                attention_cursor: cursor,
                limit: Some(ATTENTION_PAGE),
            })
            .await
            .map_err(|error| {
                if error.to_string().contains("cursor") {
                    KipError::new(KipErrorCode::CursorInvalid, error.to_string())
                } else {
                    kip_error(error)
                }
            })?;
        let mut items = Vec::new();
        for item in page.items {
            // A target formed in another task is not this task's attention.
            let mut admitted = true;
            for target in &item.target_refs {
                if let Some(row) = self.element_row(target, None).await {
                    let (task, contexts) = concept_scope(&row);
                    admitted &= scope.admits(task.as_deref(), &contexts);
                }
            }
            if !admitted {
                continue;
            }
            items.push(AttentionItem {
                reference: item.reference,
                kind: if item.kind == "commitment_due" {
                    AttentionKind::CommitmentDue
                } else {
                    AttentionKind::WatchFired
                },
                summary: nonblank(item.summary, "attention raised"),
                raised_seq: item.raised_seq,
                due_at: item.due_at,
                target_refs: item.target_refs,
                priority: item.priority,
            });
        }
        Ok((items, page.attention_cursor))
    }

    /// Retains a briefing's basis for expansion and returns its ref.
    #[allow(clippy::too_many_arguments)]
    async fn retain(
        &self,
        namespace: &str,
        snapshot_seq: u64,
        scope: &ResolvedScope,
        basis: Json,
        coverage: &Coverage,
        items: &[(String, Vec<(String, u64)>)],
        _after: &[wire::Progress],
    ) -> Result<String, KipError> {
        let id = format!("basis-{}", hex_id());
        let plans = [
            ("constraints", Plan::approximate(true)),
            ("commitments", Plan::approximate(true)),
            ("dependencies", Plan::approximate(true)),
            ("failures", Plan::approximate(true)),
            ("experiences", Plan::approximate(true)),
            ("skills", Plan::approximate(true)),
            ("evidence", Plan::approximate(true)),
        ];
        self.retain_as(
            &id,
            namespace,
            snapshot_seq,
            scope,
            basis,
            coverage,
            items,
            &plans,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn retain_as(
        &self,
        id: &str,
        namespace: &str,
        snapshot_seq: u64,
        scope: &ResolvedScope,
        basis: Json,
        coverage: &Coverage,
        items: &[(String, Vec<(String, u64)>)],
        plans: &[(&'static str, Plan)],
    ) -> Result<String, KipError> {
        let authorization_view = basis["authorization_view"]
            .as_str()
            .unwrap_or("kip:system")
            .to_string();
        let mut channel_states = Map::new();
        let mut plan_values = Map::new();
        for (name, plan) in plans {
            let state = coverage
                .channels
                .iter()
                .find(|(channel, _)| channel == name)
                .map(|(_, state)| state)
                .unwrap_or(ChannelState::Incomplete);
            channel_states.insert(
                (*name).into(),
                json!({"completed": state != ChannelState::Incomplete, "truncated": plan.truncation.is_some()}),
            );
            let selector = anda_cognitive_nexus::content_digest(
                &json!({"channel": name, "exact": plan.exact}),
            )?;
            plan_values.insert(
                (*name).into(),
                json!({
                    "selector": {"artifact_ref": format!("anda-brain:recall-plan/{name}"), "content_digest": selector},
                    "scope": scope.memory_scope(),
                    "method": if plan.exact { "exact" } else { "approximate" },
                    "snapshot_seq": snapshot_seq,
                    "index_seq": snapshot_seq,
                    "covered_through_seq": snapshot_seq,
                    "authorization_view": authorization_view,
                    "complete": plan.complete,
                    "truncation_reason": plan.truncation,
                }),
            );
        }
        let recall_coverage = json!({
            "basis": basis,
            "channels": channel_states,
            "unverified_preconditions": coverage.unverified_preconditions,
            "action_eligible": coverage.action_eligible,
            "plans": plan_values,
        });
        let retained = RetainedRecall {
            namespace: namespace.to_string(),
            snapshot_seq,
            scope: scope.clone(),
            basis,
            coverage: recall_coverage,
            items: items.to_vec(),
            created_at: anda_engine::unix_ms(),
        };
        self.memory_interface
            .journal
            .put(&format!("recalls/{id}"), &retained, PutMode::Create)
            .await
            .map_err(kip_error)?;
        Ok(id.to_string())
    }

    /// `detail: "evidence"` or a plain expansion of a retained result: the
    /// retained basis and coverage, and each pinned element at the version
    /// that produced the item — never a newer one (MI §6).
    async fn expand(
        &self,
        namespace: &str,
        target: &str,
        evidence: bool,
    ) -> Result<Briefing, KipError> {
        let (basis_id, item) = match target.split_once(':') {
            Some((basis, index)) => (basis, Some(index)),
            None => (target, None),
        };
        let not_found = || KipError::not_found_or_not_visible("result reference not found");
        if !basis_id.starts_with("basis-")
            || basis_id.len() != 46
            || !basis_id[6..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(not_found());
        }
        let retained = self
            .memory_interface
            .journal
            .read::<RetainedRecall>(&format!("recalls/{basis_id}"))
            .await
            .map_err(kip_error)?
            .ok_or_else(not_found)?
            .value;
        if retained.namespace != namespace {
            return Err(not_found());
        }
        let selected: Vec<&(String, Vec<(String, u64)>)> = retained
            .items
            .iter()
            .filter(|(reference, _)| item.is_none() || reference == target)
            .collect();
        if item.is_some() && selected.is_empty() {
            return Err(not_found());
        }
        let mut elements = Vec::new();
        let mut uncertainties = Vec::new();
        for (_, pins) in &selected {
            for (id, version) in pins {
                match self.element_row(id, Some(retained.snapshot_seq)).await {
                    Some(row)
                        if row["_system"]["state"] != "purged"
                            && (*version == 0 || version_of(&row) == *version) =>
                    {
                        if elements.len() < MAX_ITEMS {
                            elements.push(row);
                        }
                    }
                    _ => uncertainties.push(format!(
                        "{id} as it was at this result is no longer available"
                    )),
                }
            }
        }
        let coverage = Coverage::new(
            retained.scope.requested.clone(),
            Channels::all(ChannelState::Complete),
            vec![],
            vec![],
        );
        Ok(Briefing {
            summary: format!(
                "Expansion of {target} at snapshot {}: {} element(s) retained.",
                retained.snapshot_seq,
                elements.len()
            ),
            items: vec![],
            uncertainties,
            basis_ref: basis_id.to_string(),
            coverage: Coverage {
                // An expansion reads a retained result; it never becomes a
                // current, action-eligible briefing.
                action_eligible: false,
                ..coverage
            },
            after: vec![],
            continuation_ref: None,
            details: evidence.then(|| Details {
                basis: retained.basis.clone(),
                coverage: retained.coverage.clone(),
                elements,
            }),
            attention: None,
            attention_cursor: None,
        })
    }

    /// Records `retrieved` for the returned items (Spec §66.8). The log is
    /// not cognition: it advances no sequence and reinforces nothing.
    async fn record_exposure(
        &self,
        briefing: &Briefing,
        candidates: &[Candidate],
        snapshot_seq: u64,
    ) {
        let mut entries = Vec::new();
        for candidate in candidates.iter().take(briefing.items.len()) {
            for (id, _) in &candidate.pins {
                if entries.len() >= 256 {
                    break;
                }
                entries.push(anda_cognitive_nexus::exposure::ExposureInput {
                    element_id: id.clone(),
                    exposure: anda_kip::cognitive::Exposure::Retrieved,
                    snapshot_seq,
                    decision_ref: None,
                    recall_ref: Some(briefing.basis_ref.clone()),
                });
            }
        }
        if entries.is_empty() {
            return;
        }
        if let Err(error) = self
            .memory
            .nexus()
            .system_session()
            .record_exposures(DEFAULT_SPACE, entries)
            .await
        {
            log::warn!(target: "brain", space_id = self.id(); "exposure log write failed: {error}");
        }
    }

    fn briefing_response(
        &self,
        request: &Request,
        briefing: Briefing,
        max_tokens: u64,
        warnings: Vec<String>,
    ) -> Result<Response, KipError> {
        let tokens = count_tokens(&briefing)?;
        if tokens > max_tokens as usize {
            return Err(KipError::result_limit_exceeded(format!(
                "the result needs {tokens} tokens; the budget is {max_tokens}"
            )));
        }
        let status = if briefing.coverage.complete {
            Status::Succeeded
        } else if !briefing.coverage.pending_receipts.is_empty() {
            Status::Pending
        } else {
            Status::Partial
        };
        Ok(Response {
            kip_memory: wire::KIP_MEMORY_VERSION.into(),
            request_id: request.request_id.clone(),
            operation: request.operation,
            status,
            receipt: None,
            progress: None,
            result: Some(json!(briefing)),
            error: None,
            warnings,
        })
    }
}

/// A Concept as a briefing item, by what kind of memory it is.
fn concept_item(row: &Json, required: bool) -> Option<Candidate> {
    let id = row["id"].as_str()?;
    let type_name = row["schema_ref"].as_str().map(local).unwrap_or("");
    let attributes = &row["attributes"];
    let name = row["name"].as_str().unwrap_or("");
    let summary = attributes["summary"]
        .as_str()
        .or_else(|| attributes["goal"].as_str())
        .unwrap_or(name);
    // The host's own scope handles are bookkeeping, not memory.
    if attributes["event_class"] == "memory_scope" {
        return None;
    }
    let (role, status, standing, text, required) = match type_name {
        "Insight" if attributes["insight_class"] == "constraint" => (
            ItemRole::Constraint,
            EpistemicStatus::Accepted,
            None,
            format!("Constraint: {summary}"),
            true,
        ),
        "Commitment" => (
            ItemRole::Constraint,
            EpistemicStatus::Accepted,
            None,
            format!(
                "Open commitment ({}): {summary}{}",
                attributes["status"].as_str().unwrap_or("pending"),
                attributes["due_at"]
                    .as_str()
                    .map(|due| format!(", due {due}"))
                    .unwrap_or_default()
            ),
            required,
        ),
        "Experience" | "Event" => {
            let outcome = attributes["outcome_status"].as_str().unwrap_or("unknown");
            let failed = matches!(outcome, "failure" | "aborted");
            (
                if failed {
                    ItemRole::Warning
                } else {
                    ItemRole::Experience
                },
                if failed {
                    EpistemicStatus::Uncertain
                } else {
                    EpistemicStatus::NotApplicable
                },
                None,
                format!(
                    "{} ({outcome}): {summary}",
                    if failed { "Past failure" } else { "Experience" }
                ),
                required || failed,
            )
        }
        "Skill" => {
            let revoked = row["_system"]["lifecycle"]["status"] == "revoked"
                || attributes["status"] == "revoked";
            (
                ItemRole::Procedure,
                EpistemicStatus::NotApplicable,
                // Only the learning level serves validated standing; here a
                // procedure is an unproven candidate, or a revoked warning.
                Some(if revoked {
                    Standing::Revoked
                } else {
                    Standing::Unproven
                }),
                format!("Procedure candidate (unproven, grants no permission): {name}"),
                required,
            )
        }
        "Insight" => (
            ItemRole::Fact,
            EpistemicStatus::Uncertain,
            None,
            format!("Insight: {summary}"),
            required,
        ),
        _ => return None,
    };
    Some(Candidate {
        item: MemoryItem {
            reference: String::new(),
            text: truncate(&text, 4096),
            role,
            epistemic_status: status,
            evidence_refs: vec![],
            action_eligible: false,
            standing,
        },
        pins: vec![(id.to_string(), version_of(row))],
        required,
    })
}

fn role_rank(role: ItemRole) -> u8 {
    match role {
        ItemRole::Constraint => 0,
        ItemRole::Warning => 1,
        ItemRole::Fact => 2,
        ItemRole::Procedure => 3,
        ItemRole::Experience => 4,
        ItemRole::Source => 5,
    }
}

fn channels_of(plans: &[(&'static str, Plan)]) -> Channels {
    let state = |name: &str| {
        plans
            .iter()
            .find(|(channel, _)| *channel == name)
            .map(|(_, plan)| plan.state())
            .unwrap_or(ChannelState::Incomplete)
    };
    Channels {
        constraints: state("constraints"),
        commitments: state("commitments"),
        dependencies: state("dependencies"),
        failures: state("failures"),
        experiences: state("experiences"),
        skills: state("skills"),
        evidence: state("evidence"),
    }
}

fn count_tokens(briefing: &Briefing) -> Result<usize, KipError> {
    let text =
        serde_json::to_string(briefing).map_err(|e| KipError::internal_error(e.to_string()))?;
    crate::recall_budget::count(&text).map_err(|e| KipError::internal_error(e.to_string()))
}

fn dedup(mut values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    values.retain(|v| seen.insert(v.clone()));
    values.truncate(128);
    values
}

fn hex_id() -> String {
    let bytes: [u8; 20] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The Recall pass's question, with the request's scope, times and
/// transient context stated as data.
fn recall_prompt(
    query: &str,
    mode: RecallMode,
    scope: &ResolvedScope,
    valid_at: Option<&str>,
    as_of: Option<u64>,
    input: &RecallInput,
) -> String {
    let mut lines = vec![query.to_string(), String::new()];
    lines.push(format!(
        "[Memory Interface recall — mode {mode:?}. Read-only: write nothing.]"
    ));
    if !scope.contexts.is_empty() {
        lines.push(format!(
            "Scope: only memory whose context_refs are within {:?} (task {}) applies; memory \
             from other tasks does not.",
            scope.contexts,
            scope.task.as_deref().unwrap_or("none")
        ));
    }
    if let Some(at) = valid_at {
        lines.push(format!("Answer for world time {at} (`FOR TIME`)."));
    }
    if let Some(seq) = as_of {
        lines.push(format!("Answer as the Brain stood at `AS OF SEQ {seq}`."));
    }
    if let Some(goal) = &input.goal {
        lines.push(format!("Goal: {goal}"));
    }
    if let Some(context) = &input.context {
        lines.push(format!(
            "Current situation (transient, not memory; do not store): {context}"
        ));
    }
    lines.join("\n")
}

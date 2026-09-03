//! Shared assessment instruments (memory evolution plan, module M0).
//!
//! These are the pieces of the eval harness that are also useful outside it:
//! the semantic-assertion judge, recall trace extraction, and read-only KIP
//! probe helpers. The offline eval harness (`crate::eval`) and the online
//! maintenance self-test (plan module M7) consume the same implementations,
//! so "what CI measures" and "what the brain checks about itself" cannot
//! drift apart.

use anda_core::{
    AgentOutput, BoxError, CompletionRequest, ContentPart, Json, Message, ModelEffort, Usage,
};
use anda_kip::{Request, Response};

use crate::kip;
use serde::{Deserialize, Serialize};

use crate::space::Space;
use crate::types::MemoryCitation;

/// Default similarity threshold for semantic assertion probes.
pub const DEFAULT_ASSERTION_SEARCH_THRESHOLD: f64 = 0.35;

/// Default result limit for semantic assertion probes.
pub const DEFAULT_ASSERTION_SEARCH_LIMIT: usize = 8;

/// Upper bound applied to serialized evidence blobs fed to a judge.
pub(crate) const MAX_EVIDENCE_CHARS: usize = 6_000;

/// Maintenance-backlog probes: episodic memory nobody has consolidated yet.
///
/// KIP 1.x measured this as "concepts still in the `Unsorted` Domain". The
/// Cognitive Memory Profile declares no Domain, so the backlog is now what
/// Maintenance actually owes: Events and Experiences with no `consolidated_to`
/// lineage. One probe per type, summed — a structural field is a schema symbol,
/// so a single query cannot range over both.
pub const UNCONSOLIDATED_COUNT_KQL: &[&str] = &[
    "FIND(COUNT(?x)) WHERE { ?x CONCEPT {type: \"Event\"} NOT { STRUCTURAL (?x, \"consolidated_to\", ?to) } }",
    "FIND(COUNT(?x)) WHERE { ?x CONCEPT {type: \"Experience\"} NOT { STRUCTURAL (?x, \"consolidated_to\", ?to) } }",
];

/// Orphan probe: Concepts no Proposition mentions on either side.
///
/// The 1.x sweep asked which Concepts lacked a `belongs_to_domain` link, and
/// had to iterate the `$ConceptType` inventory because `?n {}` was a syntax
/// error. KIP 2.0 accepts an unconstrained Concept pattern, so the whole census
/// is one query — and it now measures the thing the old one stood in for: a
/// Concept nothing says anything about.
const ORPHAN_COUNT_KQL: &str =
    "FIND(COUNT(?c)) WHERE { ?c CONCEPT {} NOT { (?c, ?out, ?o) } NOT { (?s, ?in, ?c) } }";

/// Minimal capabilities the assessment instruments need from their host:
/// one-shot LLM completions (for judges and simulators) and read-only KIP
/// access (for graph probes). `Space` implements it directly; eval drivers
/// inherit it as a supertrait of `EvalDriver`.
#[async_trait::async_trait]
pub trait AssessContext: Send + Sync {
    /// One-shot LLM completion. Hosts without a model can leave the default.
    async fn complete(&self, _req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        Err("assess context does not support LLM completions".into())
    }

    /// Completion used by judges. Defaults to [`Self::complete`]; hosts with
    /// an independent judge model override this (plan M9), so judge scores
    /// stop sharing the evaluated system's blind spots.
    async fn judge_complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.complete(req).await
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError>;
}

#[async_trait::async_trait]
impl AssessContext for Space {
    async fn complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        self.eval_complete(req).await
    }

    async fn judge_complete(&self, req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        match self.judge_model() {
            Some(model) => model.completion(req).await,
            None => self.eval_complete(req).await,
        }
    }

    async fn execute_kip_readonly(&self, request: Request) -> Result<Response, BoxError> {
        // Inherent method (space.rs); takes priority over this trait method.
        self.execute_kip_readonly(request).await
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecallTrace {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolTrace>,
}

impl RecallTrace {
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut tools: Vec<ToolTrace> = Vec::new();

        for message in messages {
            for part in &message.content {
                match part {
                    ContentPart::ToolCall {
                        name,
                        args,
                        call_id,
                    } => tools.push(ToolTrace {
                        name: name.clone(),
                        args: args.clone(),
                        call_id: call_id.clone(),
                        output: None,
                        is_error: None,
                    }),
                    ContentPart::ToolOutput {
                        name,
                        output,
                        is_error,
                        call_id,
                        ..
                    } => {
                        if let Some(existing) = tools.iter_mut().rev().find(|trace| {
                            trace.output.is_none()
                                && trace.name == *name
                                && (call_id.is_none() || trace.call_id == *call_id)
                        }) {
                            existing.output = Some(output.clone());
                            existing.is_error = *is_error;
                        } else {
                            tools.push(ToolTrace {
                                name: name.clone(),
                                args: Json::Null,
                                call_id: call_id.clone(),
                                output: Some(output.clone()),
                                is_error: *is_error,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        Self { tools }
    }

    /// Checks whether any term appears in a tool *output*. Tool names and
    /// args are deliberately excluded: recall echoes the user's query into
    /// search args, so matching them would misread "searched for it" as
    /// "retrieved it" and flip grounding failures into synthesis failures.
    pub fn contains_any_term(&self, terms: &[String]) -> bool {
        if terms.is_empty() {
            return false;
        }

        let haystack = self
            .tools
            .iter()
            .filter_map(|tool| tool.output.as_ref())
            .map(|output| serde_json::to_string(output).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        terms
            .iter()
            .any(|term| !term.trim().is_empty() && haystack.contains(&term.to_lowercase()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolTrace {
    pub name: String,
    pub args: Json,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Json>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A judge invocation's verdict plus its token usage.
#[derive(Debug, Clone)]
pub struct JudgeCall<T> {
    pub verdict: T,
    pub usage: Usage,
}

/// Verdict for one semantic graph probe.
#[derive(Debug, Clone, Deserialize)]
pub struct AssertionVerdict {
    pub holds: bool,

    #[serde(default)]
    pub reason: String,
}

const ASSERTION_INSTRUCTIONS: &str = r#"You are inspecting a knowledge graph for an AI memory system. You will receive a statement about what the graph may currently assert, plus raw evidence returned by a semantic graph search.

Decide whether the evidence shows the statement currently holds in the graph. Superseded, archived, expired, or explicitly deactivated memories do NOT count as holding. Absence of any matching evidence means the statement does not hold. Do not assume facts beyond the evidence.

Respond with ONLY a JSON object: {"holds": true, "reason": "..."}"#;

/// Asks the judge whether `evidence` shows that `assertion` currently holds
/// in the graph.
pub async fn judge_assertion<C>(
    ctx: &C,
    assertion: &str,
    evidence: &Json,
) -> Result<JudgeCall<AssertionVerdict>, BoxError>
where
    C: AssessContext + ?Sized,
{
    let prompt = format!(
        "# Statement to verify\n{}\n\n# Graph search evidence\n{}",
        assertion,
        truncate_chars(
            &serde_json::to_string(evidence).unwrap_or_default(),
            MAX_EVIDENCE_CHARS
        ),
    );

    let output = ctx
        .judge_complete(CompletionRequest {
            instructions: ASSERTION_INSTRUCTIONS.to_string(),
            prompt,
            effort: Some(ModelEffort::Low),
            ..Default::default()
        })
        .await?;

    Ok(JudgeCall {
        verdict: parse_json_payload(&output.content)?,
        usage: output.usage,
    })
}

/// Builds the semantic search command for an assertion probe. The search
/// text is embedded in a KQL string literal via the crate's shared escaping
/// helper ([`crate::kip::string_literal`]).
pub fn assertion_search_request(search: &str, threshold: f64, limit: usize) -> Request {
    // MODE is omitted deliberately: the engine picks hybrid where it has
    // semantic capability and keyword otherwise, whereas asking for `semantic`
    // outright is an `UnsupportedCapability` refusal on a deployment with no
    // embedding model — which a probe would read as "the graph holds nothing".
    kip::request_with(
        "SEARCH CONCEPT :term THRESHOLD :threshold LIMIT :limit",
        serde_json::Map::from_iter([
            ("term".to_string(), Json::from(search)),
            ("threshold".to_string(), Json::from(threshold)),
            ("limit".to_string(), Json::from(limit)),
        ]),
    )
}

/// Extracts the first JSON object from model output, tolerating code fences
/// and prose around it.
pub fn parse_json_payload<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, BoxError> {
    let start = text
        .find('{')
        .ok_or_else(|| format!("no JSON object in judge output: {text:.120}"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| format!("unterminated JSON object in judge output: {text:.120}"))?;
    if end < start {
        return Err(format!("malformed JSON object in judge output: {text:.120}").into());
    }
    Ok(serde_json::from_str(&text[start..=end])?)
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("…(truncated)");
    out
}

/// True for an element id of the given KIP 2.0 kind tag.
///
/// KIP 2.0 mints `<tag>-<row>` — `C-7`, `P-11`, `A-3`, `E-2`, `X-1` — where 1.x
/// used `C:7` and packed the predicate into `P:11:has_allergy`. The tag is the
/// only type information a bare reference carries, which is why these matchers
/// read it rather than guessing from context.
fn is_element_id(value: &str, tag: char) -> bool {
    value
        .strip_prefix(tag)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|seq| !seq.is_empty() && seq.bytes().all(|byte| byte.is_ascii_digit()))
}

/// True for Concept ids of the form `"C-<u64>"`.
pub fn is_concept_entity_id(value: &str) -> bool {
    is_element_id(value, 'C')
}

/// True for Proposition ids of the form `"P-<u64>"`.
pub fn is_proposition_entity_id(value: &str) -> bool {
    is_element_id(value, 'P')
}

/// True for Assertion ids of the form `"A-<u64>"`.
pub fn is_assertion_entity_id(value: &str) -> bool {
    is_element_id(value, 'A')
}

/// True for any Cognitive Element id the brain cites or meters.
///
/// Evidence (`E-`) and Activity (`X-`) are deliberately absent: they are
/// provenance records rather than memories, and counting a recall of one as
/// usage of a memory would reinforce the receipt instead of the fact.
pub fn is_entity_id(value: &str) -> bool {
    is_concept_entity_id(value) || is_proposition_entity_id(value) || is_assertion_entity_id(value)
}

/// The local name of a schema symbol reference.
///
/// A persisted reference names one exact version —
/// `kip://profiles/cognitive-memory@2.0.0/Person` — because that is what keeps
/// an element's meaning from drifting when a package is republished. What a
/// caller wants to read is `Person`.
pub fn local_symbol_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

impl RecallTrace {
    /// Element ids (`C-*` / `P-*` / `A-*`) surfaced in successful tool outputs.
    /// This is the usage-ledger signal (plan module M1): which memories a
    /// recall actually retrieved.
    pub fn entity_ids(&self) -> std::collections::BTreeSet<String> {
        let mut ids = std::collections::BTreeSet::new();
        for tool in &self.tools {
            if tool.is_error == Some(true) {
                continue;
            }
            if let Some(output) = &tool.output {
                collect_entity_objects(output, &mut |id, _| {
                    ids.insert(id.to_string());
                });
            }
        }
        ids
    }
}

/// Deterministic memory citations for a recall answer (plan module M4):
/// the entities the trace shows were retrieved, with type/name/confidence
/// harvested from the tool outputs when present. Never trusts the model to
/// self-report which ids it used.
pub fn extract_memory_citations(trace: &RecallTrace) -> Vec<MemoryCitation> {
    let mut seen = std::collections::BTreeSet::new();
    let mut citations = Vec::new();
    for tool in &trace.tools {
        if tool.is_error == Some(true) {
            continue;
        }
        let Some(output) = &tool.output else { continue };
        collect_citations(output, &mut seen, &mut citations);
    }
    citations
}

/// Memory citations found anywhere in one KIP result JSON (plan module M5:
/// the metamemory probe reports its hits in the same shape recall does).
pub fn citations_from_json(value: &Json) -> Vec<MemoryCitation> {
    let mut seen = std::collections::BTreeSet::new();
    let mut citations = Vec::new();
    collect_citations(value, &mut seen, &mut citations);
    citations
}

fn collect_citations(
    value: &Json,
    seen: &mut std::collections::BTreeSet<String>,
    citations: &mut Vec<MemoryCitation>,
) {
    collect_entity_objects(value, &mut |id, object| {
        if !seen.insert(id.to_string()) {
            return;
        }
        // KIP 1.x kept every one of these in one `metadata` bag. KIP 2.0 puts
        // each where it belongs: the type in the Concept's `schema_ref` or the
        // Proposition's `predicate_ref`, engine truth under `_system`, and the
        // stance — which only an Assertion has — on the Assertion itself.
        let r#type = object
            .get("schema_ref")
            .or_else(|| object.get("predicate_ref"))
            .and_then(Json::as_str)
            .map(|reference| local_symbol_name(reference).to_string());
        citations.push(MemoryCitation {
            entity: id.to_string(),
            r#type,
            name: object
                .get("name")
                .and_then(Json::as_str)
                .map(str::to_string),
            confidence: object.get("confidence").and_then(Json::as_f64),
            // Whose stance this is, when the cited element carries one. Never
            // the caller's Principal: attribution is cognition, authority is
            // Governance, and citing one as the other is the confusion KIP 2.0
            // exists to prevent.
            source: object.get("asserted_by").and_then(element_reference),
            created_at: object
                .get("_system")
                .and_then(|system| system.get("created_at"))
                .and_then(Json::as_str)
                .map(str::to_string),
        });
    });
}

/// Reads an element reference — `{"id": "C-7"}` or a bare id string.
fn element_reference(value: &Json) -> Option<String> {
    match value {
        Json::String(id) => Some(id.clone()),
        Json::Object(map) => map.get("id").and_then(Json::as_str).map(str::to_string),
        _ => None,
    }
}

/// Walks a KIP result recursively, visiting every object that carries a
/// Cognitive Element `id`.
pub(crate) fn collect_entity_objects(
    value: &Json,
    visit: &mut impl FnMut(&str, &serde_json::Map<String, Json>),
) {
    match value {
        Json::Array(items) => {
            for item in items {
                collect_entity_objects(item, visit);
            }
        }
        Json::Object(map) => {
            // A lone `{"id": ...}` is a reference, not a retrieved element —
            // `asserted_by`, an evidence citation, a structural edge. Metering
            // one as a recall would reinforce a memory nobody read, and
            // reinforcement is supposed to track what the answer actually used.
            //
            // A SEARCH hit is `{id, kind, score, element}`: the wrapper repeats
            // the id but holds none of the content, so visiting it would take
            // the id and leave the real element deduplicated away — every
            // citation coming back with no type and no name.
            if let Some(Json::String(id)) = map.get("id")
                && map.len() > 1
                && is_entity_id(id)
                && !wraps_element(map, id)
            {
                visit(id, map);
            }
            for nested in map.values() {
                collect_entity_objects(nested, visit);
            }
        }
        _ => {}
    }
}

/// Whether this object is an envelope around the element it names, rather than
/// the element itself.
fn wraps_element(map: &serde_json::Map<String, Json>, id: &str) -> bool {
    map.get("element")
        .and_then(Json::as_object)
        .and_then(|element| element.get("id"))
        .and_then(Json::as_str)
        == Some(id)
}

/// Opening tag of the recall self-report footer (plan module M4).
pub const RECALL_META_TAG_OPEN: &str = "<memory_meta>";

/// Closing tag of the recall self-report footer.
pub const RECALL_META_TAG_CLOSE: &str = "</memory_meta>";

/// The recall model's structured self-report, appended to its final answer.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct RecallMeta {
    /// Whether the graph held relevant memory for the query.
    #[serde(default)]
    pub found: Option<bool>,

    /// Self-assessed uncertainty of the answer, 0 (certain) ..= 1 (guessing).
    #[serde(default)]
    pub uncertainty: Option<f64>,
}

/// Splits the `<memory_meta>{...}</memory_meta>` self-report off a recall
/// answer, tail-anchored: the LAST closing tag and its nearest preceding
/// opening tag delimit the one block that is stripped and parsed; trailing
/// prose after the block is joined back onto the answer. Everything else —
/// earlier echoed example blocks, prose mentions, unclosed opens — stays in
/// the answer as literal text: marker leakage is accepted, content loss
/// never is. An absent or malformed payload degrades to `None` — the footer
/// is an enhancement, never a failure mode.
pub fn split_recall_meta(content: &str) -> (String, Option<RecallMeta>) {
    let Some(close) = content.rfind(RECALL_META_TAG_CLOSE) else {
        return (content.trim_end().to_string(), None);
    };
    let Some(open) = content[..close].rfind(RECALL_META_TAG_OPEN) else {
        return (content.trim_end().to_string(), None);
    };

    let meta = parse_json_payload::<RecallMeta>(&content[open + RECALL_META_TAG_OPEN.len()..close])
        .ok()
        .map(|meta| RecallMeta {
            uncertainty: meta
                .uncertainty
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 1.0)),
            ..meta
        });

    let before = content[..open].trim();
    let after = content[close + RECALL_META_TAG_CLOSE.len()..].trim();
    let answer = match (before.is_empty(), after.is_empty()) {
        (false, false) => format!("{before}\n{after}"),
        (false, true) => before.to_string(),
        (true, _) => after.to_string(),
    };
    (answer, meta)
}

/// Runs a read-only KIP count query and digs out the first integer in the
/// result. Returns `None` on error so callers degrade gracefully — but the
/// failure is always logged: a silently swallowed probe error once hid a
/// health metric that had never worked at all.
pub async fn kip_count<C>(ctx: &C, command: &str) -> Option<u64>
where
    C: AssessContext + ?Sized,
{
    match ctx.execute_kip_readonly(kip::request(command)).await {
        Ok(response) if kip::succeeded(&response) => {
            let count = kip::ok_result(&response).and_then(first_integer);
            if count.is_none() {
                log::warn!(target: "brain", command = command; "kip_count: no integer in result");
            }
            count
        }
        Ok(response) => {
            let error = kip::error_message(&response);
            log::warn!(target: "brain", command = command, error:% = error; "kip_count: KIP error");
            None
        }
        Err(err) => {
            log::warn!(target: "brain", command = command, error:% = err; "kip_count: request failed");
            None
        }
    }
}

/// Sums a group of count probes, answering `None` when any leg fails so a
/// partial sum is never mistaken for a census.
pub async fn kip_count_sum<C>(ctx: &C, commands: &[&str]) -> Option<u64>
where
    C: AssessContext + ?Sized,
{
    let mut total = 0u64;
    for command in commands {
        total = total.saturating_add(kip_count(ctx, command).await?);
    }
    Some(total)
}

/// Counts Concepts nothing refers to.
///
/// Heavy — it scans the Space's Propositions — so callers run it at settlement
/// time only. Returns `None` (with the failure logged) when the probe fails, so
/// a missing census is never mistaken for a clean graph.
pub async fn orphan_count<C>(ctx: &C) -> Option<u64>
where
    C: AssessContext + ?Sized,
{
    kip_count(ctx, ORPHAN_COUNT_KQL).await
}

pub fn first_integer(value: &Json) -> Option<u64> {
    match value {
        Json::Number(number) => number.as_u64(),
        Json::Array(items) => items.iter().find_map(first_integer),
        Json::Object(map) => map.values().find_map(first_integer),
        _ => None,
    }
}

pub fn response_hit_count(response: &Response) -> usize {
    response
        .results
        .iter()
        .filter_map(|result| result.result.as_ref())
        .map(json_hit_count)
        .sum()
}

fn json_hit_count(value: &Json) -> usize {
    match value {
        Json::Null => 0,
        Json::Bool(false) => 0,
        Json::Bool(true) => 1,
        Json::Number(number) => {
            if number.as_f64().unwrap_or_default() == 0.0 {
                0
            } else {
                1
            }
        }
        Json::String(text) => usize::from(!text.trim().is_empty()),
        Json::Array(items) => {
            if items.iter().all(looks_like_serialized_kip_response) {
                items.iter().map(json_hit_count).sum()
            } else {
                items.len()
            }
        }
        Json::Object(map) => {
            if map.is_empty() {
                0
            } else if let Some(result) = map.get("result") {
                json_hit_count(result)
            } else if map.contains_key("error") {
                0
            } else {
                1
            }
        }
    }
}

fn looks_like_serialized_kip_response(value: &Json) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.contains_key("result") || map.contains_key("error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_core::ToolOutput;
    use serde_json::json;

    #[test]
    fn recall_trace_extracts_tool_calls_and_outputs() {
        let call = ContentPart::ToolCall {
            name: "execute_kip_readonly".to_string(),
            args: json!({"command": "FIND(?x) WHERE { ?x {type: \"Preference\"} }"}),
            call_id: Some("call_1".to_string()),
        };
        let output = ToolOutput::new(json!([{"name": "prefers concise"}]));
        let output = ContentPart::ToolOutput {
            name: "execute_kip_readonly".to_string(),
            output: json!(output.output),
            is_error: None,
            call_id: Some("call_1".to_string()),
            remote_id: None,
        };
        let messages = vec![Message {
            role: "assistant".to_string(),
            content: vec![call, output],
            ..Default::default()
        }];

        let trace = RecallTrace::from_messages(&messages);

        assert_eq!(trace.tools.len(), 1);
        assert!(trace.contains_any_term(&["concise".to_string()]));
        // Terms that only appear in args must not count as evidence.
        assert!(!trace.contains_any_term(&["Preference".to_string()]));
    }

    #[test]
    fn entity_id_matchers_accept_element_ids_only() {
        assert!(is_concept_entity_id("C-7"));
        assert!(is_proposition_entity_id("P-11"));
        assert!(is_assertion_entity_id("A-3"));
        assert!(is_entity_id("P-0"));
        // Evidence and Activity are provenance records, not memories: metering
        // a recall of one as usage would reinforce the receipt, not the fact.
        assert!(!is_entity_id("E-2"));
        assert!(!is_entity_id("X-1"));
        // The KIP 1.x spellings must not match either, or a stale ledger row
        // would silently keep metering an id nothing in the graph answers to.
        for bad in [
            "C:7",
            "P:11:likes",
            "C-",
            "C-x",
            "P11",
            "C--1",
            "call_1",
            "wiki://x",
            "",
        ] {
            assert!(!is_entity_id(bad), "{bad} must not match");
        }
    }

    #[test]
    fn entity_ids_and_citations_come_from_successful_outputs_only() {
        let trace = RecallTrace {
            tools: vec![
                ToolTrace {
                    name: "execute_kip_readonly".to_string(),
                    args: json!({"command": "mentions C-50 in args only"}),
                    call_id: None,
                    output: Some(json!({"result": [
                        {"id": "C-7",
                         "schema_ref": "kip://profiles/cognitive-memory@2.0.0/Preference",
                         "name": "oolong",
                         "_system": {"created_at": "2026-07-01T00:00:00.000Z"}},
                        {"id": "A-3", "confidence": 0.8,
                         "asserted_by": {"id": "C-1"},
                         "_system": {"created_at": "2026-07-02T00:00:00.000Z"}},
                        {"id": "P-3",
                         "predicate_ref": "kip://profiles/cognitive-memory@2.0.0/prefers"},
                        {"id": "not-an-entity"},
                        {"nested": [{"id": "C-7"}]}
                    ]})),
                    is_error: None,
                },
                ToolTrace {
                    name: "execute_kip_readonly".to_string(),
                    args: Json::Null,
                    call_id: None,
                    output: Some(json!([{"id": "C-99"}])),
                    is_error: Some(true),
                },
            ],
        };

        let ids = trace.entity_ids();
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec!["A-3".to_string(), "C-7".to_string(), "P-3".to_string()]
        );

        let citations = extract_memory_citations(&trace);
        assert_eq!(citations.len(), 3);
        // A type is read from the exact schema reference and shown by its local
        // name: the version is what keeps the meaning pinned, not what a reader
        // needs to see.
        assert_eq!(citations[0].entity, "C-7");
        assert_eq!(citations[0].r#type.as_deref(), Some("Preference"));
        assert_eq!(citations[0].name.as_deref(), Some("oolong"));
        assert_eq!(
            citations[0].created_at.as_deref(),
            Some("2026-07-01T00:00:00.000Z")
        );
        // Only an Assertion carries a stance, so only an Assertion cites one.
        assert_eq!(citations[0].confidence, None);
        assert_eq!(citations[1].entity, "A-3");
        assert_eq!(citations[1].confidence, Some(0.8));
        assert_eq!(citations[1].source.as_deref(), Some("C-1"));
        // A Proposition's "type" is the predicate it relates its endpoints by.
        assert_eq!(citations[2].entity, "P-3");
        assert_eq!(citations[2].r#type.as_deref(), Some("prefers"));
        assert_eq!(citations[2].confidence, None);
    }

    #[test]
    fn split_recall_meta_strips_footer_and_degrades_gracefully() {
        let (answer, meta) = split_recall_meta(
            "Answer.\n<memory_meta>{\"found\": true, \"uncertainty\": 0.25}</memory_meta>",
        );
        assert_eq!(answer, "Answer.");
        let meta = meta.unwrap();
        assert_eq!(meta.found, Some(true));
        assert_eq!(meta.uncertainty, Some(0.25));

        // Malformed payload: the tag block is still stripped.
        let (answer, meta) = split_recall_meta("Answer.\n<memory_meta>oops</memory_meta>");
        assert_eq!(answer, "Answer.");
        assert!(meta.is_none());

        // No footer at all.
        let (answer, meta) = split_recall_meta("Plain answer.  ");
        assert_eq!(answer, "Plain answer.");
        assert!(meta.is_none());

        // Out-of-range uncertainty clamps; prose after the footer survives.
        let (answer, meta) =
            split_recall_meta("Answer.\n<memory_meta>{\"uncertainty\": 7.0}</memory_meta>\ntail");
        assert_eq!(answer, "Answer.\ntail");
        assert_eq!(meta.unwrap().uncertainty, Some(1.0));

        // Tail-anchored: only the LAST closed block is stripped. An earlier
        // echoed example block stays in the answer as literal text — marker
        // leakage is accepted, content loss never is.
        let (answer, meta) = split_recall_meta(
            "<memory_meta>{\"uncertainty\": 0.9}</memory_meta>\nAnswer.\n\
             <memory_meta>{\"found\": true, \"uncertainty\": 0.1}</memory_meta>",
        );
        assert_eq!(
            answer,
            "<memory_meta>{\"uncertainty\": 0.9}</memory_meta>\nAnswer."
        );
        let meta = meta.unwrap();
        assert_eq!(meta.found, Some(true));
        assert_eq!(meta.uncertainty, Some(0.1));

        // No closing tag anywhere: nothing is stripped and nothing is
        // salvaged — an unclosed open (prose mention or truncated footer)
        // stays in the answer verbatim.
        let (answer, meta) = split_recall_meta("The <memory_meta> tag marks the footer, see docs.");
        assert_eq!(answer, "The <memory_meta> tag marks the footer, see docs.");
        assert!(meta.is_none());
        let (answer, meta) = split_recall_meta("Answer.\n<memory_meta>{\"found\": false}");
        assert_eq!(answer, "Answer.\n<memory_meta>{\"found\": false}");
        assert!(meta.is_none());

        // A prose mention followed by the real footer: the close pairs with
        // its NEAREST preceding open, so all answer content survives — the
        // mentioned tag is kept as literal text.
        let (answer, meta) = split_recall_meta(
            "I found it. As instructed, the <memory_meta> footer follows.\n\
             Your meeting is on Friday at 3pm.\n\
             <memory_meta>{\"found\": true, \"uncertainty\": 0.1}</memory_meta>",
        );
        assert_eq!(
            answer,
            "I found it. As instructed, the <memory_meta> footer follows.\n\
             Your meeting is on Friday at 3pm."
        );
        let meta = meta.unwrap();
        assert_eq!(meta.found, Some(true));
        assert_eq!(meta.uncertainty, Some(0.1));
    }

    #[test]
    fn response_hit_count_handles_batch_responses() {
        let response = Response::ok(json!([
            {"result": [{"name": "a"}, {"name": "b"}]},
            {"result": []},
            {"error": {"code": "KIP_3002"}}
        ]));

        assert_eq!(response_hit_count(&response), 2);
    }

    #[test]
    fn first_integer_digs_into_kip_count_results() {
        assert_eq!(first_integer(&json!([{"result": [{"count": 7}]}])), Some(7));
        assert_eq!(first_integer(&json!("nope")), None);
        assert_eq!(first_integer(&json!(3)), Some(3));
    }

    #[test]
    fn an_assertion_probe_binds_its_term_instead_of_splicing_it() {
        // The search term comes from an eval set, and a quote or a backslash in
        // it used to have to be escaped into the command text. A bound
        // parameter occupies a complete value position, so the term is data
        // even when it looks like syntax.
        let request = assertion_search_request("say \"hi\" \\ bye", 0.5, 3);
        request.validate().unwrap();
        assert_eq!(
            request.operations[0].command.as_deref(),
            Some("SEARCH CONCEPT :term THRESHOLD :threshold LIMIT :limit")
        );
        assert_eq!(
            request.parameters.as_ref().unwrap()["term"],
            Json::from("say \"hi\" \\ bye")
        );
    }

    #[test]
    fn parse_json_payload_rejects_non_json() {
        assert!(parse_json_payload::<AssertionVerdict>("no json here").is_err());
    }

    #[test]
    fn parse_assertion_verdict() {
        let verdict: AssertionVerdict =
            parse_json_payload("{\"holds\": false, \"reason\": \"superseded\"}").unwrap();
        assert!(!verdict.holds);
        assert_eq!(verdict.reason, "superseded");
    }

    #[test]
    fn truncate_chars_bounds_output() {
        let text = "x".repeat(100);
        let out = truncate_chars(&text, 10);
        assert!(out.starts_with("xxxxxxxxxx"));
        assert!(out.ends_with("(truncated)"));
        assert_eq!(truncate_chars("short", 10), "short");
    }
}

//! Online memory diagnostics and typed, read-only KIP observations.
//!
//! Self-test, shadow diagnostics, Recall citations/metadata and status counters
//! share these instruments. SEARCH describes retrieval; only an explicit
//! engine BELIEF projection can answer a belief question. Offline product
//! evaluation belongs to MIB; none of these diagnostics grants Skill standing.

use anda_core::{BoxError, ContentPart, Json, Message};

use crate::kip;

mod probe;
pub use probe::{
    ProbeObservation, SearchObservation, probe_observation, search_observation, single_read_result,
};
use serde::{Deserialize, Serialize};

use crate::types::MemoryCitation;

/// Maintenance-backlog probes: episodic memory nobody has consolidated yet.
///
/// KIP 1.x measured this as "concepts still in the `Unsorted` Domain". The
/// Cognitive Memory Profile declares no Domain, so the backlog is now what
/// Maintenance actually owes: Events and Experiences no formation or
/// consolidation Activity has taken as input and produced something from.
/// That is Activity provenance read directly: `consolidated_to` is a computed,
/// read-only view of the same thing (Profile §7), and nothing writes it.
pub const UNCONSOLIDATED_COUNT_KQL: &[&str] = &[
    "FIND(COUNT(?x)) WHERE { ?x CONCEPT {type: \"Event\"} NOT { ?a ACTIVITY {} STRUCTURAL (?a, \"inputs\", ?x) STRUCTURAL (?a, \"outputs\", ?to) FILTER(?a.activity_class == \"experience_formation\" || ?a.activity_class == \"semantic_consolidation\" || ?a.activity_class == \"procedural_consolidation\") } }",
    "FIND(COUNT(?x)) WHERE { ?x CONCEPT {type: \"Experience\"} NOT { ?a ACTIVITY {} STRUCTURAL (?a, \"inputs\", ?x) STRUCTURAL (?a, \"outputs\", ?to) FILTER(?a.activity_class == \"experience_formation\" || ?a.activity_class == \"semantic_consolidation\" || ?a.activity_class == \"procedural_consolidation\") } }",
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

mod context;
pub use context::AssessContext;

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
            confidence: is_assertion_entity_id(id)
                .then(|| object.get("confidence").and_then(Json::as_f64))
                .flatten(),
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

/// Visits returned elements through known protocol containers. Element
/// attributes, Evidence payloads, explanations and reference objects are not
/// recursively promoted into retrieved memories.
pub(crate) fn collect_entity_objects(
    value: &Json,
    visit: &mut impl FnMut(&str, &serde_json::Map<String, Json>),
) {
    collect_element_objects(value, &mut |id, row| {
        if is_entity_id(id) {
            visit(id, row);
        }
    });
}

/// Typed elements actually returned by a read, including provenance records.
/// Memory Interface scope checks need these; usage metering still excludes them.
pub(crate) fn collect_element_objects(
    value: &Json,
    visit: &mut impl FnMut(&str, &serde_json::Map<String, Json>),
) {
    match value {
        Json::Array(items) => {
            for item in items {
                collect_element_objects(item, visit);
            }
        }
        Json::Object(map) => {
            if map.get("error").is_some_and(|error| !error.is_null()) {
                return;
            }
            if map.contains_key("kip") {
                if map.get("kip").and_then(Json::as_str) != Some("2.0")
                    || !matches!(
                        map.get("status").and_then(Json::as_str),
                        Some("succeeded" | "partial")
                    )
                {
                    return;
                }
                if let Some(Json::Array(results)) = map.get("results") {
                    for result in results {
                        // Only confirmed successful operations in a partial
                        // batch contribute retrieval diagnostics.
                        if result.get("status").and_then(Json::as_str) == Some("succeeded") {
                            collect_element_objects(result, visit);
                        }
                    }
                }
                return;
            }
            if let Some(result) = map.get("result") {
                if map
                    .get("status")
                    .is_none_or(|status| status.as_str() == Some("succeeded"))
                {
                    collect_element_objects(result, visit);
                }
                return;
            }
            if let Some(Json::Array(hits)) = map.get("hits") {
                for hit in hits {
                    if let Some(element) = hit.get("element")
                        && hit.get("id").and_then(Json::as_str)
                            == element.get("id").and_then(Json::as_str)
                    {
                        collect_element_objects(element, visit);
                    }
                }
                return;
            }
            if let Some(element) = map.get("element") {
                if map.get("id").and_then(Json::as_str) == element.get("id").and_then(Json::as_str)
                {
                    collect_element_objects(element, visit);
                }
                return;
            }
            if let Some(Json::String(id)) = map.get("id")
                && id.parse::<anda_cognitive_nexus::ElementId>().is_ok()
                && (map.contains_key("_system")
                    || map.contains_key("schema_ref")
                    || map.contains_key("predicate_ref")
                    || map.contains_key("asserted_by"))
            {
                visit(id, map);
            }
        }
        _ => {}
    }
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
        Ok(response) if single_read_result(&response).is_ok() => {
            let count = single_read_result(&response).ok().and_then(first_integer);
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
        Json::Array(items) if items.len() == 1 => first_integer(&items[0]),
        Json::Object(map) if !map.get("error").is_some_and(|error| !error.is_null()) => map
            .get("result")
            .or_else(|| map.get("count"))
            .and_then(first_integer),
        _ => None,
    }
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
            args: json!({"command": "FIND(?x) WHERE { ?x {type: \"TeaKind\"} }"}),
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
        assert_eq!(trace.tools[0].call_id.as_deref(), Some("call_1"));
        assert_eq!(
            trace.tools[0].output,
            Some(json!([{"name":"prefers concise"}]))
        );
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
                         "schema_ref": "kip://anda-brain/memory@1.0.1/TeaKind",
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
        assert_eq!(citations[0].r#type.as_deref(), Some("TeaKind"));
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

    #[tokio::test]
    async fn search_observation_reads_real_nexus_results() {
        use crate::testkit::{app_state_core, create_loaded_space};
        use anda_engine::model::Models;
        use std::sync::Arc;

        let app = app_state_core(
            "search_counts",
            Arc::new(Models::default()),
            vec![],
            "test",
            0,
        );
        let space = create_loaded_space(&app, "search_counts").await;
        // No model is installed: these probes execute the real local Nexus.
        let response = space
            .execute_kip_readonly(crate::kip::request(
                "SEARCH CONCEPT \"unfindable-zqxv\" LIMIT 8",
            ))
            .await
            .unwrap();
        assert!(crate::kip::succeeded(&response), "{response:?}");
        assert_eq!(crate::kip::ok_result(&response).unwrap()["hits"], json!([]));
        assert_eq!(search_observation(&response).unwrap().hits.len(), 0);

        let inserted = anda_kip::execute_request(
            space.memory.nexus().as_ref(),
            &crate::kip::request(
                r#"MUTATE {
                CREATE CONCEPT ?a {TYPE "Person" NAME "probeunique Alpha"}
                CREATE CONCEPT ?b {TYPE "Person" NAME "probeunique Beta"}
            }"#,
            ),
        )
        .await;
        assert!(crate::kip::succeeded(&inserted), "{inserted:?}");
        let response = space
            .execute_kip_readonly(crate::kip::request(
                "SEARCH CONCEPT \"probeunique\" LIMIT 8",
            ))
            .await
            .unwrap();
        assert!(crate::kip::succeeded(&response), "{response:?}");
        let hits = crate::kip::ok_result(&response).unwrap()["hits"]
            .as_array()
            .unwrap();
        assert_eq!(
            search_observation(&response).unwrap().hits.len(),
            hits.len()
        );
        assert_eq!(hits.len(), 2);
        space.close().await.unwrap();
    }

    #[test]
    fn first_integer_digs_into_kip_count_results() {
        assert_eq!(first_integer(&json!([{"result": [{"count": 7}]}])), Some(7));
        assert_eq!(first_integer(&json!("nope")), None);
        assert_eq!(first_integer(&json!(3)), Some(3));
    }

    #[test]
    fn parse_json_payload_rejects_non_json() {
        assert!(parse_json_payload::<Json>("no json here").is_err());
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

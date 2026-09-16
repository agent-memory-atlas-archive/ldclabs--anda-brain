//! The dream self-test (plan M7): after a maintenance cycle, sample recent
//! memories and ask whether search can still surface them.
//!
//! A memory that exists but cannot be found is a memory the Brain will never
//! use, and nothing else in the system notices that. A miss files a
//! `consolidate` SleepTask for the next cycle, keyed by the Concept it is
//! about, so re-encoding happens where the words a person would search for
//! are chosen — not here.

use super::*;

impl Space {
    /// Fires the dream self-test in the background (plan M7); called after a
    /// maintenance cycle completes. Skipped when disabled by policy or when
    /// a pass is already running.
    pub(super) fn kick_memory_self_test(self: &Arc<Self>) {
        if self.memory_policy().self_test_queries_per_cycle == 0 {
            return;
        }
        let space = self.clone();
        tokio::spawn(async move {
            match space.run_memory_self_test(unix_ms()).await {
                Ok(Some(report)) => {
                    log::info!(
                        target: "brain",
                        space_id = space.id,
                        report:serde = report;
                        "memory self-test completed"
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "memory self-test failed: {err:?}"
                    );
                }
            }
        });
    }

    /// The dream self-test (plan M7): sample memories the brain has not probed
    /// yet, generate one natural query each (single LLM call), and check
    /// whether search actually surfaces them. Ungroundable memories become
    /// SleepTasks the next full maintenance re-encodes. Self-test retrievals
    /// count only into `self_test_count` — never into usage reinforcement.
    ///
    /// KIP 1.x stamped `metadata.self_tested_at` on each sampled link so the
    /// next pass would skip it. A Proposition is immutable and carries no
    /// metadata, so coverage is paced by a Space-sequence cursor instead: each
    /// pass reads the window after the last one and parks the cursor at its
    /// end, wrapping around once the horizon has passed. The bookkeeping that
    /// remains — which memories were tested, and how often — lives in the usage
    /// ledger, where it never touches cognition at all.
    ///
    /// Returns `None` when disabled, already running, or nothing qualifies.
    pub(super) async fn run_memory_self_test(
        &self,
        now_ms: u64,
    ) -> Result<Option<SelfTestReport>, BoxError> {
        let Ok(_guard) = self.self_test_lock.try_lock() else {
            return Ok(None);
        };
        let policy = self.memory_policy();
        let budget = policy.self_test_queries_per_cycle as usize;
        if budget == 0 {
            return Ok(None);
        }

        // 1) Sample the next window of Propositions. Projecting the subject and
        // object variables returns the whole Concept — name and type included —
        // so a groundable query can be written without a second lookup per
        // endpoint, which is what the 1.x pass spent most of its round trips on.
        let cursor: SelfTestCursor = self
            .db
            .get_extension_as("memory_self_test_cursor")
            .unwrap_or_default();
        let window = budget * 4;
        let response = self
            .execute_kip_readonly(kip::request_with(
                format!(
                    r#"FIND(?p.id, ?p._system.space_seq, ?s, ?o)
WHERE {{
  ?p (?s, ?predicate, ?o)
  FILTER(?p._system.space_seq > :after)
}}
ORDER BY ?p._system.space_seq
LIMIT {window}"#
                ),
                kip::param("after", cursor.after),
            ))
            .await?;
        if assess::single_read_result(&response).is_err() {
            log::error!(
                target: "brain",
                space_id = self.id;
                "self-test sampling scan failed — dream self-test is NOT running \
                 (graph past the full-scan engine cap?): {}",
                kip::error_message(&response)
            );
            return Ok(None);
        }
        let sampled = self_test_candidates(assess::single_read_result(&response)?);

        // A short window means the cursor has reached the end of the graph.
        // Park it back at zero so coverage cycles — but only once the retest
        // horizon has passed, or a small graph would re-test the same handful
        // of memories on every cycle and burn its whole token budget doing it.
        let reached_end = sampled.len() < window;
        let sampled_after = sampled.last().map(|c| c.seq).unwrap_or(cursor.after);

        // Prefer memories with no usage evidence at all: recalled ones are
        // proven groundable, already-tested ones had their chance.
        let mut candidates = Vec::new();
        for candidate in sampled {
            let usage = self.ledger.get(&candidate.id).await?;
            if usage
                .as_ref()
                .is_none_or(|row| row.recall_count == 0 && row.self_test_count == 0)
            {
                candidates.push(candidate);
            }
            if candidates.len() >= budget {
                break;
            }
        }
        candidates.retain(|candidate| !candidate.subject_name.is_empty());
        if candidates.is_empty() {
            let wrap =
                reached_end && now_ms.saturating_sub(cursor.cycled_at) >= SELF_TEST_RETEST_MS;
            self.db
                .save_extension_from(
                    "memory_self_test_cursor".into(),
                    &SelfTestCursor {
                        after: if wrap { 0 } else { sampled_after },
                        cycled_at: if wrap { now_ms } else { cursor.cycled_at },
                    },
                )
                .await?;
            return Ok(None);
        }

        // 2) One LLM call generates all probe queries. The token budget is
        // enforced *before* the call by shrinking the candidate batch to fit
        // (≈3 chars per token, conservative); the knob bounds real spend
        // instead of warning after the fact.
        let max_prompt_chars = (policy.self_test_token_budget as usize).saturating_mul(3);
        while candidates.len() > 1
            && serde_json::to_string(&candidates)
                .map(|prompt| prompt.len() > max_prompt_chars)
                .unwrap_or(false)
        {
            candidates.pop();
        }
        let output = assess::AssessContext::complete(
            self,
            anda_core::CompletionRequest {
                instructions: SELF_TEST_INSTRUCTIONS.to_string(),
                prompt: serde_json::to_string_pretty(&candidates).unwrap_or_default(),
                effort: Some(anda_core::ModelEffort::Low),
                ..Default::default()
            },
        )
        .await?;
        let queries: SelfTestQueries = assess::parse_json_payload(&output.content)?;
        let mut report = SelfTestReport {
            tested_at: now_ms,
            usage: output.usage,
            ..Default::default()
        };
        let budget_tokens = report
            .usage
            .input_tokens
            .saturating_add(report.usage.output_tokens);
        if budget_tokens > policy.self_test_token_budget {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "memory self-test used {budget_tokens} tokens, over policy budget {}",
                policy.self_test_token_budget
            );
        }

        // 3) Deterministic grounding check: does search surface the memory's
        // subject or object concept for the generated query?
        let mut tested_entities = BTreeSet::new();
        for candidate in &candidates {
            let Some(query) = queries
                .queries
                .iter()
                .find(|query| query.id == candidate.id)
                .map(|query| query.query.trim())
                .filter(|query| !query.is_empty())
            else {
                return Err(
                    "self-test query generation did not cover every selected candidate".into(),
                );
            };
            let response = self
                .execute_kip_readonly(kip::request_with(
                    "SEARCH CONCEPT :query LIMIT 8",
                    kip::param("query", query),
                ))
                .await?;
            let observation = assess::search_observation(&response)?;
            let result = assess::single_read_result(&response)?;
            let mut hit_ids = BTreeSet::new();
            assess::collect_entity_objects(result, &mut |id, _| {
                hit_ids.insert(id.to_string());
            });
            let grounded =
                hit_ids.contains(&candidate.subject) || hit_ids.contains(&candidate.object);
            if !grounded && observation.exhaustive != Some(true) {
                return Err(
                    "self-test grounding is unknown under incomplete search coverage".into(),
                );
            }
            report.tested += 1;
            tested_entities.insert(candidate.id.clone());
            if grounded {
                report.grounded += 1;
                continue;
            }

            // Ungroundable: enqueue a SleepTask for the next cycle, unless one
            // is already pending for this concept.
            if self.has_pending_review_task(&candidate.subject).await? {
                continue;
            }
            match self
                .run_kip_settlement(self_test_task_request(candidate, query, now_ms))
                .await
            {
                Ok(response) if kip::succeeded(&response) => report.reencode_tasks += 1,
                Ok(response) => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "self-test SleepTask creation failed: {}",
                        kip::error_message(&response)
                    );
                }
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "self-test SleepTask creation failed: {err:?}"
                    );
                }
            }
        }

        self.ledger
            .record_self_test(&tested_entities, now_ms)
            .await?;
        self.bump_metrics(|metrics| {
            metrics.self_test_tested += report.tested;
            metrics.self_test_grounded += report.grounded;
            metrics.reencode_tasks += report.reencode_tasks;
        });
        self.db
            .set_extension_from("memory_self_test".to_string(), report.clone());
        // Advance only past the batch actually generated and checked. Prompt
        // budget truncation and failed/unknown probes must not skip candidates.
        let next_after = candidates.last().map(|c| c.seq).unwrap_or(cursor.after);
        let wrap = reached_end
            && next_after == sampled_after
            && now_ms.saturating_sub(cursor.cycled_at) >= SELF_TEST_RETEST_MS;
        self.db
            .save_extension_from(
                "memory_self_test_cursor".into(),
                &SelfTestCursor {
                    after: if wrap { 0 } else { next_after },
                    cycled_at: if wrap { now_ms } else { cursor.cycled_at },
                },
            )
            .await?;
        self.db.flush_metadata(now_ms).await?;
        Ok(Some(report))
    }

    /// True when a pending SleepTask already covers this Concept.
    async fn has_pending_review_task(&self, target: &str) -> Result<bool, BoxError> {
        let response = self
            .execute_kip_readonly(kip::request_with(
                r#"FIND(?task) WHERE {
  ?task CONCEPT {type: "SleepTask"}
  FILTER(?task.attributes.status == "pending")
  ?about CONCEPT {id: :target}
  STRUCTURAL (?task, "about", ?about)
} LIMIT 1"#,
                kip::param("target", target),
            ))
            .await?;
        // An error here means *unknown*, and the caller's fallback is to create
        // one more task than it needed — which the `key` on the upsert then
        // collapses anyway.
        Ok(kip::ok_result(&response).is_some_and(|result| {
            let mut found = false;
            assess::collect_entity_objects(result, &mut |_, _| found = true);
            found
        }))
    }
}

/// A memory self-tested longer ago than this becomes eligible for re-sampling,
/// so re-encoded memories eventually get their grounding re-verified.
const SELF_TEST_RETEST_MS: u64 = 30 * 24 * 3_600 * 1_000;

/// Where the dream self-test's sampling window sits.
///
/// A Space sequence coordinate, not a timestamp: it is the same monotonic
/// counter the engine stamps on every element, so "the memories formed after
/// the ones I last looked at" is exact rather than approximate.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize, serde::Serialize)]
struct SelfTestCursor {
    /// The highest `_system.space_seq` a previous pass already sampled.
    after: u64,
    /// When the cursor last wrapped back to the start of the graph.
    cycled_at: u64,
}

/// One memory sampled for the dream self-test (plan M7); serialized as the
/// query-generation prompt.
#[derive(Debug, serde::Serialize)]
struct SelfTestCandidate {
    id: String,
    #[serde(skip)]
    seq: u64,
    subject: String,
    object: String,
    subject_type: String,
    subject_name: String,
    object_name: String,
}

/// Reads the self-test sampling rows —
/// `FIND(?p.id, ?p._system.space_seq, ?s, ?o)` — into candidates.
///
/// Projecting a bare element variable returns the whole rendered element, so
/// the subject's type and name arrive with the row. An object that is a literal
/// rather than an element contributes its text and no id, which is correct: a
/// literal cannot be what a `SEARCH CONCEPT` surfaces.
fn self_test_candidates(result: &serde_json::Value) -> Vec<SelfTestCandidate> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let columns = row.as_array()?;
            let id = columns.first().and_then(serde_json::Value::as_str)?;
            let seq = columns.get(1).and_then(serde_json::Value::as_u64)?;
            let subject = columns.get(2)?;
            let object = columns.get(3);
            Some(SelfTestCandidate {
                id: id.to_string(),
                seq,
                subject: element_field(subject, "id"),
                object: object.map(|o| element_field(o, "id")).unwrap_or_default(),
                subject_type: assess::local_symbol_name(&element_field(subject, "schema_ref"))
                    .to_string(),
                subject_name: element_field(subject, "name"),
                object_name: object.map(element_label).unwrap_or_default(),
            })
        })
        .collect()
}

/// Reads one string field out of a rendered element, or `""`.
fn element_field(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// How a Proposition endpoint reads in a prompt: a Concept's name, or the
/// literal itself when the endpoint is one.
fn element_label(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(_) => element_field(value, "name"),
        other if other.is_null() => String::new(),
        other => other.to_string(),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct SelfTestQueries {
    #[serde(default)]
    queries: Vec<SelfTestQuery>,
}

#[derive(Debug, serde::Deserialize)]
struct SelfTestQuery {
    id: String,
    query: String,
}

const SELF_TEST_INSTRUCTIONS: &str = r#"You test the searchability of an AI's memory graph. You will receive a JSON array of memories, each a proposition with a subject, predicate, and object.

For each memory, write ONE short natural-language query a real user would plausibly ask that this memory should answer. Use the everyday words of the subject/object names — never internal ids, never the predicate name verbatim unless a user would say it.

Respond with ONLY a JSON object:
{"queries": [{"id": "<memory id>", "query": "..."}]}"#;

/// Self-test write: enqueue a SleepTask for a memory that search could not
/// surface, pointed at its subject Concept — re-encoding (aliases, a richer
/// description, links to neighbouring memory) happens at the Concept level.
///
/// The task is keyed by the Concept it is about, so a memory that stays
/// ungroundable across several passes accumulates one task rather than one per
/// pass, and `UPSERT` refreshes the existing one instead of colliding with it.
/// `assigned_to` is deliberately absent: KIP 1.x assigned these to a `$system`
/// Person, and the Profile is explicit that semantic assignment — to `$system`
/// least of all — grants no Principal any permission.
fn self_test_task_request(candidate: &SelfTestCandidate, query: &str, now_ms: u64) -> Request {
    let summary = format!(
        "memory self-test: the query {query:?} did not surface `{}` ({}) via search; re-encode the \
         Concept with aliases, a richer description, or links to neighbouring memory so it becomes \
         findable",
        candidate.subject_name, candidate.id
    );
    let parameters = serde_json::Map::from_iter([
        (
            "key".to_string(),
            serde_json::Value::from(format!("self_test:{}", candidate.subject)),
        ),
        (
            "name".to_string(),
            serde_json::Value::from(format!("Re-encode {}", candidate.subject_name)),
        ),
        ("summary".to_string(), serde_json::Value::from(summary)),
        (
            "created_at".to_string(),
            serde_json::Value::from(kip::timestamp(now_ms)),
        ),
        (
            "target".to_string(),
            serde_json::Value::from(candidate.subject.clone()),
        ),
    ]);
    kip::request_with(
        r#"UPSERT CONCEPT ?task {
  MATCH { type: "SleepTask", key: :key }
  SET FIELDS { name: :name }
  SET ATTRIBUTES {
    task_class: "consolidate",
    summary: :summary,
    status: "pending",
    priority: 2,
    created_at: :created_at
  }
  SET STRUCTURAL { ("about", :target) }
}"#,
        parameters,
    )
}

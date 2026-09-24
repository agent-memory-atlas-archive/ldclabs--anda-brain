//! Optional document-to-graph bridge. Extraction proposes attributed claims;
//! explicit per-claim reviews, not selection omissions, authorize withdrawal.
//! The document generation fences every native publication. Pending work lives
//! with the document; the latest protected digest event retains claim ownership
//! and versioned citations for subsequent reconciliation.

use anda_core::{BoxError, CompletionFeatures, CompletionRequest, Usage};
use anda_db::{
    query::{Filter, Fv, Query, RangeQuery},
    schema::Json,
};
use anda_engine::{context::AgentCtx, memory::MemoryManagement, model::Models};
use anda_kip::Request;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha3::{Digest, Sha3_256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    EVAL_NAMESPACE, EVENT_DIGEST_EXTRACTED, EVENT_DIGEST_FAILED, WikiChunkRecord, WikiDocRecord,
    WikiError, WikiService, WikiVerifyInput, WikiVerifyStatus, WikiVersionRecord,
    chunk::chunk_checksum, citation_uri,
};
// A digest claim is the brain's reading of a document, so it is attributed to
// the brain's semantic self — cognition, which grants the brain nothing it did
// not already have from Governance. The Space designates the same Concept as
// its §5.6 `$self`.
use crate::space::SELF_ACTOR_KEY;
use crate::{kip, vocabulary::MemoryVocabulary};

/// Extractor fingerprint prefix written into proposition metadata; bump on
/// prompt or renderer changes so maintenance can bulk-invalidate old
/// extractions. The full fingerprint appends the model id (PRD §7.3):
/// `wiki_digest@v1/<model_id>`.
pub const WIKI_DIGEST_EXTRACTOR: &str = "wiki_digest@v2";
const DIGEST_PROMPT: &str = include_str!("../../assets/BrainWikiDigest.md");
/// Collection-extension key holding the digest high-water mark (version id).
const DIGEST_CURSOR_KEY: &str = "wiki_digested";
/// Last document id attempted by the fair pending-work scan.
const DIGEST_DOC_CURSOR_KEY: &str = "wiki_digest_doc_cursor";
const DIGEST_USAGE_KEY: &str = "wiki_digest_usage";
const MAX_FACTS_PER_VERSION: usize = 64;
const MAX_EXTRA_CONCEPTS: usize = 64;
const MAX_BATCH_BYTES: usize = 24 * 1024;
const MAX_VERSIONS_PER_RUN: usize = 20;
const MAX_IDENT_CHARS: usize = 120;
/// How many recent digests the post-run citation sample re-verifies.
const VERIFY_SAMPLE_EVENTS: usize = 5;

/// Resets the running flag on drop so a panicking digest never wedges.
struct RunningGuard(Arc<AtomicU64>);
impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct WikiDigest {
    wiki: Arc<WikiService>,
    memory: Arc<MemoryManagement>,
    /// For the extractor fingerprint: `wiki_digest@v1/<model_id>`.
    models: Arc<Models>,
    /// 0 = idle; otherwise the version id currently being digested.
    running: Arc<AtomicU64>,
}

enum DigestOutcome {
    Digested,
    Skipped,
    /// The document changed during extraction. Its current generation stays queued.
    Changed,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WikiDigestReport {
    /// Document generations reconciled with extracted or reused claims.
    pub digested: usize,
    /// Claims retained in the processed document ledgers (not exhaustive facts).
    pub facts: usize,
    /// Document-owned Assertions retracted after review or source withdrawal.
    pub superseded: usize,
    /// Archived, labeled or evaluation documents reconciled without extraction.
    pub skipped: usize,
    /// Documents that failed and remain queued for a later run.
    pub failed: usize,
    /// Post-run citation sample: how many were checked / found corrupt.
    pub citations_checked: usize,
    pub citations_invalid: usize,
    pub usage: Usage,
}

/// LLM output schema (see assets/BrainWikiDigest.md).
#[derive(Debug, Clone, Default, Deserialize)]
struct Extraction {
    #[serde(default)]
    concepts: Vec<ExtractedConcept>,
    facts: Vec<ExtractedFact>,
    #[serde(default)]
    reviews: Vec<FactReview>,
}

#[derive(Debug, Clone, Deserialize)]
struct FactReview {
    index: usize,
    verdict: ReviewVerdict,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReviewVerdict {
    Supported,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractedConcept {
    r#type: String,
    name: String,
    #[serde(default)]
    attributes: serde_json::Map<String, Json>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConceptRef {
    r#type: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractedFact {
    subject: ConceptRef,
    predicate: String,
    object: ConceptRef,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    anchor: Option<String>,
}

/// A validated fact with its resolved citation, as persisted in the
/// `DigestExtracted` event (the digest ledger used for superseding and
/// citation sampling).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DigestedFact {
    pub subject_type: String,
    pub subject_name: String,
    pub predicate: String,
    pub object_type: String,
    pub object_name: String,
    pub confidence: f64,
    pub citation: String,
    pub checksum: String,
    /// Previous digest ledger event. A retraction starts a new assertion
    /// generation even when restoring the same immutable document version.
    pub assertion_generation: u64,
}

/// (subject_type, subject_name, predicate, object_type, object_name)
type TripleKey = (String, String, String, String, String);

impl DigestedFact {
    fn triple_key(&self) -> TripleKey {
        (
            self.subject_type.clone(),
            self.subject_name.clone(),
            self.predicate.clone(),
            self.object_type.clone(),
            self.object_name.clone(),
        )
    }
}

impl WikiDigest {
    pub fn new(wiki: Arc<WikiService>, memory: Arc<MemoryManagement>, models: Arc<Models>) -> Self {
        Self {
            wiki,
            memory,
            models,
            running: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn is_processing(&self) -> bool {
        self.running.load(Ordering::SeqCst) != 0
    }

    /// The extractor fingerprint, model id included, so §13's
    /// "bulk-invalidate by fingerprint" can target one model's extractions.
    fn extractor(&self) -> String {
        match self.models.get_model() {
            Some(model) => format!("{WIKI_DIGEST_EXTRACTOR}/{}", model.model_name()),
            None => WIKI_DIGEST_EXTRACTOR.to_string(),
        }
    }

    /// Observability high-water mark only; it does not schedule or attest coverage.
    pub fn cursor(&self) -> u64 {
        self.wiki
            .docs
            .get_extension_as::<u64>(DIGEST_CURSOR_KEY)
            .unwrap_or_default()
    }

    /// Reconcile a bounded set of document generations. The pending bit lives
    /// in the document row, so archive/restore and restart cannot lose work.
    pub async fn run_pending(
        &self,
        ctx: AgentCtx,
        now_ms: u64,
    ) -> Result<WikiDigestReport, BoxError> {
        if self
            .running
            .compare_exchange(0, u64::MAX, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("wiki digest is already running".into());
        }
        let _guard = RunningGuard(self.running.clone());
        let mut report = WikiDigestReport::default();
        let docs = self.pending_documents().await?;
        for doc in docs {
            // Advance on every attempt, including failures and stale
            // generations. Otherwise the lowest failing document ids can
            // occupy the bounded batch forever and starve later work.
            self.save_doc_cursor(doc._id);
            self.running.store(doc.current_version, Ordering::SeqCst);
            match self.digest_document(&ctx, &doc, now_ms, &mut report).await {
                Ok(DigestOutcome::Changed) => continue,
                Ok(_) => {
                    self.save_cursor(self.cursor().max(doc.current_version))
                        .await;
                }
                Err(err) => {
                    report.failed += 1;
                    log::warn!(target: "brain", doc_id = doc._id; "wiki digest failed; document remains queued: {err}");
                    let wiki = self.wiki.clone();
                    let detail = BTreeMap::from([("error".into(), Json::from(err.to_string()))]);
                    let _ = self
                        .wiki
                        .owned(async move {
                            wiki.write_event(
                                EVENT_DIGEST_FAILED,
                                Some(doc._id),
                                Some(doc.current_version),
                                "wiki_digest".into(),
                                detail,
                                now_ms,
                            )
                            .await
                        })
                        .await;
                }
            }
        }
        let (checked, invalid) = self.verify_recent(now_ms).await?;
        report.citations_checked = checked;
        report.citations_invalid = invalid;
        self.save_usage(&report.usage).await;
        Ok(report)
    }

    /// Select one cyclic, ascending page of pending documents. The cursor is
    /// scheduling state only: `digest_pending` remains the durable source of
    /// truth, so failed work is retried after the scan wraps around.
    async fn pending_documents(&self) -> Result<Vec<WikiDocRecord>, WikiError> {
        let cursor = self
            .wiki
            .docs
            .get_extension_as::<u64>(DIGEST_DOC_CURSOR_KEY)
            .unwrap_or_default();
        let mut docs = self
            .pending_documents_in(RangeQuery::Gt(Fv::U64(cursor)), MAX_VERSIONS_PER_RUN)
            .await?;
        let remaining = MAX_VERSIONS_PER_RUN.saturating_sub(docs.len());
        if remaining > 0 {
            docs.extend(
                self.pending_documents_in(RangeQuery::Le(Fv::U64(cursor)), remaining)
                    .await?,
            );
        }
        Ok(docs)
    }

    async fn pending_documents_in(
        &self,
        id_range: RangeQuery<Fv>,
        limit: usize,
    ) -> Result<Vec<WikiDocRecord>, WikiError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self
            .wiki
            .docs
            .search_as(Query {
                search: None,
                filter: Some(Filter::And(vec![
                    Box::new(Filter::Field(("_id".into(), id_range))),
                    Box::new(Filter::Field((
                        "digest_pending".into(),
                        RangeQuery::Eq(Fv::U64(1)),
                    ))),
                    Box::new(Filter::Field((
                        "current_version".into(),
                        RangeQuery::Gt(Fv::U64(0)),
                    ))),
                ])),
                limit: Some(limit),
            })
            .await?)
    }

    fn save_doc_cursor(&self, cursor: u64) {
        self.wiki
            .docs
            .set_extension_from(DIGEST_DOC_CURSOR_KEY.to_string(), cursor);
    }

    /// Caller holds the wiki write lock and has checked the generation.
    async fn finish_generation(&self, doc: &WikiDocRecord) -> Result<(), BoxError> {
        self.wiki
            .docs
            .update(
                doc._id,
                BTreeMap::from([("digest_pending".into(), Fv::U64(0))]),
            )
            .await?;
        Ok(())
    }

    async fn digest_document(
        &self,
        ctx: &AgentCtx,
        doc: &WikiDocRecord,
        now_ms: u64,
        report: &mut WikiDigestReport,
    ) -> Result<DigestOutcome, BoxError> {
        let version = self
            .wiki
            .published_version(doc, doc.current_version)
            .await?;
        if doc.status != super::DOC_STATUS_ACTIVE
            || doc.namespace == EVAL_NAMESPACE
            || !doc.acl_label.is_empty()
        {
            let this = self.clone();
            let doc = doc.clone();
            let effect = self
                .wiki
                .writes
                .run(async move {
                    let _lock = this.wiki.write_lock.lock().await;
                    let current = this.wiki.doc_record(doc._id).await?;
                    if current.generation != doc.generation || current.digest_pending == 0 {
                        return Ok(None);
                    }
                    let count = this.retract_digested(&doc, &version, now_ms).await?;
                    this.finish_generation(&doc).await?;
                    Ok(Some(count))
                })
                .await?;
            return match effect {
                Some(count) => {
                    report.superseded += count;
                    report.skipped += 1;
                    Ok(DigestOutcome::Skipped)
                }
                None => Ok(DigestOutcome::Changed),
            };
        }
        // Extract from immutable source text, never a partially replaced index.
        let chunks = source_chunks(doc, &version)?;
        let version = &version;
        let (generation, previous) = self
            .previous_digest_head(doc._id, version._id.saturating_add(1))
            .await?;
        let unchanged = if generation == 0 {
            false
        } else {
            let head: super::WikiEventRecord = self.wiki.events.get_as(generation).await?;
            let checksum = match head.version_id {
                Some(id) => Some(self.wiki.version_record(id).await?.checksum),
                None => None,
            };
            checksum.as_deref() == Some(&version.checksum)
                && !head
                    .detail
                    .get("retracted")
                    .and_then(Json::as_bool)
                    .unwrap_or(false)
        };
        let (extraction, absent) = if unchanged {
            (Extraction::default(), BTreeSet::new())
        } else {
            self.extract(ctx, doc, version, &chunks, &previous, report)
                .await?
        };
        let extractor = self.extractor();
        let (new_facts, observed) =
            normalize_facts(&self.wiki.space_id, doc, version, &chunks, &extraction);
        // The ledger covers every claim we still own, including claims omitted
        // by a bounded extraction. Positive observations override an inconsistent
        // negative review. Retained claims take priority over newly proposed ones.
        let mut facts: Vec<DigestedFact> = previous
            .iter()
            .enumerate()
            .filter(|(index, fact)| {
                !absent.contains(index) || observed.contains(&fact.triple_key())
            })
            .map(|(_, fact)| fact.clone())
            .collect();
        for mut fact in new_facts {
            if let Some(old) = facts
                .iter_mut()
                .find(|old| old.triple_key() == fact.triple_key())
            {
                fact.assertion_generation = old.assertion_generation;
                *old = fact;
            } else if facts.len() < MAX_FACTS_PER_VERSION {
                fact.assertion_generation = generation;
                facts.push(fact);
            }
        }
        let alive: BTreeSet<_> = facts.iter().map(DigestedFact::triple_key).collect();
        let previously_claimed: BTreeSet<_> =
            previous.iter().map(DigestedFact::triple_key).collect();
        let mut to_assert: Vec<_> = facts
            .iter()
            .filter(|fact| !previously_claimed.contains(&fact.triple_key()))
            .cloned()
            .collect();

        let this = self.clone();
        let doc = doc.clone();
        let version = version.clone();
        let effect = self
            .wiki
            .writes
            .run(async move {
                let _lock = this.wiki.write_lock.lock().await;
                let current = this.wiki.doc_record(doc._id).await?;
                if current.generation != doc.generation || current.digest_pending == 0 {
                    return Ok(None);
                }
                // A cancelled waiter may leave an owned publication finishing.
                // Do not overwrite a newer ledger with a plan based on its predecessor.
                if this
                    .previous_digest_head(doc._id, version._id.saturating_add(1))
                    .await?
                    .0
                    != generation
                {
                    return Ok(None);
                }

                let mut proposition_ids: Vec<String> = Vec::new();
                if !to_assert.is_empty() {
                    // The Space's Schema Environment has to declare every symbol this
                    // extraction uses before the write can name one. Facts needing a
                    // symbol the vocabulary refused (it is at its cap) are dropped
                    // here rather than failing the whole document.
                    let known = this.ensure_vocabulary(&to_assert).await?;
                    to_assert.retain(|fact| {
                        known.covers(
                            [fact.subject_type.as_str(), fact.object_type.as_str()],
                            [fact.predicate.as_str()],
                        )
                    });
                    let unknown: BTreeSet<TripleKey> =
                        to_assert.iter().map(DigestedFact::triple_key).collect();
                    facts.retain(|fact| {
                        previously_claimed.contains(&fact.triple_key())
                            || unknown.contains(&fact.triple_key())
                    });
                }
                if !to_assert.is_empty() {
                    let request =
                        digest_request(&doc, &version, &extraction, &to_assert, &extractor, now_ms);
                    let response =
                        anda_kip::execute_request(this.memory.nexus().as_ref(), &request).await;
                    if !kip::succeeded(&response) {
                        return Err(format!(
                            "wiki digest write failed: {}",
                            kip::error_message(&response)
                        )
                        .into());
                    }
                    proposition_ids = kip::ok_result(&response)
                        .and_then(|result| result.get("handles"))
                        .and_then(Json::as_object)
                        .map(|handles| {
                            handles
                                .iter()
                                .filter(|(handle, _)| handle.starts_with("p"))
                                .filter_map(|(_, id)| id.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                }

                let superseded = this.retract_facts(doc._id, &previous, &alive).await?;

                this.wiki
                    .write_event(
                        EVENT_DIGEST_EXTRACTED,
                        Some(doc._id),
                        Some(version._id),
                        "wiki_digest".to_string(),
                        BTreeMap::from([
                            (
                                "facts".to_string(),
                                serde_json::to_value(&facts).unwrap_or(Json::Null),
                            ),
                            (
                                "proposition_ids".to_string(),
                                Json::from(proposition_ids.clone()),
                            ),
                            ("superseded".to_string(), Json::from(superseded as u64)),
                            ("extractor".to_string(), Json::from(extractor)),
                        ]),
                        now_ms,
                    )
                    .await?;

                this.finish_generation(&doc).await?;
                Ok(Some((facts.len(), superseded)))
            })
            .await?;
        match effect {
            Some((facts, superseded)) => {
                report.digested += 1;
                report.facts += facts;
                report.superseded += superseded;
                Ok(DigestOutcome::Digested)
            }
            None => Ok(DigestOutcome::Changed),
        }
    }

    /// Runs the extraction prompt over section batches and merges the
    /// results. One retry per batch on non-JSON replies.
    async fn extract(
        &self,
        ctx: &AgentCtx,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        chunks: &[WikiChunkRecord],
        previous: &[DigestedFact],
        report: &mut WikiDigestReport,
    ) -> Result<(Extraction, BTreeSet<usize>), BoxError> {
        let header = format!(
            "Document: {}\nURI: {}\nNamespace: {}\nTags: {}\n",
            doc.title,
            citation_uri(&self.wiki.space_id, doc._id, version._id, 0, version.size),
            doc.namespace,
            doc.tags.join(", "),
        );

        let mut batches: Vec<String> = Vec::new();
        let mut batch = String::new();
        for chunk in chunks {
            let section = format!(
                "\n[anchor: {}] {}\n{}\n",
                chunk.anchor,
                chunk.heading_path.join(" > "),
                chunk.text,
            );
            if !batch.is_empty() && batch.len() + section.len() > MAX_BATCH_BYTES {
                batches.push(std::mem::take(&mut batch));
            }
            batch.push_str(&section);
        }
        if !batch.is_empty() {
            batches.push(batch);
        }

        let prior: Vec<_> = previous
            .iter()
            .enumerate()
            .map(|(index, fact)| {
                json!({
                    "index": index,
                    "subject": {"type": fact.subject_type, "name": fact.subject_name},
                    "predicate": fact.predicate,
                    "object": {"type": fact.object_type, "name": fact.object_name},
                })
            })
            .collect();
        let review_input = serde_json::to_string(&prior)?;
        let mut absent: BTreeSet<usize> = (0..previous.len()).collect();
        let mut merged = Extraction::default();
        if batches.is_empty() {
            return Err("cannot digest an empty source layout".into());
        }
        for batch in batches {
            let prompt = format!(
                "{header}\nPrevious claims to check against THIS batch: {review_input}\n{batch}"
            );
            let extraction = self.extract_batch(ctx, prompt, report).await?;
            absent.retain(|index| {
                let mut reviews = extraction
                    .reviews
                    .iter()
                    .filter(|review| review.index == *index);
                reviews
                    .next()
                    .is_some_and(|review| review.verdict == ReviewVerdict::Absent)
                    && reviews.next().is_none()
            });
            merged.concepts.extend(extraction.concepts);
            merged.facts.extend(extraction.facts);
        }
        Ok((merged, absent))
    }

    async fn extract_batch(
        &self,
        ctx: &AgentCtx,
        prompt: String,
        report: &mut WikiDigestReport,
    ) -> Result<Extraction, BoxError> {
        let mut attempt_prompt = prompt.clone();
        for attempt in 0..2 {
            let res = ctx
                .completion(
                    CompletionRequest {
                        instructions: DIGEST_PROMPT.to_string(),
                        prompt: attempt_prompt.clone(),
                        ..Default::default()
                    },
                    Vec::new(),
                )
                .await?;
            report.usage.accumulate(&res.usage);
            if let Some(reason) = res.failed_reason {
                return Err(format!("digest completion failed: {reason}").into());
            }
            match parse_extraction(&res.content) {
                Ok(extraction) => return Ok(extraction),
                Err(err) if attempt == 0 => {
                    attempt_prompt = format!(
                        "{prompt}\n\nYour previous reply was not the required JSON object ({err}). Reply with ONLY the JSON object."
                    );
                }
                Err(err) => {
                    return Err(format!("digest extraction returned invalid JSON: {err}").into());
                }
            }
        }
        unreachable!("extract_batch loops at most twice")
    }

    /// Withdraw every claim owned by this document. An empty retracted
    /// ledger starts a new assertion generation if the source is restored.
    /// Repeated withdrawal of an already empty ledger is a no-op.
    async fn retract_digested(
        &self,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        now_ms: u64,
    ) -> Result<usize, BoxError> {
        // `+ 1`: unlike superseding during a digest, retraction must cover
        // the given version's own digest — an archived document's facts are
        // recorded at its still-current version.
        let previous = self
            .previous_digest_facts(doc._id, version._id.saturating_add(1))
            .await?;
        if previous.is_empty() {
            return Ok(0);
        }
        let superseded = self
            .retract_facts(doc._id, &previous, &BTreeSet::new())
            .await?;
        self.wiki
            .write_event(
                EVENT_DIGEST_EXTRACTED,
                Some(doc._id),
                Some(version._id),
                "wiki_digest".to_string(),
                BTreeMap::from([
                    ("facts".to_string(), json!([])),
                    ("superseded".to_string(), Json::from(superseded as u64)),
                    ("retracted".to_string(), Json::from(true)),
                    ("extractor".to_string(), Json::from(self.extractor())),
                ]),
                now_ms,
            )
            .await?;
        log::info!(
            target: "brain",
            doc_id = doc._id,
            version_id = version._id;
            "wiki digest retracted {superseded} facts (document no longer distillable)"
        );
        Ok(superseded)
    }

    /// Withdraws the digest's own claim about each fact not in `alive`.
    ///
    /// KIP 1.x flagged the Proposition `superseded`. That said the wrong thing
    /// twice: a Proposition is truth-neutral and carries no stance to withdraw,
    /// and the flag spoke for every actor rather than for the digest. What
    /// actually happened is that this document stopped saying it — so the
    /// digest retracts its own Assertion, leaving the Proposition and anyone
    /// else's Assertions about it untouched. A retraction that finds nothing is
    /// a no-op, so a Concept maintenance merged or archived in the meantime
    /// costs one harmless statement instead of resurrecting anything.
    ///
    /// Missing endpoints are harmless; scan/write failures retain the ledger
    /// head so a retry finishes withdrawing the remaining owned claims.
    async fn retract_facts(
        &self,
        doc_id: u64,
        facts: &[DigestedFact],
        alive: &BTreeSet<TripleKey>,
    ) -> Result<usize, BoxError> {
        let mut retracted = 0usize;
        for fact in facts {
            if alive.contains(&fact.triple_key()) {
                continue;
            }
            let mut after = String::new();
            let mut owned = Vec::new();
            let mut complete = false;
            // client_key is host bookkeeping, deliberately absent from the
            // model-facing Core view. Resolve authorized candidates through KQL,
            // then check their ownership in the host before guarded transitions.
            for _ in 0..64 {
                let response = anda_kip::execute_request(
                    self.memory.nexus().as_ref(),
                    &claim_candidates_request(fact, &after),
                )
                .await;
                if !kip::succeeded(&response) {
                    return Err(format!(
                        "reading document {doc_id} claims failed: {}",
                        kip::error_message(&response)
                    )
                    .into());
                }
                let rows: Vec<(String, u64)> = serde_json::from_value(
                    kip::ok_result(&response)
                        .cloned()
                        .ok_or("missing claim candidates")?,
                )?;
                for (id, version) in &rows {
                    let row = self.memory.nexus().store.get_element(id.parse()?).await?;
                    if let anda_cognitive_nexus::store::Element::Assertion(row) = row
                        && owns_claim(doc_id, fact, &row)
                    {
                        owned.push((id.clone(), *version));
                    }
                }
                if rows.len() < 128 {
                    complete = true;
                    break;
                }
                after = rows.last().unwrap().0.clone();
            }
            if !complete {
                return Err(
                    "document claim scan reached its budget; digest ledger retained".into(),
                );
            }
            for (id, version) in owned {
                let response = anda_kip::execute_request(
                    self.memory.nexus().as_ref(),
                    &retract_request(&id, version),
                )
                .await;
                if !kip::succeeded(&response) {
                    return Err(format!(
                        "retracting document {doc_id} claim {id} failed: {}",
                        kip::error_message(&response)
                    )
                    .into());
                }
                retracted +=
                    usize::try_from(kip::transitioned(&response, "retracted")).unwrap_or(0);
            }
        }
        Ok(retracted)
    }

    /// Drafts the symbols these facts use that the Space does not yet speak,
    /// and returns the vocabulary as it now stands.
    ///
    /// New vocabulary is draft vocabulary (Spec §20.16): one `DEFINE` per new
    /// symbol, each queued for review. Nothing is defined when the Space
    /// already covers every symbol, and a name refused (malformed, or past the
    /// cap) costs only the facts that needed it.
    async fn ensure_vocabulary(
        &self,
        facts: &[DigestedFact],
    ) -> Result<MemoryVocabulary, BoxError> {
        let nexus = self.memory.nexus();
        let vocabulary = MemoryVocabulary::load(nexus.as_ref()).await?;
        let types: BTreeSet<&str> = facts
            .iter()
            .flat_map(|fact| [fact.subject_type.as_str(), fact.object_type.as_str()])
            .collect();
        let predicates: BTreeSet<&str> = facts.iter().map(|fact| fact.predicate.as_str()).collect();
        if vocabulary.covers(types.iter().copied(), predicates.iter().copied()) {
            return Ok(vocabulary);
        }

        let types: Vec<&str> = types.into_iter().collect();
        let predicates: Vec<&str> = predicates.into_iter().collect();
        let drafted = crate::vocabulary::draft_symbols(nexus.as_ref(), &types, &predicates).await?;
        if !drafted.rejected.is_empty() {
            log::warn!(
                target: "brain",
                space_id = self.wiki.space_id;
                "the wiki digest proposed symbols this Space will not draft: {:?}",
                drafted.rejected
            );
        }
        MemoryVocabulary::load(nexus.as_ref()).await
    }

    /// Facts recorded by the most recent digest of this document before the
    /// given version.
    async fn previous_digest_facts(
        &self,
        doc_id: u64,
        before_version: u64,
    ) -> Result<Vec<DigestedFact>, BoxError> {
        Ok(self.previous_digest_head(doc_id, before_version).await?.1)
    }

    async fn previous_digest_head(
        &self,
        doc_id: u64,
        before_version: u64,
    ) -> Result<(u64, Vec<DigestedFact>), BoxError> {
        let events = self
            .wiki
            .list_events(
                Some(EVENT_DIGEST_EXTRACTED.to_string()),
                Some(doc_id),
                None,
                Some(20),
            )
            .await?;
        let latest = events
            .events
            .iter()
            .filter(|e| e.version_id.is_some_and(|v| v < before_version))
            .max_by_key(|e| e.id);
        let Some(event) = latest else {
            return Ok((0, Vec::new()));
        };
        let facts = match event
            .detail
            .get("facts")
            .cloned()
            .map(serde_json::from_value::<Vec<DigestedFact>>)
        {
            Some(Ok(facts)) => facts,
            Some(Err(err)) => {
                return Err(format!(
                    "document {doc_id} digest ledger {} is unreadable: {err}",
                    event.id
                )
                .into());
            }
            None => {
                return Err(
                    format!("document {doc_id} digest ledger {} has no facts", event.id).into(),
                );
            }
        };
        Ok((event.id, facts))
    }

    /// Re-verifies every citation recorded by recent digests; `Invalid`
    /// results are corruption signals (evented inside `verify` when the
    /// stored content itself is corrupt).
    pub async fn verify_recent(&self, now_ms: u64) -> Result<(usize, usize), BoxError> {
        verify_recent_citations(&self.wiki, now_ms).await
    }

    async fn save_cursor(&self, cursor: u64) {
        self.wiki
            .docs
            .set_extension_from(DIGEST_CURSOR_KEY.to_string(), cursor);
    }

    async fn save_usage(&self, usage: &Usage) {
        if usage.requests == 0 {
            return;
        }
        // In-memory metadata update, flushed with the collection; the return
        // value is the previous entry (None on first write), not an error.
        let _ =
            self.wiki
                .docs
                .set_extension_from_with::<_, Usage>(DIGEST_USAGE_KEY.to_string(), |v| {
                    let mut total: Usage = v.unwrap_or_default();
                    total.accumulate(usage);
                    Some(total)
                });
    }
}

/// Rebuild a deterministic extraction input from the immutable version.
fn source_chunks(
    doc: &WikiDocRecord,
    version: &WikiVersionRecord,
) -> Result<Vec<WikiChunkRecord>, WikiError> {
    let prepared = super::prepare_commit(super::WikiCommitInput {
        title: doc.title.clone(),
        content: version.content.clone(),
        ..Default::default()
    })?;
    Ok(prepared
        .chunks
        .into_iter()
        .map(|mut row| {
            row.doc_id = doc._id;
            row.version_id = version._id;
            row.anchor = format!("chunk-{}", row.ordinal);
            row
        })
        .collect())
}

/// Whether the digest ledger already covers (doc, version): true when the
/// document's newest `DigestExtracted` event points at that version. A
/// retraction marker does NOT count — its facts were withdrawn, so a
/// restored document must be re-digested for them to re-enter the graph.
#[cfg(test)]
async fn version_digested(
    wiki: &WikiService,
    doc_id: u64,
    version_id: u64,
) -> Result<bool, BoxError> {
    let events = wiki
        .list_events(
            Some(EVENT_DIGEST_EXTRACTED.to_string()),
            Some(doc_id),
            None,
            Some(20),
        )
        .await?;
    Ok(events.events.iter().max_by_key(|e| e.id).is_some_and(|e| {
        e.version_id == Some(version_id)
            && !e
                .detail
                .get("retracted")
                .and_then(Json::as_bool)
                .unwrap_or(false)
    }))
}

/// Re-verifies the citations recorded by the most recent digests. All facts
/// of one digest cite the same (doc, version), so both rows are loaded once
/// per event and each fact only re-checks its own range and checksum.
async fn verify_recent_citations(
    wiki: &WikiService,
    now_ms: u64,
) -> Result<(usize, usize), BoxError> {
    let events = wiki
        .list_events(
            Some(EVENT_DIGEST_EXTRACTED.to_string()),
            None,
            None,
            Some(VERIFY_SAMPLE_EVENTS),
        )
        .await?;
    let mut checked = 0usize;
    let mut invalid = 0usize;
    let mut loaded = BTreeMap::new();
    for event in events.events {
        let Some(facts) = event
            .detail
            .get("facts")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<DigestedFact>>(value).ok())
        else {
            continue;
        };
        for fact in facts {
            let (doc_id, version_id, start, end) = wiki.verify_target(&WikiVerifyInput {
                uri: Some(fact.citation.clone()),
                ..Default::default()
            })?;
            let key = (doc_id, version_id);
            if let std::collections::btree_map::Entry::Vacant(entry) = loaded.entry(key) {
                let pair = match wiki.doc_record(doc_id).await {
                    Ok(doc) => match wiki.published_version(&doc, version_id).await {
                        Ok(version) => Some((doc, version)),
                        Err(WikiError::NotFound(_)) => None,
                        Err(err) => return Err(err.into()),
                    },
                    Err(WikiError::NotFound(_)) => None,
                    Err(err) => return Err(err.into()),
                };
                entry.insert(pair);
            }
            let status = match loaded[&key].as_ref() {
                Some((doc, version)) => {
                    wiki.verify_resolved(
                        "wiki_digest".into(),
                        doc,
                        version,
                        (start, end),
                        Some(&fact.checksum),
                        now_ms,
                    )
                    .await?
                    .status
                }
                None => WikiVerifyStatus::NotFound,
            };
            checked += 1;
            if matches!(
                status,
                WikiVerifyStatus::Invalid | WikiVerifyStatus::NotFound
            ) {
                invalid += 1;
            }
        }
    }
    Ok((checked, invalid))
}

/// Whether a KQL FIND result contains any row.
/// Parses the extraction JSON, tolerating markdown fences and surrounding
/// prose (first `{` to last `}`).
fn parse_extraction(content: &str) -> Result<Extraction, String> {
    let trimmed = content.trim();
    if let Ok(extraction) = serde_json::from_str::<Extraction>(trimmed) {
        return Ok(extraction);
    }
    let start = trimmed.find('{').ok_or("no JSON object found")?;
    let end = trimmed.rfind('}').ok_or("no JSON object found")?;
    if start >= end {
        return Err("no JSON object found".to_string());
    }
    serde_json::from_str::<Extraction>(&trimmed[start..=end]).map_err(|err| err.to_string())
}

fn clean_ident(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_IDENT_CHARS
        || value.starts_with('$')
        || value.starts_with('_')
    {
        return None;
    }
    Some(value.to_string())
}

/// Cleans an extracted concept-type name and normalizes it to UpperCamelCase.
///
/// Extraction models emit `"drug"`, `"medical device"` and `"works_at"`-shaped
/// variants of one type. In KIP 1.x those became three graph nodes somebody
/// could merge later; in 2.0 each would be a symbol published into this Space's
/// schema package, and a published symbol cannot be tidied away — so the
/// normalization has to happen here, before the name reaches the vocabulary.
///
/// Bounded by the vocabulary's own limit rather than a second one of its own: a
/// name this accepts and the vocabulary then refuses is a fact dropped between
/// two caps that disagree.
fn clean_type_ident(value: &str) -> Option<String> {
    let value = clean_ident(value)?;
    if value.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && value.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some(value);
    }
    let mut out = String::with_capacity(value.len());
    for word in value.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    (out.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && out.chars().count() <= crate::vocabulary::MAX_SYMBOL_CHARS)
        .then_some(out)
}

/// Cleans an extracted predicate name and normalizes it to the snake_case
/// KIP requires (KIP §2.8.2): camelCase boundaries become underscores and
/// non-alphanumeric runs collapse to a single `_`.
fn clean_predicate_ident(value: &str) -> Option<String> {
    let value = clean_ident(value)?;
    if value.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Some(value);
    }
    let mut out = String::with_capacity(value.len() + 4);
    let mut prev_lower_or_digit = false;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && prev_lower_or_digit {
                out.push('_');
            }
            out.extend(c.to_lowercase());
            prev_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_lower_or_digit = false;
        }
    }
    let out = out.trim_matches('_').to_string();
    (out.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && out.chars().count() <= crate::vocabulary::MAX_SYMBOL_CHARS)
        .then_some(out)
}

/// Validates extracted facts and resolves each anchor to its chunk's byte
/// range and checksum; facts with unknown anchors cite the whole version.
/// Returns the persisted facts (capped at [`MAX_FACTS_PER_VERSION`]) plus
/// the alive-set of ALL valid triples: superseding compares against the
/// uncapped set, so truncation never marks a still-asserted fact stale.
fn normalize_facts(
    space_id: &str,
    doc: &WikiDocRecord,
    version: &WikiVersionRecord,
    chunks: &[WikiChunkRecord],
    extraction: &Extraction,
) -> (Vec<DigestedFact>, BTreeSet<TripleKey>) {
    let by_anchor: BTreeMap<&str, &WikiChunkRecord> = chunks
        .iter()
        .map(|chunk| (chunk.anchor.as_str(), chunk))
        .collect();
    let whole_doc = (
        citation_uri(space_id, doc._id, version._id, 0, version.size),
        chunk_checksum(
            &version.checksum,
            0,
            version.size as usize,
            &version.content,
        ),
    );

    let mut alive = BTreeSet::new();
    let mut facts = Vec::new();
    for fact in &extraction.facts {
        let (Some(s_type), Some(s_name), Some(predicate), Some(o_type), Some(o_name)) = (
            clean_type_ident(&fact.subject.r#type),
            clean_ident(&fact.subject.name),
            clean_predicate_ident(&fact.predicate),
            clean_type_ident(&fact.object.r#type),
            clean_ident(&fact.object.name),
        ) else {
            continue;
        };
        let (citation, checksum) = fact
            .anchor
            .as_deref()
            .and_then(|anchor| by_anchor.get(anchor))
            .map(|chunk| {
                (
                    citation_uri(
                        space_id,
                        doc._id,
                        version._id,
                        chunk.byte_start,
                        chunk.byte_end,
                    ),
                    chunk.checksum.clone(),
                )
            })
            .unwrap_or_else(|| whole_doc.clone());
        let normalized = DigestedFact {
            subject_type: s_type,
            subject_name: s_name,
            predicate,
            object_type: o_type,
            object_name: o_name,
            confidence: fact.confidence.unwrap_or(0.7).clamp(0.0, 1.0),
            citation,
            checksum,
            assertion_generation: 0,
        };
        if alive.insert(normalized.triple_key()) && facts.len() < MAX_FACTS_PER_VERSION {
            facts.push(normalized);
        }
    }
    (facts, alive)
}

/// Whether a name may appear as a bare object key in KIP.
fn is_kip_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Builds one atomic transaction for a digested version.
///
/// Everything the digest learned from one document version commits together or
/// not at all: the Evidence for each cited passage, the endpoint Concepts, and
/// one attributed Assertion per fact. A half-written digest would leave claims
/// whose Evidence never landed, which is the one shape a memory system must
/// never produce.
///
/// Where the 1.x metadata went:
///
/// ```text
/// confidence          → the Assertion's own confidence
/// citation + checksum → an Evidence per cited passage
/// source + extractor  → that Evidence's class and payload
/// author "$self"      → asserted_by, the brain's semantic self
/// status/superseded_by→ Assertion lifecycle (see `retract_facts`)
/// ```
fn digest_request(
    doc: &WikiDocRecord,
    version: &WikiVersionRecord,
    extraction: &Extraction,
    facts: &[DigestedFact],
    extractor: &str,
    now_ms: u64,
) -> Request {
    let mut parameters = serde_json::Map::new();
    let mut lines = vec!["MUTATE {".to_string()];
    parameters.insert(
        "observed_at".to_string(),
        json!(
            anda_engine::rfc3339_datetime(now_ms).unwrap_or_else(anda_engine::rfc3339_datetime_now)
        ),
    );

    // The brain's semantic self, resolved (or created) in the same transaction
    // so a fresh space can be digested into without a bootstrap step.
    lines.push(format!(
        "  UPSERT CONCEPT ?self {{ MATCH {{type: \"Person\", key: \"{SELF_ACTOR_KEY}\"}} SET FIELDS {{name: \"{SELF_ACTOR_KEY}\"}} }}"
    ));

    // One Evidence per cited passage, not per fact: several facts read out of
    // the same section rest on the same observation, and minting one Evidence
    // each would let a Projection count one passage as several corroborations.
    let mut evidence_handles: BTreeMap<String, String> = BTreeMap::new();
    for fact in facts {
        if evidence_handles.contains_key(&fact.citation) {
            continue;
        }
        let index = evidence_handles.len();
        let handle = format!("?e{index}");
        parameters.insert(
            format!("ekey{index}"),
            json!(format!("wiki:{}", fact.citation)),
        );
        parameters.insert(
            format!("epayload{index}"),
            json!({
                "citation": fact.citation,
                "checksum": fact.checksum,
                "extractor": extractor,
                "doc_id": doc._id,
                "version_id": version._id,
                "title": doc.title,
            }),
        );
        lines.push(format!(
            "  CREATE EVIDENCE {handle} {{ CLIENT KEY :ekey{index} SET FIELDS {{ evidence_class: \"document\", payload: :epayload{index}, observed_at: :observed_at }} }}"
        ));
        evidence_handles.insert(fact.citation.clone(), handle);
    }

    // Optional descriptions from the extraction, only for endpoints in use.
    let mut attributes: BTreeMap<(String, String), serde_json::Map<String, Json>> = BTreeMap::new();
    for concept in extraction.concepts.iter().take(MAX_EXTRA_CONCEPTS) {
        let (Some(t), Some(n)) = (
            clean_type_ident(&concept.r#type),
            clean_ident(&concept.name),
        ) else {
            continue;
        };
        if !concept.attributes.is_empty() {
            attributes.insert((t, n), concept.attributes.clone());
        }
    }

    // The endpoint Concepts. `key` is the extracted name: identity is scoped to
    // the type, so a `Drug` and a `Symptom` both named "Migraine" stay two
    // Concepts, exactly as the 1.x `(type, name)` identity had them.
    let mut endpoints: Vec<(String, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for fact in facts {
        for (t, n) in [
            (&fact.subject_type, &fact.subject_name),
            (&fact.object_type, &fact.object_name),
        ] {
            if seen.insert((t.clone(), n.clone())) {
                endpoints.push((t.clone(), n.clone()));
            }
        }
    }
    let mut handles: BTreeMap<(String, String), String> = BTreeMap::new();
    for (index, (t, n)) in endpoints.iter().enumerate() {
        let handle = format!("?c{index}");
        parameters.insert(format!("ct{index}"), json!(t));
        parameters.insert(format!("cn{index}"), json!(n));
        let mut block = format!(
            "  UPSERT CONCEPT {handle} {{ MATCH {{type: :ct{index}, key: :cn{index}}} SET FIELDS {{name: :cn{index}}}"
        );
        if let Some(attrs) = attributes.get(&(t.clone(), n.clone())) {
            // `SET ATTRIBUTES` takes an object literal, so each extracted value
            // is bound to its own parameter rather than the map being passed
            // whole: the keys are syntax and the values are data.
            let assignments: Vec<String> = attrs
                .iter()
                .filter(|(key, _)| is_kip_identifier(key))
                .enumerate()
                .map(|(slot, (key, value))| {
                    parameters.insert(format!("ca{index}_{slot}"), value.clone());
                    format!("{key}: :ca{index}_{slot}")
                })
                .collect();
            if !assignments.is_empty() {
                block.push_str(&format!(" SET ATTRIBUTES {{ {} }}", assignments.join(", ")));
            }
        }
        block.push_str(" }");
        lines.push(block);
        handles.insert((t.clone(), n.clone()), handle);
    }

    // One attributed claim per fact. `mode: "inferred"` is the honest label: the
    // digest read prose and drew a structured conclusion from it — the document
    // did not state a triple. `key` makes a re-digest of the same passage
    // resolve to the same Assertion instead of stacking duplicates.
    for (index, fact) in facts.iter().enumerate() {
        let subject = &handles[&(fact.subject_type.clone(), fact.subject_name.clone())];
        let object = &handles[&(fact.object_type.clone(), fact.object_name.clone())];
        let evidence = &evidence_handles[&fact.citation];
        parameters.insert(format!("pred{index}"), json!(fact.predicate));
        parameters.insert(format!("conf{index}"), json!(fact.confidence));
        parameters.insert(
            format!("fkey{index}"),
            json!(format!(
                "{}{}:{}",
                assertion_key_prefix(doc._id, fact),
                version._id,
                fact.assertion_generation
            )),
        );
        lines.push(format!(
            "  ASSERT ?p{index} ({subject}, :pred{index}, {object}) {{ by: ?self, mode: \"inferred\", confidence: :conf{index}, evidence: {evidence}, key: :fkey{index} }}"
        ));
    }
    lines.push("}".to_string());

    kip::request_with(lines.join("\n"), parameters)
}

/// Withdraws the digest's own Assertion about one fact.
///
/// Scoped to `asserted_by: ?self`: another actor may hold the same belief for
/// their own reasons, and this document going quiet is no reason to speak for
/// them. A `WHERE` that matches nothing retracts nothing.
fn assertion_key_prefix(doc_id: u64, fact: &DigestedFact) -> String {
    let tuple = json!([
        fact.subject_type,
        fact.subject_name,
        fact.predicate,
        fact.object_type,
        fact.object_name
    ]);
    let digest: String = Sha3_256::digest(tuple.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("wiki:{doc_id}:claim:{digest}:")
}

fn owns_claim(
    doc_id: u64,
    fact: &DigestedFact,
    row: &anda_cognitive_nexus::store::rows::AssertionRow,
) -> bool {
    row.client_key
        .starts_with(&assertion_key_prefix(doc_id, fact))
}

fn claim_candidates_request(fact: &DigestedFact, after: &str) -> Request {
    let parameters = serde_json::Map::from_iter([
        ("st".to_string(), json!(fact.subject_type)),
        ("sn".to_string(), json!(fact.subject_name)),
        ("pred".to_string(), json!(fact.predicate)),
        ("ot".to_string(), json!(fact.object_type)),
        ("on".to_string(), json!(fact.object_name)),
        ("self_key".to_string(), json!(SELF_ACTOR_KEY)),
        ("after".to_string(), json!(after)),
    ]);
    kip::request_with(
        r#"FIND(?a.id, ?a._system.version)
WHERE {
  ?s CONCEPT {type: :st, key: :sn}
  ?o CONCEPT {type: :ot, key: :on}
  ?p (?s, :pred, ?o)
  ?self CONCEPT {type: "Person", key: :self_key}
  ?a ASSERTION {proposition: ?p, asserted_by: ?self}
  FILTER(?a.lifecycle.status == "active")
  FILTER(?a.id > :after)
}
ORDER BY ?a.id LIMIT 128"#,
        parameters,
    )
}

fn retract_request(id: &str, version: u64) -> Request {
    kip::request_with(
        "TRANSITION :id TO \"retracted\" EXPECT VERSION :version",
        serde_json::Map::from_iter([("id".into(), json!(id)), ("version".into(), json!(version))]),
    )
}

#[cfg(test)]
mod tests;

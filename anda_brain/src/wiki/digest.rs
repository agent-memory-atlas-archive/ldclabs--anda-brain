//! WikiDigest: distills committed wiki versions into the Cognitive Nexus
//! (PRD §7.3), the graph half of the "graph understands, wiki proves" story.
//!
//! Provenance-by-construction: the LLM only proposes structured facts
//! (subject/predicate/object + section anchor); this module builds the KIP
//! itself, minting one Evidence per cited passage and attributing every claim
//! to the brain's own semantic self with `mode: "inferred"`. A prompt can
//! forget provenance — a builder cannot.
//!
//! Three things KIP 2.0 changed here. Extracted vocabulary can no longer be
//! registered by the write: `$ConceptType` / `$PropositionType` nodes are gone,
//! and the symbols a document introduces enter through a host-published Schema
//! Package instead ([`super::vocabulary`]). A fact is now a truth-neutral
//! Proposition plus the digest's Assertion about it, so the confidence lives on
//! the stance rather than on the link. And when a new version drops a fact, the
//! digest **retracts its own Assertion** rather than flagging the Proposition
//! superseded — the document stopped saying it, which is a withdrawal, not a
//! claim that the world changed. The Proposition and every other actor's
//! Assertion about it survive untouched.
//!
//! Every digest is still recorded as a `DigestExtracted` wiki event whose fact
//! list doubles as the citation sample for verification.

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
    Collection, EVAL_NAMESPACE, EVENT_DIGEST_EXTRACTED, EVENT_DIGEST_FAILED, WikiChunkRecord,
    WikiDocRecord, WikiError, WikiService, WikiVerifyInput, WikiVerifyStatus, WikiVersionRecord,
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
pub const WIKI_DIGEST_EXTRACTOR: &str = "wiki_digest@v1";
const DIGEST_PROMPT: &str = include_str!("../../assets/BrainWikiDigest.md");
/// Collection-extension key holding the digest high-water mark (version id).
const DIGEST_CURSOR_KEY: &str = "wiki_digested";
const DIGEST_USAGE_KEY: &str = "wiki_digest_usage";
/// Collection-extension key tracking consecutive failures of one version
/// (the poison-version fuse). Single slot: both the main loop and the
/// pending pass stop at their first retryable failure, so the same version
/// keeps re-bumping this slot until it succeeds or fuses off.
const DIGEST_FAILURE_KEY: &str = "wiki_digest_failure";
/// Collection-extension key holding doc ids queued for a digest catch-up
/// (documents restored from archive after the cursor passed their version).
pub(super) const DIGEST_PENDING_KEY: &str = "wiki_digest_pending";
/// After this many consecutive failures a version is skipped (with a
/// `DigestFailed` event) instead of wedging the pipeline and re-burning
/// tokens every run.
const MAX_VERSION_FAILURES: u64 = 3;
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

/// Outcome of one version's digest attempt.
enum DigestOutcome {
    Digested,
    /// Permanently not digestible (superseded, archived, labeled, eval
    /// corpus, reclaimed): the cursor advances past it.
    Skipped,
    /// The version row exists but its document has not flipped to it yet
    /// (commit in flight, or a crash leftover awaiting the orphan sweep):
    /// the cursor must NOT advance, or the version would silently never be
    /// digested.
    NotReady,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WikiDigestReport {
    /// Versions digested into the graph this run.
    pub digested: usize,
    /// Propositions written (across all digested versions).
    pub facts: usize,
    /// Propositions from older versions marked superseded.
    pub superseded: usize,
    /// Pending versions skipped (already superseded, archived, eval corpus).
    pub skipped: usize,
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
    #[serde(default)]
    facts: Vec<ExtractedFact>,
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
    #[serde(default)]
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

    /// High-water mark: the largest version id already digested (or skipped).
    pub fn cursor(&self) -> u64 {
        self.wiki
            .docs
            .get_extension_as::<u64>(DIGEST_CURSOR_KEY)
            .unwrap_or_default()
    }

    /// Digests all pending versions (bounded per run), supersedes stale
    /// facts, then re-verifies a citation sample from recent digests.
    /// Single-flight per space; failures stop the run without advancing the
    /// cursor past the failed version, so the next run retries it.
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
        let mut cursor = self.cursor();
        let mut processed = 0usize;

        'run: while processed < MAX_VERSIONS_PER_RUN {
            let versions: Vec<WikiVersionRecord> = self
                .wiki
                .versions
                .search_as(Query {
                    search: None,
                    filter: Some(Filter::Field((
                        "_id".to_string(),
                        RangeQuery::Gt(Fv::U64(cursor)),
                    ))),
                    limit: Some(MAX_VERSIONS_PER_RUN),
                })
                .await
                .map_err(WikiError::from)?;
            if versions.is_empty() {
                break;
            }

            for version in versions {
                if processed >= MAX_VERSIONS_PER_RUN {
                    break 'run;
                }
                processed += 1;
                self.running.store(version._id, Ordering::SeqCst);

                match self
                    .digest_version(&ctx, &version, now_ms, &mut report)
                    .await
                {
                    Ok(DigestOutcome::NotReady) => {
                        // Commit in flight: retry from here next run (the
                        // orphan sweep reclaims it if the commit crashed).
                        break 'run;
                    }
                    Ok(_) => {
                        cursor = version._id;
                        self.save_cursor(cursor).await;
                        self.clear_failure();
                    }
                    Err(err) => {
                        let failures = self.bump_failure(version._id);
                        if failures >= MAX_VERSION_FAILURES {
                            // Poison-version fuse: skip it after repeated
                            // failures so one bad document cannot wedge the
                            // pipeline and re-burn tokens forever.
                            log::error!(
                                target: "brain",
                                version_id = version._id,
                                doc_id = version.doc_id;
                                "wiki digest failed {failures} times, skipping version: {err:?}"
                            );
                            self.fuse_version(
                                version.doc_id,
                                version._id,
                                err.to_string(),
                                failures,
                                now_ms,
                                &mut report,
                            )
                            .await;
                            cursor = version._id;
                            self.save_cursor(cursor).await;
                            continue;
                        }
                        // Leave the cursor before the failed version: the
                        // next run retries it instead of silently skipping.
                        log::error!(
                            target: "brain",
                            version_id = version._id,
                            doc_id = version.doc_id;
                            "wiki digest failed (attempt {failures}/{MAX_VERSION_FAILURES}): {err:?}"
                        );
                        self.save_usage(&report.usage).await;
                        return Err(err);
                    }
                }
            }
        }

        self.digest_pending_restores(&ctx, now_ms, cursor, &mut processed, &mut report)
            .await;

        let (checked, invalid) = self.verify_recent(now_ms).await?;
        report.citations_checked = checked;
        report.citations_invalid = invalid;
        self.save_usage(&report.usage).await;
        Ok(report)
    }

    /// Catch-up pass for documents restored from archive after the cursor
    /// passed their version: the main loop never revisits those ids, so
    /// without this their facts would never enter the graph. Each queued
    /// document's current version is digested unless the ledger shows it
    /// already was. Never fails the run; a retryable error stops this
    /// round's pass (the entry stays queued and is retried first next run,
    /// which keeps the single-slot poison fuse counting consecutive
    /// failures of one version), while `NotFound` unqueues the entry.
    async fn digest_pending_restores(
        &self,
        ctx: &AgentCtx,
        now_ms: u64,
        cursor: u64,
        processed: &mut usize,
        report: &mut WikiDigestReport,
    ) {
        let pending = self
            .wiki
            .docs
            .get_extension_as::<BTreeSet<u64>>(DIGEST_PENDING_KEY)
            .unwrap_or_default();
        for doc_id in pending {
            if *processed >= MAX_VERSIONS_PER_RUN {
                break; // budget spent; the rest stays queued for the next run
            }
            let doc = match self.wiki.doc_record(doc_id).await {
                Ok(doc) => doc,
                Err(WikiError::NotFound(_)) => {
                    // Reclaimed or re-initializing: nothing to catch up.
                    self.clear_pending(doc_id);
                    continue;
                }
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id;
                        "pending digest doc load failed, kept queued: {err:?}"
                    );
                    break;
                }
            };
            if doc.current_version > cursor {
                // The main cursor loop reaches this version by itself; once
                // its DigestExtracted lands, the ledger check below clears
                // the entry on the next run.
                continue;
            }
            match version_digested(&self.wiki, doc_id, doc.current_version).await {
                Ok(true) => {
                    self.clear_pending(doc_id);
                    continue;
                }
                Ok(false) => {}
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id;
                        "pending digest ledger check failed, kept queued: {err:?}"
                    );
                    break;
                }
            }
            let version = match self.wiki.version_record(doc.current_version).await {
                Ok(version) => version,
                Err(WikiError::NotFound(_)) => {
                    // The current version row is gone for good: unqueue.
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id,
                        version_id = doc.current_version;
                        "pending digest version row missing, dropped"
                    );
                    self.clear_pending(doc_id);
                    continue;
                }
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        doc_id = doc_id,
                        version_id = doc.current_version;
                        "pending digest version load failed, kept queued: {err:?}"
                    );
                    break;
                }
            };
            *processed += 1;
            self.running.store(version._id, Ordering::SeqCst);
            match self.digest_version(ctx, &version, now_ms, report).await {
                // Digested, or permanently skipped (re-archived, labeled,
                // eval corpus): either way the queue entry is done. A
                // later restore re-queues re-archived documents.
                Ok(_) => {
                    self.clear_pending(doc_id);
                    self.clear_failure();
                }
                Err(err) => {
                    let failures = self.bump_failure(version._id);
                    log::error!(
                        target: "brain",
                        version_id = version._id,
                        doc_id = doc_id;
                        "pending digest failed (attempt {failures}/{MAX_VERSION_FAILURES}): {err:?}"
                    );
                    if failures >= MAX_VERSION_FAILURES {
                        self.fuse_version(
                            doc_id,
                            version._id,
                            err.to_string(),
                            failures,
                            now_ms,
                            report,
                        )
                        .await;
                        self.clear_pending(doc_id);
                    } else {
                        // Retryable: stop this round so the next run bumps
                        // the same fuse slot instead of interleaving.
                        break;
                    }
                }
            }
        }
    }

    /// Poison-version fuse trip: records the `DigestFailed` event, clears
    /// the failure slot, and counts the skip. Callers decide what retiring
    /// the version means (cursor advance in the main loop, unqueue in the
    /// pending pass).
    async fn fuse_version(
        &self,
        doc_id: u64,
        version_id: u64,
        err_text: String,
        failures: u64,
        now_ms: u64,
        report: &mut WikiDigestReport,
    ) {
        let _ = self
            .wiki
            .write_event(
                EVENT_DIGEST_FAILED,
                Some(doc_id),
                Some(version_id),
                "wiki_digest".to_string(),
                BTreeMap::from([
                    ("error".to_string(), Json::from(err_text)),
                    ("attempts".to_string(), Json::from(failures)),
                    ("extractor".to_string(), Json::from(self.extractor())),
                ]),
                now_ms,
            )
            .await;
        self.clear_failure();
        report.skipped += 1;
    }

    fn clear_pending(&self, doc_id: u64) {
        let _ = self.wiki.docs.set_extension_from_with::<_, BTreeSet<u64>>(
            DIGEST_PENDING_KEY.to_string(),
            |v| {
                let mut set = v.unwrap_or_default();
                set.remove(&doc_id);
                Some(set)
            },
        );
    }

    async fn digest_version(
        &self,
        ctx: &AgentCtx,
        version: &WikiVersionRecord,
        now_ms: u64,
        report: &mut WikiDigestReport,
    ) -> Result<DigestOutcome, BoxError> {
        let doc = match self.wiki.doc_record(version.doc_id).await {
            Ok(doc) => doc,
            // Orphan or reclaimed document: nothing to digest.
            Err(WikiError::NotFound(_)) => {
                report.skipped += 1;
                return Ok(DigestOutcome::Skipped);
            }
            Err(err) => return Err(err.into()),
        };
        if version._id > doc.current_version {
            // Written but not flipped: a concurrent commit is between step 1
            // and its activation point. Advancing past it here would leave
            // the graph stale until the document's next commit.
            return Ok(DigestOutcome::NotReady);
        }
        if doc.current_version != version._id {
            // Stale version: the current version's own digest supersedes
            // for it, so skipping loses nothing.
            report.skipped += 1;
            return Ok(DigestOutcome::Skipped);
        }
        if doc.status != super::DOC_STATUS_ACTIVE
            || doc.namespace == EVAL_NAMESPACE
            // The Cognitive Nexus has no ACL: distilling a labeled document
            // would let any Read principal recall its facts (and citation
            // URIs) through the graph.
            || !doc.acl_label.is_empty()
        {
            // The document reached a state the digest refuses to distill —
            // but an earlier version may already be in the graph, and no
            // other path ever retracts it. Without this, committing an
            // `acl_label` onto a public document would leave its previously
            // digested facts recallable by every Read principal forever.
            report.superseded += self.retract_digested(&doc, version, now_ms).await?;
            report.skipped += 1;
            return Ok(DigestOutcome::Skipped);
        }

        let Some(chunks) = digest_chunks(&self.wiki, version).await? else {
            // The doc pointed at this version a moment ago but its chunks
            // are gone: a concurrent commit superseded it mid-digest. Skip —
            // running an empty extraction here would let `supersede_stale`
            // mark every fact of the previous digest superseded.
            report.skipped += 1;
            return Ok(DigestOutcome::Skipped);
        };
        let extraction = self.extract(ctx, &doc, version, &chunks, report).await?;
        let extractor = self.extractor();
        let (mut facts, alive) =
            normalize_facts(&self.wiki.space_id, &doc, version, &chunks, &extraction);

        // What the previous digest of this document recorded. A fact in both
        // is one this reader already claims: re-asserting it would put a second
        // active Assertion by the same actor on the same Proposition, and two
        // copies of one belief read as corroboration to a Projection. Repetition
        // is not evidence — re-reading the same sentence in a new revision is
        // the same observation, not a second one. `facts` stays the whole
        // current set, because that is what the ledger event means by "what
        // this document says"; only the write is narrowed.
        let (generation, previous) = self
            .previous_digest_head(doc._id, version._id.saturating_add(1))
            .await?;
        for fact in &mut facts {
            fact.assertion_generation = previous
                .iter()
                .find(|old| old.triple_key() == fact.triple_key())
                .map(|old| old.assertion_generation)
                .unwrap_or(generation);
        }
        let previously_claimed: BTreeSet<TripleKey> =
            previous.iter().map(DigestedFact::triple_key).collect();
        let mut to_assert: Vec<DigestedFact> = facts
            .iter()
            .filter(|fact| !previously_claimed.contains(&fact.triple_key()))
            .cloned()
            .collect();

        let mut proposition_ids: Vec<String> = Vec::new();
        if !to_assert.is_empty() {
            // The Space's Schema Environment has to declare every symbol this
            // extraction uses before the write can name one. Facts needing a
            // symbol the vocabulary refused (it is at its cap) are dropped
            // here rather than failing the whole document.
            let known = self.ensure_vocabulary(&to_assert).await?;
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
                digest_request(&doc, version, &extraction, &to_assert, &extractor, now_ms);
            let response = anda_kip::execute_request(self.memory.nexus().as_ref(), &request).await;
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

        let superseded = self.retract_facts(doc._id, &previous, &alive).await?;

        self.wiki
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

        report.digested += 1;
        report.facts += facts.len();
        report.superseded += superseded;
        Ok(DigestOutcome::Digested)
    }

    /// Runs the extraction prompt over section batches and merges the
    /// results. One retry per batch on non-JSON replies.
    async fn extract(
        &self,
        ctx: &AgentCtx,
        doc: &WikiDocRecord,
        version: &WikiVersionRecord,
        chunks: &[WikiChunkRecord],
        report: &mut WikiDigestReport,
    ) -> Result<Extraction, BoxError> {
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

        let mut merged = Extraction::default();
        for batch in batches {
            let prompt = format!("{header}{batch}");
            let extraction = self.extract_batch(ctx, prompt, report).await?;
            merged.concepts.extend(extraction.concepts);
            merged.facts.extend(extraction.facts);
        }
        Ok(merged)
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

    /// Retracts the document's newest digest at or before `version`: every
    /// fact it recorded is marked superseded, and an empty `retracted`
    /// ledger entry becomes the document's digest head so later supersede
    /// passes see a clean slate. `version_digested` treats the marker as
    /// "not digested", which keeps the restore catch-up re-digesting the
    /// version if the document becomes distillable again. No-op — and no
    /// ledger entry — when nothing is recorded (never digested, or the head
    /// is already a retraction), so repeated passes stay cheap and quiet.
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

    /// Publishes the Schema Package covering these facts' symbols, when the
    /// digest has met a new one.
    ///
    /// Schema is protected control state: KML cannot declare a type, so the
    /// host does it. Activation only happens when the vocabulary actually grew
    /// — every activation mints a new Schema Environment version, and walking
    /// that forward on every digest would invalidate clients' preconditions for
    /// no change at all.
    async fn ensure_vocabulary(
        &self,
        facts: &[DigestedFact],
    ) -> Result<MemoryVocabulary, BoxError> {
        let nexus = self.memory.nexus();
        let mut vocabulary = MemoryVocabulary::load(nexus.as_ref()).await?;
        let types: BTreeSet<&str> = facts
            .iter()
            .flat_map(|fact| [fact.subject_type.as_str(), fact.object_type.as_str()])
            .collect();
        let predicates: BTreeSet<&str> = facts.iter().map(|fact| fact.predicate.as_str()).collect();
        if vocabulary.covers(types.iter().copied(), predicates.iter().copied()) {
            return Ok(vocabulary);
        }

        let rejected = vocabulary.extend(types.iter().copied(), predicates.iter().copied());
        if !rejected.is_empty() {
            log::warn!(
                target: "brain",
                space_id = self.wiki.space_id;
                "the wiki digest proposed symbols this Space will not publish: {rejected:?}"
            );
        }
        vocabulary.activate(nexus.as_ref()).await?;
        Ok(vocabulary)
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
        if let Err(err) = self
            .wiki
            .docs
            .save_extension(DIGEST_CURSOR_KEY.to_string(), cursor.into())
            .await
        {
            // Non-fatal: the next run re-digests from the stale cursor
            // (idempotent for the graph, but re-billed), so be loud.
            log::warn!(
                target: "brain",
                cursor = cursor;
                "wiki digest cursor save failed (next run will re-digest): {err:?}"
            );
        }
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

    /// Consecutive-failure counter for the poison fuse; resets whenever a
    /// different version fails or any version succeeds. Single-slot is
    /// sound because both loops stop at their first retryable failure, so
    /// the failing version is always the next one retried.
    fn bump_failure(&self, version_id: u64) -> u64 {
        let mut count = 1u64;
        let _ = self.wiki.docs.set_extension_from_with::<_, (u64, u64)>(
            DIGEST_FAILURE_KEY.to_string(),
            |v| {
                if let Some((prev, prev_count)) = v
                    && prev == version_id
                {
                    count = prev_count + 1;
                }
                Some((version_id, count))
            },
        );
        count
    }

    fn clear_failure(&self) {
        let _ = self
            .wiki
            .docs
            .set_extension_from_with::<_, (u64, u64)>(DIGEST_FAILURE_KEY.to_string(), |_| {
                Some((0, 0))
            });
    }
}

/// Chunk rows for one version, or `None` when they raced away: a concurrent
/// commit can activate a newer version and delete this version's chunks
/// between the caller's doc check and this read. A committed version always
/// has at least one chunk (content is never empty), so an empty set is that
/// race, not a real state — callers must skip WITHOUT extracting or
/// superseding, or the previous digest's facts would all be marked stale.
async fn digest_chunks(
    wiki: &WikiService,
    version: &WikiVersionRecord,
) -> Result<Option<Vec<WikiChunkRecord>>, BoxError> {
    let mut rows: Vec<WikiChunkRecord> = wiki
        .chunks
        .search_as(Query {
            search: None,
            filter: Some(Filter::And(vec![
                Box::new(Filter::Field((
                    "doc_id".to_string(),
                    RangeQuery::Eq(Fv::U64(version.doc_id)),
                ))),
                Box::new(Filter::Field((
                    "version_id".to_string(),
                    RangeQuery::Eq(Fv::U64(version._id)),
                ))),
            ])),
            limit: Some(Collection::MAX_SEARCH_LIMIT),
        })
        .await
        .map_err(WikiError::from)?;
    if rows.is_empty() {
        log::warn!(
            target: "brain",
            doc_id = version.doc_id,
            version_id = version._id;
            "version has no chunk rows; digest skipped without superseding"
        );
        return Ok(None);
    }
    rows.sort_by_key(|row| row.ordinal);
    Ok(Some(rows))
}

/// Whether the digest ledger already covers (doc, version): true when the
/// document's newest `DigestExtracted` event points at that version. A
/// retraction marker does NOT count — its facts were withdrawn, so a
/// restored document must be re-digested for them to re-enter the graph.
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
    let verify_input = |fact: &DigestedFact| WikiVerifyInput {
        uri: Some(fact.citation.clone()),
        checksum: Some(fact.checksum.clone()),
        ..Default::default()
    };
    let mut checked = 0usize;
    let mut invalid = 0usize;
    for event in events.events {
        let Some(facts) = event
            .detail
            .get("facts")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<DigestedFact>>(v).ok())
        else {
            continue;
        };
        let Some(first) = facts.first() else {
            continue;
        };
        let (doc_id, version_id, _, _) = wiki.verify_target(&verify_input(first))?;
        let loaded = match wiki.doc_record(doc_id).await {
            Ok(doc) => match wiki.version_record(version_id).await {
                Ok(version) => Some((doc, version)),
                Err(_) => None,
            },
            Err(_) => None,
        };
        for fact in &facts {
            let (_, _, start, end) = wiki.verify_target(&verify_input(fact))?;
            let status = match &loaded {
                Some((doc, version)) => {
                    wiki.verify_resolved(
                        "wiki_digest".to_string(),
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
            if status == WikiVerifyStatus::Invalid {
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
    if row
        .client_key
        .starts_with(&assertion_key_prefix(doc_id, fact))
        || row.client_key
            == format!(
                "wiki:{}:{}:{}:{}",
                doc_id, fact.subject_name, fact.predicate, fact.object_name
            )
    {
        return true;
    }
    // A v1 wiki claim has the migration's key, with the original ownership
    // annotation preserved verbatim. Do not infer ownership from actor alone.
    row.mode == "imported"
        && row.facets.iter().any(|(name, value)| {
            let properties = &value["record"]["properties"];
            let metadata = properties.get("m").unwrap_or(&properties["metadata"]);
            name.starts_with("kip://legacy/nexus@")
                && name.ends_with("/LegacyRecord")
                && metadata["source"] == "wiki"
                && metadata["doc_id"].as_u64() == Some(doc_id)
        })
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
mod tests {
    use super::*;

    async fn review_seed_wiki_fact(
        wiki: &WikiService,
        digest: &WikiDigest,
        title: &str,
    ) -> (WikiDocRecord, WikiVersionRecord, DigestedFact) {
        let created = wiki
            .commit("a".into(), commit_input(title, "A shared fact.\n"), 1000)
            .await
            .unwrap();
        let doc = wiki.doc_record(created.doc.id).await.unwrap();
        let version = wiki
            .versions
            .get_as::<WikiVersionRecord>(created.version.id)
            .await
            .unwrap();
        let mut item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
        item.citation = citation_uri("test_space", doc._id, version._id, 0, version.size);
        digest
            .ensure_vocabulary(std::slice::from_ref(&item))
            .await
            .unwrap();
        let request = digest_request(
            &doc,
            &version,
            &Extraction::default(),
            std::slice::from_ref(&item),
            "review",
            1500,
        );
        let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
        assert!(
            kip::succeeded(&response),
            "seed failed: {}",
            kip::error_message(&response)
        );
        wiki.write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(doc._id),
            Some(version._id),
            "wiki_digest".into(),
            BTreeMap::from([("facts".into(), json!([item.clone()]))]),
            1500,
        )
        .await
        .unwrap();
        (doc, version, item)
    }

    #[tokio::test]
    async fn review_wiki_retry_keeps_idempotency() {
        let (wiki, digest) = test_digest("review_wiki_retry").await;
        let (doc, version, item) = review_seed_wiki_fact(&wiki, &digest, "retry doc").await;
        let request = digest_request(
            &doc,
            &version,
            &Extraction::default(),
            &[item],
            "review",
            2500,
        );
        let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
        assert!(
            kip::succeeded(&response),
            "retry failed: {}",
            kip::error_message(&response)
        );
    }

    #[tokio::test]
    async fn review_wiki_retraction_is_document_scoped() {
        let (wiki, digest) = test_digest("review_wiki_scope").await;
        let (first, _, item) = review_seed_wiki_fact(&wiki, &digest, "first doc").await;
        review_seed_wiki_fact(&wiki, &digest, "second doc").await;
        assert_eq!(
            digest_claim_status(&digest, &item).await,
            ["active", "active"]
        );
        let count = digest
            .retract_facts(first._id, std::slice::from_ref(&item), &BTreeSet::new())
            .await
            .unwrap();
        let statuses = digest_claim_status(&digest, &item).await;
        println!("one document retracted: count={count}; statuses={statuses:?}");
        assert_eq!(count, 1);
        assert_eq!(
            statuses.iter().filter(|s| s.as_str() == "active").count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|s| s.as_str() == "retracted")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn review_wiki_fact_can_return_in_a_later_version() {
        let (wiki, digest) = test_digest("review_wiki_return").await;
        let (doc, previous, mut item) = review_seed_wiki_fact(&wiki, &digest, "return doc").await;
        digest
            .retract_facts(doc._id, std::slice::from_ref(&item), &BTreeSet::new())
            .await
            .unwrap();
        let mut update = commit_input("return doc", "The shared fact returns.\n");
        update.doc_id = Some(doc._id);
        update.parent_version = Some(previous._id);
        let created = wiki.commit("a".into(), update, 2000).await.unwrap();
        let version = wiki
            .versions
            .get_as::<WikiVersionRecord>(created.version.id)
            .await
            .unwrap();
        item.citation = citation_uri("test_space", doc._id, version._id, 0, version.size);
        let request = digest_request(
            &doc,
            &version,
            &Extraction::default(),
            std::slice::from_ref(&item),
            "review",
            2500,
        );
        let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
        assert!(
            kip::succeeded(&response),
            "returned fact failed: {}",
            kip::error_message(&response)
        );
        assert!(
            digest_claim_status(&digest, &item)
                .await
                .iter()
                .any(|s| s == "active")
        );
    }

    #[tokio::test]
    async fn restoring_the_same_version_starts_a_new_assertion_generation() {
        let (wiki, digest) = test_digest("same_version_restore").await;
        let (doc, version, mut item) = review_seed_wiki_fact(&wiki, &digest, "restored doc").await;
        assert_eq!(
            digest.retract_digested(&doc, &version, 2000).await.unwrap(),
            1
        );
        assert_eq!(digest_claim_status(&digest, &item).await, ["retracted"]);
        let (generation, previous) = digest
            .previous_digest_head(doc._id, version._id + 1)
            .await
            .unwrap();
        assert!(previous.is_empty());
        assert!(generation > 0);
        item.assertion_generation = generation;
        let request = digest_request(
            &doc,
            &version,
            &Extraction::default(),
            std::slice::from_ref(&item),
            "review",
            2500,
        );
        for _ in 0..2 {
            let response =
                anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
            assert!(
                kip::succeeded(&response),
                "{}",
                kip::error_message(&response)
            );
        }
        let statuses = digest_claim_status(&digest, &item).await;
        assert_eq!(
            statuses.iter().filter(|s| s.as_str() == "active").count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|s| s.as_str() == "retracted")
                .count(),
            1
        );
    }

    fn fact(s: (&str, &str), p: &str, o: (&str, &str)) -> DigestedFact {
        DigestedFact {
            subject_type: s.0.to_string(),
            subject_name: s.1.to_string(),
            predicate: p.to_string(),
            object_type: o.0.to_string(),
            object_name: o.1.to_string(),
            confidence: 0.9,
            citation: "wiki://sp/1@2#0-10".to_string(),
            checksum: "sha3-256:x".to_string(),
            assertion_generation: 0,
        }
    }

    #[test]
    fn parse_extraction_tolerates_fences_and_prose() {
        let strict = r#"{"facts": [{"subject": {"type": "A", "name": "a"}, "predicate": "p", "object": {"type": "B", "name": "b"}}]}"#;
        assert_eq!(parse_extraction(strict).unwrap().facts.len(), 1);

        let fenced = format!("Here you go:\n```json\n{strict}\n```\nDone.");
        assert_eq!(parse_extraction(&fenced).unwrap().facts.len(), 1);

        assert!(parse_extraction("no json here").is_err());
    }

    #[test]
    fn clean_ident_rejects_reserved_and_oversized() {
        assert_eq!(clean_ident(" Person "), Some("Person".to_string()));
        assert!(clean_ident("$ConceptType").is_none());
        assert!(clean_ident("_hidden").is_none());
        assert!(clean_ident("").is_none());
        assert!(clean_ident(&"x".repeat(200)).is_none());
    }

    #[test]
    fn clean_type_ident_normalizes_to_upper_camel_case() {
        assert_eq!(clean_type_ident("Drug"), Some("Drug".to_string()));
        assert_eq!(clean_type_ident("drug"), Some("Drug".to_string()));
        assert_eq!(
            clean_type_ident("medical device"),
            Some("MedicalDevice".to_string())
        );
        assert_eq!(
            clean_type_ident("clinical-trial"),
            Some("ClinicalTrial".to_string())
        );
        assert_eq!(clean_type_ident("works_at"), Some("WorksAt".to_string()));
        assert!(clean_type_ident("$ConceptType").is_none());
        assert!(clean_type_ident("3d printer").is_none());
        assert!(clean_type_ident("工作").is_none());
    }

    #[test]
    fn clean_predicate_ident_normalizes_to_snake_case() {
        assert_eq!(
            clean_predicate_ident("works_at"),
            Some("works_at".to_string())
        );
        assert_eq!(
            clean_predicate_ident("worksAt"),
            Some("works_at".to_string())
        );
        assert_eq!(
            clean_predicate_ident("WorksAt"),
            Some("works_at".to_string())
        );
        assert_eq!(
            clean_predicate_ident("works at"),
            Some("works_at".to_string())
        );
        assert_eq!(clean_predicate_ident("treats"), Some("treats".to_string()));
        assert_eq!(
            clean_predicate_ident("has  side-effect"),
            Some("has_side_effect".to_string())
        );
        assert!(clean_predicate_ident("_hidden").is_none());
        assert!(clean_predicate_ident("···").is_none());
    }

    #[test]
    fn render_digest_kml_registers_schema_and_attaches_provenance() {
        let doc = WikiDocRecord {
            _id: 3,
            namespace: "kb".to_string(),
            slug: "policy".to_string(),
            title: "安全政策".to_string(),
            status: super::super::DOC_STATUS_ACTIVE.to_string(),
            current_version: 7,
            current_checksum: "sha3-256:doc".to_string(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: "a".to_string(),
            updated_by: "a".to_string(),
            created_at: 0,
            updated_at: 0,
        };
        let version = WikiVersionRecord {
            _id: 7,
            doc_id: 3,
            parent_version: None,
            checksum: "sha3-256:v".to_string(),
            content: "c".to_string(),
            size: 1,
            author: "a".to_string(),
            message: None,
            created_at: 0,
        };
        let facts = vec![fact(
            ("Organization", "Acme \"quoted\""),
            "publishes",
            ("Policy", "安全政策"),
        )];
        let request = digest_request(
            &doc,
            &version,
            &Extraction::default(),
            &facts,
            "wiki_digest@v1/test-model",
            1_700_000_000_000,
        );
        request.validate().unwrap();
        let command = request.operations[0].command.clone().unwrap();
        let parameters = request.parameters.clone().unwrap();

        // One transaction: Evidence, endpoints and the attributed claim commit
        // together or not at all.
        assert!(command.starts_with("MUTATE {"), "{command}");
        assert!(command.contains("CREATE EVIDENCE ?e0"), "{command}");
        assert!(command.contains("UPSERT CONCEPT ?c0"), "{command}");
        assert!(
            command.contains(r#"ASSERT ?p0 (?c0, :pred0, ?c1) { by: ?self, mode: "inferred""#),
            "{command}"
        );

        // Nothing extracted is spliced into the command text — a name carrying
        // a quote is data, and the parser never sees it as syntax.
        assert!(!command.contains("Acme"), "{command}");
        assert_eq!(parameters["cn0"], json!(r#"Acme "quoted""#));
        assert_eq!(parameters["ct0"], json!("Organization"));
        assert_eq!(parameters["pred0"], json!("publishes"));
        assert_eq!(parameters["conf0"], json!(0.9));

        // Provenance is Evidence, not metadata on the link.
        assert_eq!(
            parameters["epayload0"]["citation"],
            json!("wiki://sp/1@2#0-10")
        );
        assert_eq!(
            parameters["epayload0"]["extractor"],
            json!("wiki_digest@v1/test-model")
        );

        // Dropping a fact withdraws this reader's claim, scoped to `$self`.
        let retract = retract_request("A-1", 1);
        retract.validate().unwrap();
        let command = retract.operations[0].command.clone().unwrap();
        assert!(
            command.starts_with(r#"TRANSITION :id TO "retracted""#),
            "{command}"
        );
        assert!(command.contains("EXPECT VERSION :version"), "{command}");
        let candidates = claim_candidates_request(&facts[0], "");
        assert!(
            candidates.operations[0]
                .command
                .as_ref()
                .unwrap()
                .contains("asserted_by: ?self")
        );
    }

    #[test]
    fn normalize_facts_resolves_anchors_and_dedupes() {
        let doc = WikiDocRecord {
            _id: 1,
            namespace: "kb".to_string(),
            slug: "d".to_string(),
            title: "t".to_string(),
            status: super::super::DOC_STATUS_ACTIVE.to_string(),
            current_version: 2,
            current_checksum: String::new(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: String::new(),
            updated_by: String::new(),
            created_at: 0,
            updated_at: 0,
        };
        let version = WikiVersionRecord {
            _id: 2,
            doc_id: 1,
            parent_version: None,
            checksum: "sha3-256:v".to_string(),
            content: "0123456789".to_string(),
            size: 10,
            author: String::new(),
            message: None,
            created_at: 0,
        };
        let chunk = WikiChunkRecord {
            _id: 5,
            doc_id: 1,
            version_id: 2,
            namespace: "kb".to_string(),
            current: 1,
            title: "t".to_string(),
            heading_path: vec![],
            anchor: "sec-0".to_string(),
            ordinal: 0,
            text: "01234".to_string(),
            byte_start: 0,
            byte_end: 5,
            checksum: "sha3-256:chunk".to_string(),
            chunker_version: 1,
            acl_label: String::new(),
        };
        let extraction = Extraction {
            concepts: vec![],
            facts: vec![
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: "a".into(),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: Some(2.0),
                    anchor: Some("sec-0".into()),
                },
                // Duplicate triple: dropped.
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: "a".into(),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: None,
                },
                // Unknown anchor: cites the whole version.
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: "a".into(),
                    },
                    predicate: "q".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: Some("missing".into()),
                },
                // Reserved type: dropped.
                ExtractedFact {
                    subject: ConceptRef {
                        r#type: "$Evil".into(),
                        name: "x".into(),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: None,
                },
            ],
        };

        let (facts, alive) = normalize_facts("sp", &doc, &version, &[chunk], &extraction);
        assert_eq!(facts.len(), 2);
        assert_eq!(alive.len(), 2);
        assert_eq!(facts[0].confidence, 1.0);
        assert_eq!(facts[0].citation, "wiki://sp/1@2#0-5");
        assert_eq!(facts[0].checksum, "sha3-256:chunk");
        assert_eq!(facts[1].citation, "wiki://sp/1@2#0-10");
    }

    #[test]
    fn alive_set_is_not_capped_by_fact_truncation() {
        let doc = WikiDocRecord {
            _id: 1,
            namespace: "kb".to_string(),
            slug: "d".to_string(),
            title: "t".to_string(),
            status: super::super::DOC_STATUS_ACTIVE.to_string(),
            current_version: 2,
            current_checksum: String::new(),
            tags: vec![],
            acl_label: String::new(),
            source_uri: None,
            metadata: BTreeMap::new(),
            created_by: String::new(),
            updated_by: String::new(),
            created_at: 0,
            updated_at: 0,
        };
        let version = WikiVersionRecord {
            _id: 2,
            doc_id: 1,
            parent_version: None,
            checksum: "sha3-256:v".to_string(),
            content: "x".to_string(),
            size: 1,
            author: String::new(),
            message: None,
            created_at: 0,
        };
        let extraction = Extraction {
            concepts: vec![],
            facts: (0..MAX_FACTS_PER_VERSION + 10)
                .map(|i| ExtractedFact {
                    subject: ConceptRef {
                        r#type: "A".into(),
                        name: format!("a{i}"),
                    },
                    predicate: "p".into(),
                    object: ConceptRef {
                        r#type: "B".into(),
                        name: "b".into(),
                    },
                    confidence: None,
                    anchor: None,
                })
                .collect(),
        };
        let (facts, alive) = normalize_facts("sp", &doc, &version, &[], &extraction);
        // Persisted facts are capped, but the alive set keeps every valid
        // triple so superseding never treats truncated facts as stale.
        assert_eq!(facts.len(), MAX_FACTS_PER_VERSION);
        assert_eq!(alive.len(), MAX_FACTS_PER_VERSION + 10);
        let truncated = &extraction.facts[MAX_FACTS_PER_VERSION + 5];
        let key = (
            "A".to_string(),
            truncated.subject.name.clone(),
            "p".to_string(),
            "B".to_string(),
            "b".to_string(),
        );
        assert!(alive.contains(&key));
    }

    use super::super::tests::{commit_input, test_wiki};

    #[tokio::test]
    async fn digest_chunks_skips_versions_raced_by_commits() {
        let wiki = test_wiki("wiki_digest_race").await;
        let v1 = wiki
            .commit(
                "a".to_string(),
                commit_input("竞态", "# 竞态\n\n第一版内容。\n"),
                1000,
            )
            .await
            .unwrap();
        let v1_record = wiki
            .versions
            .get_as::<WikiVersionRecord>(v1.version.id)
            .await
            .unwrap();

        // A concurrent commit lands v2 and removes v1's chunk set — exactly
        // what a digest can observe between its doc check and chunk read.
        let mut update = commit_input("竞态", "# 竞态\n\n第二版内容。\n");
        update.doc_id = Some(v1.doc.id);
        update.parent_version = Some(v1.version.id);
        let v2 = wiki.commit("a".to_string(), update, 2000).await.unwrap();

        // v1 resolves to "raced away" (None), never to an empty chunk list
        // that would supersede the previous digest's facts wholesale.
        assert!(digest_chunks(&wiki, &v1_record).await.unwrap().is_none());
        let v2_record = wiki
            .versions
            .get_as::<WikiVersionRecord>(v2.version.id)
            .await
            .unwrap();
        let chunks = digest_chunks(&wiki, &v2_record).await.unwrap().unwrap();
        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn restore_queues_doc_for_digest_catchup() {
        let wiki = test_wiki("wiki_digest_restore_pending").await;
        let out = wiki
            .commit(
                "a".to_string(),
                commit_input("归档文档", "# 归档文档\n\n内容。\n"),
                1000,
            )
            .await
            .unwrap();
        wiki.archive("a".to_string(), out.doc.id, 2000)
            .await
            .unwrap();
        wiki.restore("a".to_string(), out.doc.id, 3000)
            .await
            .unwrap();
        let pending = wiki
            .docs
            .get_extension_as::<BTreeSet<u64>>(DIGEST_PENDING_KEY)
            .unwrap_or_default();
        assert!(pending.contains(&out.doc.id));

        // The ledger check driving the catch-up: false until a
        // DigestExtracted event covers the document's current version.
        assert!(
            !version_digested(&wiki, out.doc.id, out.doc.current_version)
                .await
                .unwrap()
        );
        wiki.write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(out.doc.id),
            Some(out.doc.current_version),
            "wiki_digest".to_string(),
            BTreeMap::new(),
            4000,
        )
        .await
        .unwrap();
        assert!(
            version_digested(&wiki, out.doc.id, out.doc.current_version)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn verify_recent_checks_ledger_citations_with_grouped_loads() {
        let wiki = test_wiki("wiki_digest_verify_recent").await;
        let out = wiki
            .commit(
                "a".to_string(),
                commit_input("样本", "# 样本\n\n引用样本内容。\n"),
                1000,
            )
            .await
            .unwrap();
        let version = wiki
            .versions
            .get_as::<WikiVersionRecord>(out.version.id)
            .await
            .unwrap();
        let ok = fact(("A", "a"), "p", ("B", "b"));
        let ok = DigestedFact {
            citation: citation_uri("test_space", out.doc.id, out.version.id, 0, version.size),
            checksum: chunk_checksum(
                &version.checksum,
                0,
                version.size as usize,
                &version.content,
            ),
            ..ok
        };
        let mut stale = ok.clone();
        stale.predicate = "q".into();
        stale.checksum = "sha3-256:wrong".to_string();
        wiki.write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(out.doc.id),
            Some(out.version.id),
            "wiki_digest".to_string(),
            BTreeMap::from([(
                "facts".to_string(),
                serde_json::to_value(vec![ok, stale]).unwrap(),
            )]),
            2000,
        )
        .await
        .unwrap();

        let (checked, invalid) = verify_recent_citations(&wiki, 3000).await.unwrap();
        assert_eq!((checked, invalid), (2, 1));
        // The mismatched recorded checksum sits over intact content: a
        // reference error, not corruption — no audit event.
        let events = wiki
            .list_events(
                Some("CitationVerifyFailed".to_string()),
                None,
                None,
                Some(10),
            )
            .await
            .unwrap();
        assert!(events.events.is_empty());
    }

    use anda_cognitive_nexus::CognitiveNexus;
    use anda_db::{database::AndaDB, database::DBConfig, storage::StorageConfig};
    use object_store::memory::InMemory;

    /// A digest engine over an in-memory space with a live Cognitive Nexus
    /// (no LLM: only the non-extracting paths may run).
    async fn test_digest(name: &str) -> (Arc<WikiService>, WikiDigest) {
        let db = Arc::new(
            AndaDB::create(
                Arc::new(InMemory::new()),
                DBConfig {
                    name: name.to_string(),
                    description: "wiki digest test db".to_string(),
                    storage: StorageConfig::default(),
                    lock: None,
                },
            )
            .await
            .unwrap(),
        );
        let nexus = CognitiveNexus::connect(db.clone()).await.unwrap();
        // A Space that has activated nothing resolves Core alone, and Core
        // declares no Concept types at all.
        nexus
            .install_and_activate(
                &[(
                    "anda_brain",
                    anda_cognitive_nexus::profiles::COGNITIVE_MEMORY,
                )],
                anda_cognitive_nexus::nexus::DEFAULT_SPACE,
            )
            .await
            .unwrap();
        let nexus = Arc::new(nexus);
        let memory = Arc::new(MemoryManagement::connect(db.clone(), nexus).await.unwrap());
        let wiki = Arc::new(
            WikiService::connect("test_space".to_string(), db)
                .await
                .unwrap(),
        );
        let digest = WikiDigest::new(wiki.clone(), memory, Arc::new(Models::default()));
        (wiki, digest)
    }

    /// The lifecycle status of the digest's own Assertion about one fact.
    async fn digest_claim_status(digest: &WikiDigest, fact: &DigestedFact) -> Vec<String> {
        let request = kip::request_with(
            r#"FIND(?a.lifecycle.status) WHERE {
  ?s CONCEPT {type: :st, key: :sn}
  ?o CONCEPT {type: :ot, key: :on}
  ?p (?s, :pred, ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
            serde_json::Map::from_iter([
                ("st".to_string(), json!(fact.subject_type)),
                ("sn".to_string(), json!(fact.subject_name)),
                ("pred".to_string(), json!(fact.predicate)),
                ("ot".to_string(), json!(fact.object_type)),
                ("on".to_string(), json!(fact.object_name)),
            ]),
        );
        let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
        serde_json::from_value(kip::ok_result(&response).cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn labeling_a_document_retracts_its_digested_facts() {
        let (wiki, digest) = test_digest("wiki_digest_retract").await;
        let v1 = wiki
            .commit(
                "a".to_string(),
                commit_input("秘密文档", "# 秘密文档\n\n内容甲。\n"),
                1000,
            )
            .await
            .unwrap();
        let doc = wiki.doc_record(v1.doc.id).await.unwrap();
        let v1_version = wiki
            .versions
            .get_as::<WikiVersionRecord>(v1.version.id)
            .await
            .unwrap();

        // Seed the graph and the ledger as if v1 had been digested.
        let fact = DigestedFact {
            subject_type: "Person".to_string(),
            subject_name: "alice".to_string(),
            predicate: "knows".to_string(),
            object_type: "Topic".to_string(),
            object_name: "secret_topic".to_string(),
            confidence: 0.9,
            citation: citation_uri("test_space", doc._id, v1_version._id, 0, v1_version.size),
            checksum: "sha3-256:x".to_string(),
            assertion_generation: 0,
        };
        // The Space must declare the extracted symbols before a write can name
        // one; that is the host's decision in KIP 2.0, not the write's.
        digest
            .ensure_vocabulary(std::slice::from_ref(&fact))
            .await
            .unwrap();
        let request = digest_request(
            &doc,
            &v1_version,
            &Extraction::default(),
            std::slice::from_ref(&fact),
            "wiki_digest@v1/test",
            1500,
        );
        let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
        assert!(
            kip::succeeded(&response),
            "seed failed: {}",
            kip::error_message(&response)
        );
        wiki.write_event(
            EVENT_DIGEST_EXTRACTED,
            Some(doc._id),
            Some(v1_version._id),
            "wiki_digest".to_string(),
            BTreeMap::from([(
                "facts".to_string(),
                serde_json::to_value(vec![fact.clone()]).unwrap(),
            )]),
            1500,
        )
        .await
        .unwrap();
        assert_eq!(digest_claim_status(&digest, &fact).await, ["active"]);

        // v2 labels the document — the state the digest refuses to distill.
        let mut update = commit_input("秘密文档", "# 秘密文档\n\n内容乙。\n");
        update.doc_id = Some(doc._id);
        update.parent_version = Some(v1_version._id);
        update.acl_label = Some("secret".to_string());
        let v2 = wiki.commit("a".to_string(), update, 2000).await.unwrap();
        let doc = wiki.doc_record(v2.doc.id).await.unwrap();
        assert_eq!(doc.acl_label, "secret");
        let v2_version = wiki
            .versions
            .get_as::<WikiVersionRecord>(v2.version.id)
            .await
            .unwrap();

        let retracted = digest
            .retract_digested(&doc, &v2_version, 3000)
            .await
            .unwrap();
        assert_eq!(retracted, 1);
        // The digest withdrew its own claim; the Proposition survives…
        assert_eq!(digest_claim_status(&digest, &fact).await, ["retracted"]);
        // …the digest head is a clean, retracted slate…
        let head = digest
            .previous_digest_facts(doc._id, v2_version._id + 1)
            .await
            .unwrap();
        assert!(head.is_empty());
        // …and the retraction marker does NOT count as "digested", so a
        // restore/unlabel catch-up would re-digest the version.
        assert!(
            !version_digested(&wiki, doc._id, v2_version._id)
                .await
                .unwrap()
        );

        // Idempotent: a second retraction is a no-op with no ledger growth.
        let events_before = wiki
            .list_events(
                Some(EVENT_DIGEST_EXTRACTED.to_string()),
                Some(doc._id),
                None,
                Some(20),
            )
            .await
            .unwrap()
            .events
            .len();
        assert_eq!(
            digest
                .retract_digested(&doc, &v2_version, 4000)
                .await
                .unwrap(),
            0
        );
        let events_after = wiki
            .list_events(
                Some(EVENT_DIGEST_EXTRACTED.to_string()),
                Some(doc._id),
                None,
                Some(20),
            )
            .await
            .unwrap()
            .events
            .len();
        assert_eq!(events_before, events_after);
    }

    #[tokio::test]
    async fn poison_fuse_counts_consecutive_failures_of_one_version() {
        let (_wiki, digest) = test_digest("wiki_digest_fuse").await;
        assert_eq!(digest.bump_failure(10), 1);
        assert_eq!(digest.bump_failure(10), 2);
        assert_eq!(digest.bump_failure(10), 3);
        assert!(digest.bump_failure(10) >= MAX_VERSION_FAILURES);

        // A different version failing takes over the single slot…
        assert_eq!(digest.bump_failure(20), 1);
        assert_eq!(digest.bump_failure(20), 2);
        // …and any success resets it.
        digest.clear_failure();
        assert_eq!(digest.bump_failure(20), 1);
    }
}

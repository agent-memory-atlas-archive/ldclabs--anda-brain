//! The deterministic settlement: what maintenance settles before the model
//! sees anything.
//!
//! Four passes run on every maintenance cycle, and none of them is cognition.
//! Disuse decay is arithmetic on a clock. A superseded Assertion is a fact of
//! the graph, not a reading of it. A deadline passing is arithmetic. A Skill's
//! promotion is a rule the Cognitive Memory Profile (§14 rule 1) explicitly
//! takes away from the acting model: *the Brain proposes, compiles and
//! narrates; it never promotes*. Putting them in a prompt would make each one
//! a thing the Brain does when it happens to be scheduled, has context budget,
//! and notices.
//!
//! # The port
//!
//! Every pass is the same shape — scan the graph, decide, write back — so the
//! module takes one [`RunKip`] port instead of reaching for a graph: one
//! command in, one result out, with `readonly` picking the gate. [`Space`] is
//! one adapter and this module's own tests are the other, which is what lets
//! the rules be exercised at all. Before the port, the only way to ask "what
//! does the verdict rule do when the family baseline beats the trial?" was to
//! build a graph that produced exactly that outcome stream.
//!
//! The passes decide; they do not own effects beyond the graph. Correction
//! discovery is the clear case: [`scan_corrections`] reads the page and works
//! out the cursor it earned, while recording each correction in the usage
//! ledger and aggregating `source_reliability` stays with the [`Space`] that
//! owns those. What comes back is a decision, not a side effect.
//!
//! [`Space`]: crate::space::Space

pub(crate) mod skill;
pub(crate) mod watch;

use anda_core::BoxError;
use anda_kip::{Request, Response};
use serde_json::Value as Json;
use std::future::Future;

use crate::{
    kip,
    types::{MemoryPolicy, SkillSettlement, WatchSettlement},
};

/// Per-command row limit for bulk settlement passes.
pub(crate) const SETTLEMENT_BATCH_LIMIT: usize = 500;

/// Upper bound of decay batches per settlement (500 × 20 = 10k links).
const SETTLEMENT_MAX_BATCHES: usize = 20;

/// `MnemonicState.memory_strength` assumed for a Concept that has never been
/// metabolized. The Facet's members are all optional, so the metabolism has to
/// supply a baseline before it can decay one.
const DEFAULT_MEMORY_STRENGTH: f64 = 0.5;

/// The `retention.retention_class` a pinned memory carries.
///
/// KIP 1.x pinning was a `metadata.pinned` flag; 2.0 has no generic metadata
/// bag, and "keep this out of the metabolism ladder" is a storage-lifecycle
/// statement, which is what the retention block is for. Retention also lives on
/// every element kind, so one class keeps pinning working for Concepts and
/// Propositions alike.
pub(crate) const PINNED_RETENTION_CLASS: &str = "pinned";

/// The `retention_class` an unpinned element returns to.
pub(crate) const STANDARD_RETENTION_CLASS: &str = "standard";

/// One command in, one result out: the settlement's whole view of a graph.
///
/// `readonly` picks the gate rather than the caller picking an executor, so a
/// pass cannot reach a write path by choosing the wrong method. Write commands
/// answer `outcome_unknown` on a timeout rather than erroring — the write may
/// already have committed (Spec §80.3), and every settlement command writes
/// absolute values, so a re-run on the next cycle is the honest recovery.
pub(crate) trait RunKip: Send + Sync {
    /// The graph being settled. Log context only; nothing branches on it.
    fn space_id(&self) -> &str;

    fn run_kip(
        &self,
        request: Request,
        readonly: bool,
    ) -> impl Future<Output = Result<Response, BoxError>> + Send;
}

/// Reads one command, or reports why the page is unusable. The passes all
/// degrade the same way — a cycle that could not scan is degraded, not
/// failed — so the two failure shapes (transport error, operation error)
/// collapse to one message here rather than at four call sites.
///
/// Hands back the whole [`Response`] rather than its result value: every row
/// reader borrows, and a page of outcomes is up to `OUTCOME_WINDOW` rows that
/// a settlement over many Skills would otherwise deep-clone once per read.
async fn read(port: &impl RunKip, request: Request) -> Result<Response, String> {
    match port.run_kip(request, true).await {
        Ok(response) if kip::succeeded(&response) => Ok(response),
        Ok(response) => Err(kip::error_message(&response)),
        Err(err) => Err(err.to_string()),
    }
}

/// What the bulk disuse metabolism did this cycle.
#[derive(Debug, Default)]
pub(crate) struct DecayPass {
    /// Concepts whose `MnemonicState.memory_strength` was decayed.
    pub decayed: u64,
    /// Set when the pass stopped early — decay did not complete this cycle.
    pub error: Option<String>,
}

/// Bulk disuse metabolism: decays `MnemonicState.memory_strength` — how
/// available a memory should be — and never Assertion confidence. A fact
/// nobody asked about lately is no less credible, and KIP 2.0 forbids letting
/// time erode a stance (Profile: "Do not decay epistemic confidence merely
/// because a fact has not been recalled recently"). A failing pass degrades —
/// the surrounding passes still run — but it must page an operator rather than
/// vanish into a debug log.
///
/// The cadence is `min_interval_ms`, enforced inside the sweep's own
/// `last_metabolized_at` filter, not the cycle scope. Gating on the `full`
/// scope as well used to look like caution and was a hole: full cycles are
/// scheduled every 168 formations, so a Space forming slowly went months
/// without metabolizing while `BrainMaintenance.md` §A.1 told the model the
/// sweep had already run and not to do it by hand. With the interval doing the
/// throttling, a scope that has nothing due costs one query that matches no
/// rows and breaks on the first batch.
pub(crate) async fn metabolize(
    port: &impl RunKip,
    policy: &MemoryPolicy,
    now_ms: u64,
    min_interval_ms: u64,
) -> DecayPass {
    let mut pass = DecayPass::default();
    // Every input is loop-invariant, so the command is too: the batch cursor
    // is the graph's own `last_metabolized_at` filter, not anything this
    // builder carries.
    let request = decay_request(policy, now_ms, min_interval_ms);
    for _ in 0..SETTLEMENT_MAX_BATCHES {
        let response = match port.run_kip(request.clone(), false).await {
            Ok(response) if kip::succeeded(&response) => response,
            Ok(response) => {
                pass.error = Some(kip::error_message(&response));
                break;
            }
            Err(err) => {
                pass.error = Some(err.to_string());
                break;
            }
        };
        let updated = kip::changed(&response, "update");
        pass.decayed += updated;
        if updated < SETTLEMENT_BATCH_LIMIT as u64 {
            break;
        }
    }
    if let Some(error) = &pass.error {
        log::error!(
            target: "brain",
            space_id = port.space_id();
            "memory-strength metabolism failed — disuse decay is NOT running \
             (graph past the full-scan engine cap?): {error}"
        );
    }
    pass
}

/// One Assertion an actor has revised, as the correction scan reads it.
#[derive(Debug, Clone)]
pub(crate) struct SupersededRow {
    pub assertion: String,
    /// The *transaction* coordinate the revision landed at.
    pub space_seq: u64,
    /// Whose claim needed revising. KIP 1.x kept a free-text
    /// `metadata.source`; 2.0 attributes a claim to a semantic actor, and that
    /// actor is the reliability signal — this is not a statement about the
    /// caller's authority.
    pub actor: Option<String>,
}

/// One page of correction discovery: what was superseded, and how far the
/// cursor may advance for having read it.
#[derive(Debug, Default)]
pub(crate) struct CorrectionScan {
    pub rows: Vec<SupersededRow>,
    /// The cursor this page earned — never past a coordinate it only half
    /// read. Equal to the caller's `after` when nothing was consumed.
    pub watermark: u64,
    /// Set when one transaction superseded more Assertions than a page holds,
    /// so the remainder at that coordinate is lost.
    ///
    /// The operator signal is the error log the scan writes; this field is
    /// how a test asserts the truncation arm fired without reading logs. The
    /// settlement report has no field for it, and adding one would change a
    /// persisted shape to carry something only a test reads.
    pub truncated_at: Option<u64>,
    /// Set when the scan failed — new corrections were not recorded this
    /// cycle.
    pub error: Option<String>,
}

/// Correction discovery: the Assertions an actor has revised since `after`.
///
/// In KIP 1.x this was a `metadata.superseded` flag the settlement could clear
/// with a second write; an Assertion is immutable, so the cursor is the Space
/// sequence coordinate instead — processed revisions fall behind the
/// watermark, and a backlog larger than one batch drains across cycles rather
/// than starving new corrections behind the first LIMIT-full.
///
/// Recording what comes back is the caller's: the usage ledger dedupes
/// corrections and the `source_reliability` aggregate is Space extension
/// state, neither of which the graph owns.
pub(crate) async fn scan_corrections(port: &impl RunKip, after: u64) -> CorrectionScan {
    let mut scan = CorrectionScan {
        watermark: after,
        ..Default::default()
    };

    let request = kip::request_with(
        format!(
            "FIND(?a.id, ?a._system.space_seq, ?a.asserted_by) WHERE {{\n  ?a ASSERTION {{}}\n  FILTER(?a.lifecycle.status == \"superseded\")\n  FILTER(?a._system.space_seq > :after)\n}}\nORDER BY ?a._system.space_seq\nLIMIT {SETTLEMENT_BATCH_LIMIT}"
        ),
        kip::param("after", after),
    );
    let response = match read(port, request).await {
        Ok(response) => response,
        Err(error) => {
            log::error!(
                target: "brain",
                space_id = port.space_id();
                "correction discovery scan failed — new corrections are NOT being \
                 recorded (graph past the full-scan engine cap?): {error}"
            );
            scan.error = Some(error);
            return scan;
        }
    };

    let Some(rows) = kip::ok_result(&response) else {
        return scan;
    };
    let page_full = rows
        .as_array()
        .is_some_and(|rows| rows.len() >= SETTLEMENT_BATCH_LIMIT);
    scan.rows = superseded_rows(rows);
    let seqs: Vec<u64> = scan.rows.iter().map(|row| row.space_seq).collect();
    scan.watermark = match correction_watermark(&seqs, page_full, after) {
        CorrectionWatermark::Consumed(seq) => seq,
        CorrectionWatermark::Truncated(seq) => {
            log::error!(
                target: "brain",
                space_id = port.space_id(),
                space_seq = seq;
                "one transaction superseded more Assertions than a settlement page \
                 holds ({SETTLEMENT_BATCH_LIMIT}); the remainder at this coordinate \
                 will not be recorded as corrections"
            );
            scan.truncated_at = Some(seq);
            seq
        }
    };
    scan
}

/// Fires the silence Watches whose deadline has passed.
///
/// See [`watch`] for why this half is the runtime's and the delta half is the
/// model's. Errors are reported rather than propagated: a cycle that could not
/// sweep is degraded, not failed.
pub(crate) async fn sweep_watches(port: &impl RunKip, now_ms: u64) -> WatchSettlement {
    let now = kip::timestamp(now_ms);
    let mut report = WatchSettlement::default();

    let response = match read(port, watch::due_silence_watches_request(&now)).await {
        Ok(response) => response,
        Err(error) => {
            report.error = Some(error);
            return report;
        }
    };
    let Some(result) = kip::ok_result(&response) else {
        return report;
    };

    for due in watch::due_watches(result) {
        let response = match port
            .run_kip(watch::fire_watch_request(&due, &now), false)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                report.error = Some(err.to_string());
                return report;
            }
        };
        if kip::succeeded(&response) {
            report.fired += 1;
        } else if watch::is_version_conflict(&response) {
            // The maintenance model moved this Watch between the scan and
            // the write. It stays armed; the next sweep re-reads it.
            report.conflicted += 1;
        } else {
            log::warn!(
                target: "brain",
                space_id = port.space_id(),
                watch = due.id;
                "firing a due silence Watch failed: {}",
                kip::error_message(&response)
            );
            report.conflicted += 1;
        }
    }
    report
}

/// Runs the deterministic Skill lifecycle rule over graded outcomes.
///
/// See [`skill`] for the rule and why it lives in code. Errors are reported
/// rather than propagated: a cycle that could not grade is degraded, not
/// failed.
pub(crate) async fn settle_skills(port: &impl RunKip, now_ms: u64) -> SkillSettlement {
    let now = kip::timestamp(now_ms);
    let mut report = SkillSettlement::default();

    let response = match read(port, skill::skills_request()).await {
        Ok(response) => response,
        Err(error) => {
            report.error = Some(error);
            return report;
        }
    };
    let Some(result) = kip::ok_result(&response) else {
        return report;
    };

    for row in skill::skill_rows(result) {
        // One read per Skill rather than one grouped read: the window is
        // per-Skill and per-cursor, and a Skill that has never been graded
        // starts from a different coordinate than one that has.
        let outcomes = match read(
            port,
            skill::outcomes_request(&row.id, &row.task_family, row.cursor),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                log::warn!(
                    target: "brain",
                    space_id = port.space_id(),
                    skill = row.id;
                    "reading graded outcomes failed: {error}"
                );
                continue;
            }
        };
        let Some(window) = kip::ok_result(&outcomes).map(skill::tally) else {
            continue;
        };
        if window.graded == 0 {
            // Nothing attributed to this Skill since the last verdict, so
            // there is nothing to judge and no reason to price the family
            // aggregate below.
            continue;
        }

        // The baseline a trial is measured against: the whole family up to
        // the end of this window, from which `decide` subtracts what was
        // linked to this Skill (Profile §6.5). Read every pass rather than
        // only when a trial opens, because which arm of the rule fires is
        // not known until the tallies are in.
        let family = match read(
            port,
            skill::family_tally_request(&row.task_family, window.cursor),
        )
        .await
        {
            Ok(response) => kip::ok_result(&response)
                .map(skill::family_tally)
                .unwrap_or_default(),
            Err(error) => {
                log::warn!(
                    target: "brain",
                    space_id = port.space_id(),
                    skill = row.id;
                    "reading the task family's baseline failed: {error}"
                );
                continue;
            }
        };

        let Some(verdict) = skill::decide(&row, &window, &family) else {
            // No new graded outcome: an idle stream writes nothing.
            continue;
        };

        let response = match port
            .run_kip(skill::verdict_request(&row, &verdict, &window, &now), false)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                report.error = Some(err.to_string());
                return report;
            }
        };
        if kip::succeeded(&response) {
            report.graded += 1;
            if verdict.transition.is_some() {
                report.transitions += 1;
                log::info!(
                    target: "brain",
                    space_id = port.space_id(),
                    skill = row.id;
                    "skill lifecycle verdict: {}",
                    verdict.rationale
                );
            }
        } else if watch::is_version_conflict(&response) {
            // The cursor did not advance, so the next pass re-reads the
            // same outcomes and reaches the same verdict.
            report.conflicted += 1;
        } else {
            log::warn!(
                target: "brain",
                space_id = port.space_id(),
                skill = row.id;
                "recording a lifecycle verdict failed: {}",
                kip::error_message(&response)
            );
            report.conflicted += 1;
        }
    }
    report
}

/// Settlement write: one disuse-metabolism batch (plan M2 step 2).
///
/// This decays `MnemonicState.memory_strength` on Concepts — accessibility, not
/// truth. KIP 1.x decayed `metadata.confidence` on every link, which is exactly
/// what the Profile now forbids: a fact nobody has asked about in a month is no
/// less credible, and an Assertion is immutable in any case.
///
/// Batched and re-run rather than unbounded, and the rate-window filter is also
/// the intra-settlement batch cursor: rows stamped `now` by this pass stop
/// matching, so an interval of `0` still terminates and merely disables the
/// *cross-cycle* limit. A pinned Concept is exempt through its retention class.
///
/// A Concept that has never carried the Facet is metabolized from a baseline
/// rather than skipped: leaving it out would make "the model forgot to set
/// MnemonicState" mean "this memory never fades", which is not a decision
/// anybody made.
fn decay_request(policy: &MemoryPolicy, now_ms: u64, min_interval_ms: u64) -> Request {
    let parameters = serde_json::Map::from_iter([
        ("baseline".to_string(), Json::from(DEFAULT_MEMORY_STRENGTH)),
        (
            "factor".to_string(),
            Json::from(policy.memory_strength_decay_factor),
        ),
        ("floor".to_string(), Json::from(policy.decay_floor)),
        ("now".to_string(), Json::from(kip::timestamp(now_ms))),
        (
            "metabolized_before".to_string(),
            Json::from(kip::timestamp(now_ms.saturating_sub(min_interval_ms))),
        ),
        ("pinned".to_string(), Json::from(PINNED_RETENTION_CLASS)),
        ("limit".to_string(), Json::from(SETTLEMENT_BATCH_LIMIT)),
    ]);
    kip::request_with(
        r#"UPDATE ?c
SET FACET "MnemonicState" {
  memory_strength: CLAMP(MUL(COALESCE(?c.facets["MnemonicState"].memory_strength, :baseline), :factor), :floor, 1.0),
  last_metabolized_at: :now
}
WHERE {
  ?c CONCEPT {}
  FILTER(IS_NULL(?c.retention.retention_class) || ?c.retention.retention_class != :pinned)
  FILTER(IS_NULL(?c.facets["MnemonicState"].last_metabolized_at) || ?c.facets["MnemonicState"].last_metabolized_at < :metabolized_before)
  FILTER(IS_NULL(?c.facets["MnemonicState"].memory_strength) || ?c.facets["MnemonicState"].memory_strength > :floor)
}
LIMIT :limit"#,
        parameters,
    )
}

/// How far a correction page may advance the cursor, and whether anything was
/// lost getting there.
enum CorrectionWatermark {
    /// Every coordinate up to this one was read whole.
    Consumed(u64),
    /// One coordinate held more rows than a page, so advancing past it drops
    /// the remainder. Reported so an operator hears about it.
    Truncated(u64),
}

/// Chooses the cursor a correction page has actually earned.
///
/// `_system.space_seq` is the *transaction* coordinate, so one commit stamps
/// every Assertion it revised with the same number, and the scan's `>` filter
/// cannot page inside one coordinate. A full page therefore hands its trailing
/// coordinate back and stops one short of it — re-reading costs nothing,
/// because `record_correction` dedupes, while advancing past a half-read
/// coordinate drops the rest of it for good.
///
/// The one case with no good answer is a full page that is *entirely* one
/// coordinate: standing still re-reads it forever and never reaches the
/// corrections behind it, so the cursor advances and says so.
fn correction_watermark(seqs: &[u64], page_full: bool, after: u64) -> CorrectionWatermark {
    let Some(last) = seqs.last().copied() else {
        return CorrectionWatermark::Consumed(after);
    };
    if !page_full {
        // A short page means the scan reached the end of the backlog, so every
        // coordinate in it was read whole.
        return CorrectionWatermark::Consumed(last.max(after));
    }
    match seqs.iter().copied().filter(|seq| *seq < last).max() {
        Some(seq) => CorrectionWatermark::Consumed(seq.max(after)),
        None => CorrectionWatermark::Truncated(last),
    }
}

/// Reads the rows of the correction-discovery scan —
/// `FIND(?a.id, ?a._system.space_seq, ?a.asserted_by)`.
fn superseded_rows(result: &Json) -> Vec<SupersededRow> {
    let element_id = |value: &Json| -> Option<String> {
        match value {
            Json::String(id) => Some(id.clone()),
            Json::Object(map) => map.get("id").and_then(Json::as_str).map(str::to_string),
            _ => None,
        }
    };
    result
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let columns = row.as_array()?;
                    let id = columns.first().and_then(Json::as_str)?;
                    Some(SupersededRow {
                        assertion: id.to_string(),
                        space_seq: columns.get(1).and_then(Json::as_u64)?,
                        actor: columns.get(2).and_then(element_id),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_kip::{KipError, KipErrorCode};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::VecDeque;

    /// The settlement's other adapter: a scripted graph.
    ///
    /// Answers are keyed by a substring of the command text — enough to tell
    /// the passes' reads apart — and each key holds a queue, so a paging pass
    /// can be walked batch by batch. An unmatched command answers with an
    /// empty result rather than failing: a pass that reads something the test
    /// did not script should behave as it does against a graph that has
    /// nothing to say, not blow up somewhere unrelated.
    #[derive(Default)]
    struct FakeKip {
        replies: Mutex<Vec<(&'static str, VecDeque<Response>)>>,
        seen: Mutex<Vec<(String, bool)>>,
    }

    impl FakeKip {
        fn new(replies: impl IntoIterator<Item = (&'static str, Vec<Response>)>) -> Self {
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(key, queue)| (key, VecDeque::from(queue)))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// Every command the passes issued, with the gate they asked for.
        fn seen(&self) -> Vec<(String, bool)> {
            self.seen.lock().clone()
        }

        fn wrote(&self) -> Vec<String> {
            self.seen()
                .into_iter()
                .filter(|(_, readonly)| !readonly)
                .map(|(command, _)| command)
                .collect()
        }
    }

    impl RunKip for FakeKip {
        fn space_id(&self) -> &str {
            "fake_space"
        }

        async fn run_kip(&self, request: Request, readonly: bool) -> Result<Response, BoxError> {
            let command = request
                .operations
                .first()
                .and_then(|op| op.command.clone())
                .unwrap_or_default();
            self.seen.lock().push((command.clone(), readonly));
            let mut replies = self.replies.lock();
            for (key, queue) in replies.iter_mut() {
                if command.contains(*key)
                    && let Some(response) = queue.pop_front()
                {
                    return Ok(response);
                }
            }
            Ok(Response::ok(json!([])))
        }
    }

    /// A committed mutation that moved `count` rows under `op`.
    fn changed(op: &str, count: usize) -> Response {
        Response::ok(json!({
            "changes": (0..count).map(|_| json!({"op": op})).collect::<Vec<_>>()
        }))
    }

    fn failed(message: &str) -> Response {
        Response::failed(KipError::new(KipErrorCode::InternalError, message))
    }

    /// The one failure a sweep treats as ordinary: the maintenance model moved
    /// the element between the scan and the write.
    fn conflicted() -> Response {
        Response::failed(KipError::new(
            KipErrorCode::VersionConflict,
            "moved under the sweep",
        ))
    }

    #[tokio::test]
    async fn metabolism_pages_until_a_batch_comes_back_short() {
        let port = FakeKip::new([(
            "MnemonicState",
            vec![
                changed("update", SETTLEMENT_BATCH_LIMIT),
                changed("update", SETTLEMENT_BATCH_LIMIT),
                changed("update", 7),
            ],
        )]);
        let pass = metabolize(&port, &MemoryPolicy::default(), 1_000, 0).await;

        assert_eq!(pass.decayed, SETTLEMENT_BATCH_LIMIT as u64 * 2 + 7);
        assert!(pass.error.is_none());
        // A short batch ends the pass: no fourth command was issued.
        assert_eq!(port.seen().len(), 3);
        // Decay is a write, whatever it decays.
        assert_eq!(port.wrote().len(), 3);
    }

    #[tokio::test]
    async fn a_failing_metabolism_reports_what_it_managed_and_stops() {
        let port = FakeKip::new([(
            "MnemonicState",
            vec![
                changed("update", SETTLEMENT_BATCH_LIMIT),
                failed("scan cap"),
            ],
        )]);
        let pass = metabolize(&port, &MemoryPolicy::default(), 1_000, 0).await;

        // The batch that did land is still reported — a degraded cycle, not a
        // cycle that did nothing.
        assert_eq!(pass.decayed, SETTLEMENT_BATCH_LIMIT as u64);
        assert!(pass.error.unwrap().contains("scan cap"));
        assert_eq!(port.seen().len(), 2);
    }

    #[tokio::test]
    async fn the_metabolism_never_runs_past_its_batch_ceiling() {
        // A graph that always answers "a full page" would page forever
        // without the ceiling.
        let port = FakeKip::new([(
            "MnemonicState",
            (0..SETTLEMENT_MAX_BATCHES + 5)
                .map(|_| changed("update", SETTLEMENT_BATCH_LIMIT))
                .collect(),
        )]);
        let pass = metabolize(&port, &MemoryPolicy::default(), 1_000, 0).await;

        assert_eq!(port.seen().len(), SETTLEMENT_MAX_BATCHES);
        assert_eq!(
            pass.decayed,
            (SETTLEMENT_MAX_BATCHES * SETTLEMENT_BATCH_LIMIT) as u64
        );
    }

    /// The correction cursor advances only over coordinates it read whole.
    #[tokio::test]
    async fn the_correction_cursor_never_steps_over_a_half_read_coordinate() {
        // A short page reached the end of the backlog: every coordinate in it
        // was read whole, so the cursor takes the last one.
        let scan = scan_corrections(&superseded(&[(7, "actor_a"), (9, "actor_b")]), 3).await;
        assert_eq!(scan.watermark, 9);
        assert_eq!(scan.rows.len(), 2);
        assert_eq!(scan.rows[0].actor.as_deref(), Some("actor_a"));
        assert!(scan.truncated_at.is_none());

        // Nothing to read leaves the cursor alone.
        assert_eq!(scan_corrections(&superseded(&[]), 3).await.watermark, 3);

        // A full page stops one coordinate short: `space_seq` is the
        // transaction coordinate, so the trailing 9s may have more behind them
        // and the next scan has to see them again.
        let mut page: Vec<(u64, &str)> = vec![(7, "a"), (8, "a")];
        page.extend((0..SETTLEMENT_BATCH_LIMIT - 2).map(|_| (9u64, "a")));
        let full = scan_corrections(&superseded(&page), 3).await;
        assert_eq!(full.watermark, 8);
        assert!(full.truncated_at.is_none());

        // Never backwards, whatever the page held: a cursor already past the
        // coordinate this page earned stays where it is, so corrections
        // already recorded are not scanned again forever.
        let ahead = scan_corrections(&superseded(&page), 8).await;
        assert_eq!(ahead.watermark, 8);
        let further = scan_corrections(&superseded(&page), 42).await;
        assert_eq!(further.watermark, 42);

        // A full page that is entirely one coordinate has no good answer:
        // standing still would re-read it forever, so it advances and says so.
        let one: Vec<(u64, &str)> = (0..SETTLEMENT_BATCH_LIMIT).map(|_| (9u64, "a")).collect();
        let stuck = scan_corrections(&superseded(&one), 3).await;
        assert_eq!(stuck.watermark, 9);
        assert_eq!(stuck.truncated_at, Some(9));
    }

    #[tokio::test]
    async fn a_failed_correction_scan_leaves_the_cursor_where_it_was() {
        let port = FakeKip::new([("superseded", vec![failed("full-scan cap")])]);
        let scan = scan_corrections(&port, 42).await;

        assert_eq!(scan.watermark, 42);
        assert!(scan.rows.is_empty());
        assert!(scan.error.unwrap().contains("full-scan cap"));
        // Reading corrections never writes.
        assert!(port.wrote().is_empty());
    }

    /// Builds a port whose correction scan answers with these
    /// `(space_seq, actor)` rows. Whether the page is full is derived from its
    /// length, exactly as the pass derives it, so a test cannot claim a full
    /// page it did not supply.
    fn superseded(rows: &[(u64, &str)]) -> FakeKip {
        let result: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(n, (seq, actor))| json!([format!("A-{n}"), seq, actor]))
            .collect();
        FakeKip::new([("superseded", vec![Response::ok(json!(result))])])
    }

    #[tokio::test]
    async fn a_due_watch_is_fired_once_and_a_conflicted_one_stays_armed() {
        let armed = |id: &str| {
            json!([
                id,
                "the vendor never replied",
                {"watch_class": "silence", "due_at": "2026-01-01T00:00:00Z"},
                1
            ])
        };
        let port = FakeKip::new([
            (
                "watch_class",
                vec![Response::ok(json!([armed("W-1"), armed("W-2")]))],
            ),
            ("watch_fire", vec![changed("lifecycle", 1), conflicted()]),
        ]);
        let report = sweep_watches(&port, 1_000).await;

        assert_eq!(report.fired, 1);
        // A Watch the maintenance model moved between scan and write stays
        // armed rather than being reported as fired.
        assert_eq!(report.conflicted, 1);
        assert!(report.error.is_none());
        // One scan, then one write per due Watch — never a write per page.
        assert_eq!(port.wrote().len(), 2);
    }

    #[tokio::test]
    async fn a_watch_scan_that_fails_writes_nothing() {
        let port = FakeKip::new([("Watch", vec![failed("engine busy")])]);
        let report = sweep_watches(&port, 1_000).await;

        assert_eq!(report.fired, 0);
        assert!(report.error.unwrap().contains("engine busy"));
        assert!(port.wrote().is_empty());
    }

    #[tokio::test]
    async fn a_skill_with_no_graded_outcome_is_not_priced_against_its_family() {
        let port = FakeKip::new([(
            r#"type: "Skill""#,
            vec![Response::ok(json!([[
                "C-skill",
                "compress the log",
                {"task_family": "logs", "status": "proposed"},
                {"success_count": 0, "failure_count": 0, "graded_count": 0},
                null,
                1
            ]]))],
        )]);
        let report = settle_skills(&port, 1_000).await;

        assert_eq!(report.graded, 0);
        assert_eq!(report.transitions, 0);
        // The scan and the outcome read happened; the family baseline did not,
        // because an idle stream has nothing to be measured against.
        assert_eq!(port.seen().len(), 2);
        assert!(port.wrote().is_empty());
    }

    #[tokio::test]
    async fn a_failed_skill_scan_degrades_the_cycle_rather_than_failing_it() {
        let port = FakeKip::new([("Skill", vec![failed("scan cap")])]);
        let report = settle_skills(&port, 1_000).await;

        assert_eq!(report.graded, 0);
        assert!(report.error.unwrap().contains("scan cap"));
        assert!(port.wrote().is_empty());
    }
}

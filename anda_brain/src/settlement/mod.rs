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
    types::{ArmedWatch, Dependent, MemoryPolicy, RevisedRoot, SkillSettlement, WatchSettlement},
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
    /// The Proposition the claim took a stance on.
    pub proposition: Option<String>,
    /// The Assertions that superseded it.
    pub superseded_by: Vec<String>,
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
            "FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition, ?a.lifecycle.superseded_by) WHERE {{\n  ?a ASSERTION {{}}\n  FILTER(?a.lifecycle.status == \"superseded\")\n  FILTER(?a._system.space_seq > :after)\n}}\nORDER BY ?a._system.space_seq\nLIMIT {SETTLEMENT_BATCH_LIMIT}"
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

/// Evaluates the armed Watches and fires the silence Watches whose deadline
/// has passed.
///
/// See [`watch`] for what the runtime evaluates and what it only guards.
/// `head_seq` is the Space's coordinate as the sweep began — what a
/// structured Watch is evaluated through — and `consumed_seq` is where the
/// last completed maintenance cycle read the stream through, which is what a
/// prose silence Watch waits on (§5.11). Errors are reported rather than
/// propagated: a cycle that could not sweep is degraded, not failed.
pub(crate) async fn sweep_watches(
    port: &impl RunKip,
    now_ms: u64,
    head_seq: Option<u64>,
    consumed_seq: Option<u64>,
) -> WatchSettlement {
    let now = kip::timestamp(now_ms);
    let mut report = WatchSettlement::default();
    let mut evaluated: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // The armed set, evaluated wherever the condition is the runtime's to
    // read. Read whole rather than filtered in KQL: whether a condition is
    // structured is a question about its shape, which is this side's to ask.
    let armed = match read(port, watch::watches_request("armed")).await {
        Ok(response) => kip::ok_result(&response)
            .map(watch::read_watch_rows)
            .unwrap_or_default(),
        Err(error) => {
            report.error = Some(error);
            return report;
        }
    };
    let structured: Vec<(watch::WatchRow, watch::ChangeFilter)> = armed
        .into_iter()
        .filter_map(|row| {
            let filter = watch::change_filter(&row.condition)?;
            Some((row, filter))
        })
        .collect();
    if !structured.is_empty() {
        match head_seq {
            Some(head) => {
                if let Err(error) = evaluate_structured(
                    port,
                    &now,
                    head,
                    consumed_seq,
                    &structured,
                    &mut evaluated,
                    &mut report,
                )
                .await
                {
                    report.error = Some(error);
                    return report;
                }
            }
            None => log::warn!(
                target: "brain",
                space_id = port.space_id();
                "the Space's head is unknown; structured Watches are not evaluated this cycle"
            ),
        }
    }

    // The silence Watches past their deadline that evaluation did not settle:
    // prose conditions, whose consumption is the Brain's. A structured Watch
    // lands here only when the head was unknown, and is then held to the
    // same guard.
    let due = match read(port, watch::due_silence_watches_request(&now)).await {
        Ok(response) => kip::ok_result(&response)
            .map(watch::read_watch_rows)
            .unwrap_or_default(),
        Err(error) => {
            report.error = Some(error);
            return report;
        }
    };
    for row in due {
        if evaluated.contains(&row.id) {
            continue;
        }
        let request = match (row.due_seen_seq, consumed_seq, head_seq) {
            // The Brain has read the stream past the head at which this
            // deadline was first seen passed: silence is a fact now.
            (Some(seen), Some(consumed), _) if consumed >= seen => {
                watch::fire_watch_request(&row, &now, watch::Fire::Silence, Some(consumed))
            }
            (Some(_), _, _) => {
                report.deferred += 1;
                continue;
            }
            // First sight of the passed deadline: record the head, so the
            // consumption the next cycle reaches can be measured against it.
            (None, _, Some(head)) => watch::stamp_request(&row, "due_seen_seq", head),
            (None, _, None) => {
                report.deferred += 1;
                continue;
            }
        };
        let firing = row.due_seen_seq.is_some();
        match write(port, request, &row.id, "settling a due silence Watch").await {
            Ok(true) if firing => report.fired += 1,
            Ok(true) => report.deferred += 1,
            Ok(false) => report.conflicted += 1,
            Err(error) => {
                report.error = Some(error);
                return report;
            }
        }
    }
    report
}

/// Evaluates the structured Watches against the Change Stream from the
/// oldest coordinate any of them needs, through `head`.
///
/// One stream read serves every Watch, and each is matched only past its own
/// start, so a Watch armed yesterday is not fired by a change committed last
/// week. The start is `evaluated_seq` where a sweep has stamped one; otherwise
/// it is the Watch's arming, found in the stream itself ([`watch::armed_at`]).
/// A Watch is armed by the maintenance model, after the cycle's `space_seq` —
/// so a Watch not yet stamped was armed past `consumed_seq`, which is where
/// the stream is read from for it, and where it starts when its arming is not
/// in the window after all (a Watch armed before this sweep existed).
///
/// The ids evaluated are collected so the due sweep does not settle them a
/// second time on a version this pass already moved.
async fn evaluate_structured(
    port: &impl RunKip,
    now: &str,
    head: u64,
    consumed_seq: Option<u64>,
    watches: &[(watch::WatchRow, watch::ChangeFilter)],
    evaluated: &mut std::collections::BTreeSet<String>,
    report: &mut WatchSettlement,
) -> Result<(), String> {
    let tentative = consumed_seq.unwrap_or(0);
    let from = watches
        .iter()
        .map(|(row, _)| row.evaluated_seq.unwrap_or(tentative))
        .min()
        .unwrap_or(head);
    let (envelopes, consumed_to) = read_changes(port, from, head).await?;

    // The slots the filters name, resolved once: an Assertion entry carries
    // only `refs.proposition`, so matching it against a slot needs the
    // Propositions of that slot.
    let mut slots = watch::SlotIndex::new();
    for (_, filter) in watches {
        if let Some(slot) = &filter.slot
            && !slots.contains_key(slot)
        {
            let response = read(port, watch::slot_request(&slot.0, &slot.1)).await?;
            let propositions = kip::ok_result(&response)
                .and_then(Json::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| match row {
                            Json::String(id) => Some(id.clone()),
                            Json::Object(map) => map.get("id").and_then(Json::as_str).map(str::to_string),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            slots.insert(slot.clone(), propositions);
        }
    }

    for (row, filter) in watches {
        evaluated.insert(row.id.clone());
        let start = row
            .evaluated_seq
            .or_else(|| watch::armed_at(&envelopes, &row.id))
            .unwrap_or(tentative);
        let through = consumed_to.max(start);
        let due_silence = row.is_silence() && row.is_due(now);
        let (request, outcome) = match watch::first_match(filter, start, &envelopes, &slots) {
            Some(seq) if row.is_silence() => (
                watch::disarm_matched_request(row, now, seq),
                Outcome::Disarmed,
            ),
            Some(seq) => (
                watch::fire_watch_request(row, now, watch::Fire::Delta(seq), None),
                Outcome::Fired,
            ),
            // Nothing matched, and the stream is consumed through the head:
            // for a due silence Watch that is silence, as §5.11 means it.
            None if due_silence && through >= head => (
                watch::fire_watch_request(row, now, watch::Fire::Silence, Some(through)),
                Outcome::Fired,
            ),
            // Nothing new to read; nothing to record.
            None if through <= start => {
                if due_silence {
                    report.deferred += 1;
                }
                continue;
            }
            None => (
                watch::stamp_request(row, "evaluated_seq", through),
                if due_silence {
                    Outcome::Deferred
                } else {
                    Outcome::Evaluated
                },
            ),
        };
        match write(port, request, &row.id, "evaluating a structured Watch").await {
            Ok(true) => match outcome {
                Outcome::Fired => report.fired += 1,
                Outcome::Disarmed => report.disarmed += 1,
                Outcome::Deferred => report.deferred += 1,
                Outcome::Evaluated => {}
            },
            Ok(false) => report.conflicted += 1,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// What one Watch write meant, for the report.
enum Outcome {
    Fired,
    Disarmed,
    Deferred,
    Evaluated,
}

/// Reads the Change Stream after `from`, page by page, within the sweep's
/// budget.
///
/// Answers the envelopes and the coordinate they are complete through: the
/// head when the stream was read to its end, or the last coordinate read when
/// the budget ran out first — which the caller records, so the next sweep
/// continues from there instead of concluding silence over changes it never
/// saw.
async fn read_changes(port: &impl RunKip, from: u64, head: u64) -> Result<(Vec<Json>, u64), String> {
    let mut after = from;
    let mut envelopes: Vec<Json> = Vec::new();
    for _ in 0..watch::CHANGES_MAX_PAGES {
        let response = read(port, watch::changes_request(after)).await?;
        let page = kip::ok_result(&response)
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let full = page.len() >= watch::CHANGES_PAGE_LIMIT;
        let last = page
            .iter()
            .filter_map(|envelope| envelope.get("space_seq").and_then(Json::as_u64))
            .max();
        envelopes.extend(page);
        match last {
            Some(seq) if full => after = seq,
            Some(seq) => return Ok((envelopes, seq.max(head))),
            None => return Ok((envelopes, after.max(head))),
        }
    }
    Ok((envelopes, after))
}

/// One guarded settlement write.
///
/// `Ok(true)` committed; `Ok(false)` was refused — a version conflict is the
/// ordinary outcome of racing the maintenance model, anything else is logged —
/// and the element stays where it was for the next sweep. `Err` is the
/// transport failing, which ends the pass.
async fn write(
    port: &impl RunKip,
    request: Request,
    watch_id: &str,
    what: &str,
) -> Result<bool, String> {
    let response = port
        .run_kip(request, false)
        .await
        .map_err(|err| err.to_string())?;
    if kip::succeeded(&response) {
        return Ok(true);
    }
    if !watch::is_version_conflict(&response) {
        log::warn!(
            target: "brain",
            space_id = port.space_id(),
            watch = watch_id;
            "{what} failed: {}",
            kip::error_message(&response)
        );
    }
    Ok(false)
}

/// The Watches in one status, as the maintenance prompt receives them.
///
/// Best-effort: an error here costs the cycle one input, not the cycle. A
/// Space that has never declared a Watch resolves the type fine (the Profile
/// declares it), so a failure is a real one and is logged as such.
pub(crate) async fn watches_in_status(port: &impl RunKip, status: &str) -> Vec<ArmedWatch> {
    match read(port, watch::watches_request(status)).await {
        Ok(response) => kip::ok_result(&response)
            .map(watch::read_watch_rows)
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.watch)
            .collect(),
        Err(error) => {
            log::warn!(
                target: "brain",
                space_id = port.space_id(),
                status;
                "reading Watches for the maintenance assessment failed: {error}"
            );
            Vec::new()
        }
    }
}

/// How many revised roots one cycle walks.
pub(crate) const REVISED_ROOTS_LIMIT: usize = 20;

/// How far a derivation walk follows Activity lineage from a revised root.
pub(crate) const DEPENDENTS_DEPTH: u64 = 2;

/// How many dependents one walk lists.
pub(crate) const DEPENDENTS_LIMIT: usize = 20;

/// The cognition derived from each newly superseded Assertion.
///
/// Spec §57.5 asks a Brain to review the dependents of a revised root, and
/// §63.5 makes them discoverable in one read — so the runtime reads them and
/// hands the cycle a list, where before it handed a count. Reachability is
/// topology, not judgment: nothing here is flagged stale; the cycle decides.
pub(crate) async fn revised_roots(port: &impl RunKip, rows: &[SupersededRow]) -> Vec<RevisedRoot> {
    let mut roots = Vec::with_capacity(rows.len().min(REVISED_ROOTS_LIMIT));
    for row in rows.iter().take(REVISED_ROOTS_LIMIT) {
        let request = kip::request_with(
            format!("LIST DEPENDENTS :root DEPTH {DEPENDENTS_DEPTH} LIMIT {DEPENDENTS_LIMIT}"),
            kip::param("root", row.assertion.as_str()),
        );
        let (dependents, truncated) = match read(port, request).await {
            Ok(response) => (dependents_of(&response), dependents_truncated(&response)),
            Err(error) => {
                log::warn!(
                    target: "brain",
                    space_id = port.space_id(),
                    assertion = row.assertion;
                    "walking the dependents of a revised root failed: {error}"
                );
                (Vec::new(), true)
            }
        };
        roots.push(RevisedRoot {
            assertion: row.assertion.clone(),
            proposition: row.proposition.clone(),
            actor: row.actor.clone(),
            superseded_by: row.superseded_by.clone(),
            space_seq: row.space_seq,
            dependents,
            truncated,
        });
    }
    roots
}

/// The rows of a `LIST DEPENDENTS` answer — `{id, kind, distance, via}`.
fn dependents_of(response: &Response) -> Vec<Dependent> {
    kip::ok_result(response)
        .and_then(Json::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Dependent {
                        id: row.get("id")?.as_str()?.to_string(),
                        kind: row
                            .get("kind")
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        distance: row.get("distance").and_then(Json::as_u64).unwrap_or(1),
                        via: row
                            .get("via")
                            .and_then(|via| via.get("activity"))
                            .and_then(Json::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the walk said it was cut short: the §63.5 `truncated` caveat, or
/// a full page.
fn dependents_truncated(response: &Response) -> bool {
    let Some(result) = response.results.first() else {
        return false;
    };
    result.next_cursor.is_some()
        || result.warnings.iter().any(|warning| {
            matches!(warning, anda_kip::Warning::Coded { code, .. } if code == "truncated")
        })
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
/// `FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition,
/// ?a.lifecycle.superseded_by)`.
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
                        proposition: columns.get(3).and_then(element_id),
                        superseded_by: columns
                            .get(4)
                            .and_then(Json::as_array)
                            .map(|ids| ids.iter().filter_map(element_id).collect())
                            .unwrap_or_default(),
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
            .map(|(n, (seq, actor))| json!([format!("A-{n}"), seq, actor, "P-1", ["A-99"]]))
            .collect();
        FakeKip::new([("superseded", vec![Response::ok(json!(result))])])
    }

    /// A Watch row as the scan projects it: `(id, name, attributes, version,
    /// space_seq)`.
    fn watch_row(id: &str, attributes: Json, _armed_seq: u64) -> Json {
        json!([id, "the vendor never replied", attributes, 1])
    }

    /// The armed scan and the due-silence scan, told apart by the filter each
    /// one carries.
    const ARMED_SCAN: &str = "status == :status";
    const DUE_SCAN: &str = r#"watch_class == "silence""#;

    #[tokio::test]
    async fn a_prose_silence_watch_waits_for_the_brain_to_consume_its_deadline() {
        let prose = |seen: Option<u64>| {
            let mut attributes = json!({
                "watch_class": "silence",
                "condition": "no reply from the vendor",
                "due_at": "2026-01-01T00:00:00Z"
            });
            if let Some(seen) = seen {
                attributes["due_seen_seq"] = json!(seen);
            }
            watch_row("W-1", attributes, 3)
        };

        // First sight of the passed deadline: the head is recorded on the
        // Watch and nothing fires — the clock alone proves nothing (§5.11).
        let port = FakeKip::new([(DUE_SCAN, vec![Response::ok(json!([prose(None)]))])]);
        let report = sweep_watches(&port, 1_000, Some(9), Some(20)).await;
        assert_eq!((report.fired, report.deferred), (0, 1), "{report:?}");
        let wrote = port.wrote();
        assert_eq!(wrote.len(), 1);
        assert!(wrote[0].contains("due_seen_seq: :seq"), "{}", wrote[0]);
        assert!(!wrote[0].contains("watch_fire"));

        // Seen at 9, and the Brain has read through 8: still not silence.
        let port = FakeKip::new([(DUE_SCAN, vec![Response::ok(json!([prose(Some(9))]))])]);
        let report = sweep_watches(&port, 1_000, Some(12), Some(8)).await;
        assert_eq!((report.fired, report.deferred), (0, 1), "{report:?}");
        assert!(port.wrote().is_empty());

        // The Brain's consumption reached the coordinate: silence is a fact.
        let port = FakeKip::new([
            (DUE_SCAN, vec![Response::ok(json!([prose(Some(9))]))]),
            ("watch_fire", vec![changed("lifecycle", 1)]),
        ]);
        let report = sweep_watches(&port, 1_000, Some(12), Some(9)).await;
        assert_eq!((report.fired, report.deferred), (1, 0), "{report:?}");
        let wrote = port.wrote();
        assert_eq!(wrote.len(), 1);
        assert!(wrote[0].contains("watch_fire"), "{}", wrote[0]);
    }

    #[tokio::test]
    async fn a_due_watch_is_fired_once_and_a_conflicted_one_stays_armed() {
        let due = |id: &str| {
            watch_row(
                id,
                json!({
                    "watch_class": "silence",
                    "condition": "no reply",
                    "due_at": "2026-01-01T00:00:00Z",
                    "due_seen_seq": 5
                }),
                2,
            )
        };
        let port = FakeKip::new([
            (DUE_SCAN, vec![Response::ok(json!([due("W-1"), due("W-2")]))]),
            ("watch_fire", vec![changed("lifecycle", 1), conflicted()]),
        ]);
        let report = sweep_watches(&port, 1_000, Some(9), Some(5)).await;

        assert_eq!(report.fired, 1);
        // A Watch the maintenance model moved between scan and write stays
        // armed rather than being reported as fired.
        assert_eq!(report.conflicted, 1);
        assert!(report.error.is_none());
        // Two scans, then one write per due Watch — never a write per page.
        assert_eq!(port.wrote().len(), 2);
    }

    #[tokio::test]
    async fn a_watch_scan_that_fails_writes_nothing() {
        let port = FakeKip::new([("Watch", vec![failed("engine busy")])]);
        let report = sweep_watches(&port, 1_000, Some(9), Some(9)).await;

        assert_eq!(report.fired, 0);
        assert!(report.error.unwrap().contains("engine busy"));
        assert!(port.wrote().is_empty());
    }

    /// One Change Envelope holding one entry.
    fn envelope(seq: u64, entry: Json) -> Json {
        json!({"space_seq": seq, "changes": [entry]})
    }

    #[tokio::test]
    async fn a_structured_delta_watch_fires_on_the_change_it_watches() {
        let armed = watch_row(
            "W-1",
            json!({
                "watch_class": "delta",
                "condition": {"element": "C-42", "ops": ["update"]},
                "due_at": ""
            }),
            10,
        );
        let port = FakeKip::new([
            (ARMED_SCAN, vec![Response::ok(json!([armed]))]),
            (
                "CHANGES AFTER SEQ",
                vec![Response::ok(json!([
                    // Committed before the Watch was armed: not its change.
                    envelope(9, json!({"op": "update", "kind": "concept", "id": "C-42"})),
                    // The arming itself, which is where evaluation starts.
                    envelope(10, json!({"op": "create", "kind": "concept", "id": "W-1"})),
                    envelope(11, json!({"op": "create", "kind": "concept", "id": "C-42"})),
                    envelope(12, json!({"op": "update", "kind": "concept", "id": "C-42"})),
                ]))],
            ),
            ("watch_fire", vec![changed("lifecycle", 1)]),
        ]);
        let report = sweep_watches(&port, 1_000, Some(12), None).await;

        assert_eq!(report.fired, 1, "{report:?}");
        let seen = port.seen();
        // The stream is read, and the Watch is matched only past its arming.
        assert!(seen.iter().any(|(command, _)| command.contains("CHANGES AFTER SEQ")));
        let wrote = port.wrote();
        assert_eq!(wrote.len(), 1);
        assert!(wrote[0].contains("watch_fire"), "{}", wrote[0]);
        assert!(wrote[0].contains("matched_seq: :matched_seq"), "{}", wrote[0]);
    }

    #[tokio::test]
    async fn a_structured_silence_watch_stands_down_on_a_match_and_fires_on_none() {
        let silence = |from: u64| {
            watch_row(
                "W-2",
                json!({
                    "watch_class": "silence",
                    "condition": {"slot": {"subject": "C-1", "predicate": "replied_about"}},
                    "due_at": "2026-01-01T00:00:00Z"
                }),
                from,
            )
        };
        // The awaited reply arrived: the Watch stands down without firing.
        let port = FakeKip::new([
            (ARMED_SCAN, vec![Response::ok(json!([silence(10)]))]),
            (DUE_SCAN, vec![Response::ok(json!([silence(10)]))]),
            ("?p (:subject", vec![Response::ok(json!(["P-11"]))]),
            (
                "CHANGES AFTER SEQ",
                vec![Response::ok(json!([envelope(
                    11,
                    json!({"op": "create", "kind": "assertion", "id": "A-3", "refs": {"proposition": "P-11"}})
                )]))],
            ),
            ("disarmed", vec![changed("update", 1)]),
        ]);
        let report = sweep_watches(&port, 1_000, Some(12), None).await;
        assert_eq!((report.fired, report.disarmed), (0, 1), "{report:?}");
        let wrote = port.wrote();
        // One write: the due scan does not settle a Watch evaluation moved.
        assert_eq!(wrote.len(), 1, "{wrote:?}");
        assert!(wrote[0].contains(r#"status: "disarmed""#), "{}", wrote[0]);

        // Nothing matched through the head: silence, concluded over a
        // consumed stream, fires in the same sweep.
        let port = FakeKip::new([
            (ARMED_SCAN, vec![Response::ok(json!([silence(10)]))]),
            (DUE_SCAN, vec![Response::ok(json!([silence(10)]))]),
            ("CHANGES AFTER SEQ", vec![Response::ok(json!([]))]),
            ("watch_fire", vec![changed("lifecycle", 1)]),
        ]);
        // Past the deadline: the sweep decides `is_due` itself, on the clock
        // it is handed, and a Watch due in 2026 is not due in 1970.
        let report = sweep_watches(&port, 1_790_000_000_000, Some(12), None).await;
        assert_eq!((report.fired, report.deferred), (1, 0), "{report:?}");
        let wrote = port.wrote();
        assert_eq!(wrote.len(), 1, "{wrote:?}");
        assert!(wrote[0].contains("evaluated_seq: :evaluated_seq"), "{}", wrote[0]);
    }

    #[tokio::test]
    async fn a_structured_watch_with_nothing_new_records_where_it_read_to() {
        let armed = watch_row(
            "W-3",
            json!({"watch_class": "delta", "condition": {"type": "Commitment"}}),
            10,
        );
        // A full page, then the budget: the Watch is stamped with what was
        // read, not with the head it never reached.
        let full_page: Vec<Json> = (11..=(10 + watch::CHANGES_PAGE_LIMIT as u64))
            .map(|seq| envelope(seq, json!({"op": "update", "kind": "assertion", "id": "A-1"})))
            .collect();
        let port = FakeKip::new([
            (ARMED_SCAN, vec![Response::ok(json!([armed]))]),
            (
                "CHANGES AFTER SEQ",
                std::iter::repeat_n(Response::ok(json!(full_page)), watch::CHANGES_MAX_PAGES)
                    .collect(),
            ),
        ]);
        let report = sweep_watches(&port, 1_000, Some(5_000), None).await;
        assert_eq!(report.fired, 0, "{report:?}");
        let wrote = port.wrote();
        assert_eq!(wrote.len(), 1, "{wrote:?}");
        assert!(wrote[0].contains("evaluated_seq: :seq"), "{}", wrote[0]);
        assert_eq!(
            port.seen()
                .iter()
                .filter(|(command, _)| command.contains("CHANGES AFTER SEQ"))
                .count(),
            watch::CHANGES_MAX_PAGES
        );
    }

    #[tokio::test]
    async fn revised_roots_carry_what_each_superseded_claim_fed() {
        let rows = vec![
            SupersededRow {
                assertion: "A-3".into(),
                space_seq: 40,
                actor: Some("C-7".into()),
                proposition: Some("P-11".into()),
                superseded_by: vec!["A-9".into()],
            },
            SupersededRow {
                assertion: "A-4".into(),
                space_seq: 41,
                actor: None,
                proposition: None,
                superseded_by: vec![],
            },
        ];
        let port = FakeKip::new([(
            "LIST DEPENDENTS",
            vec![
                Response::ok(json!([
                    {"id": "C-30", "kind": "concept", "distance": 1, "via": {"activity": "ACT-5"}},
                    {"id": "C-31", "kind": "concept", "distance": 2, "via": {"activity": "ACT-6"}}
                ])),
                failed("engine busy"),
            ],
        )]);
        let roots = revised_roots(&port, &rows).await;

        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].assertion, "A-3");
        assert_eq!(roots[0].superseded_by, vec!["A-9".to_string()]);
        assert_eq!(roots[0].dependents.len(), 2);
        assert_eq!(roots[0].dependents[0].id, "C-30");
        assert_eq!(roots[0].dependents[0].via.as_deref(), Some("ACT-5"));
        assert!(!roots[0].truncated);
        // A walk that failed says the list is not to be trusted, rather than
        // reporting a root with no derivations.
        assert!(roots[1].dependents.is_empty());
        assert!(roots[1].truncated);
        // Reads only: a derivation walk changes nothing.
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

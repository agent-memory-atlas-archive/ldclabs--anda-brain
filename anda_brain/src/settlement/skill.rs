//! The Skill lifecycle verdict: `proposed → trialed → adopted → revoked`.
//!
//! Profile §14 rule 1 is unusually specific about who runs this:
//!
//! > Promotion and demotion MUST be executed by deterministic code reading
//! > graded Outcome Evidence — not author assertion, not decay, not the acting
//! > model's judgment. **The Brain proposes, compiles, and narrates; it never
//! > promotes.**
//!
//! So the lifecycle lives here, in code, and not in a prompt. The maintenance
//! model compiles a `proposed` Skill from contrastive Experience and attaches
//! the `task_family` that can grade it; from that point the stream decides.
//! Nothing a model says about how well its own procedure worked moves a Skill:
//! that account is `agent_statement`, and the only currency this module spends
//! is Outcome Evidence written by instrumentation.
//!
//! # What counts
//!
//! Sharing a `task_family` is not being graded by it. Profile §14 rule 7 —
//! *attribution before counting* — makes the treatment set the outcomes
//! **linked** to a decision that applied this Skill: the instrument's
//! `outcome_observation` Activity names an `action_gate` Activity among its
//! `inputs`, and that gate names the Skill among its own. An outcome that
//! merely landed in the same family belongs to the **baseline** and to nothing
//! else, which is what keeps two Skills in one family from grading each other.
//!
//! Grading therefore reads `Evidence {evidence_class: "outcome"}` carrying an
//! `OutcomeRecord` facet whose `task_family` matches the Skill's *and* which is
//! reachable through that two-hop link. The Profile (§14 rule 5) asks for four
//! grades; `OutcomeRecord.outcome_status` supplies five, and this deployment
//! maps them:
//!
//! ```text
//! success   → matching-condition success
//! failure   → matching-condition failure   (magnitude ≥ HIGH_SEVERITY = severe)
//! partial   → graded, but neither a success nor a failure
//! aborted   → non-matching: the run never reached the procedure
//! unknown   → not graded at all
//! ```
//!
//! `aborted` deliberately does not count against the Skill. Rule 5: a failure
//! under non-matching conditions "narrows applicability without penalizing the
//! procedure" — a deploy that never started is not evidence the recipe is
//! wrong.
//!
//! # The rule
//!
//! Adoption is **comparative** (rule 2): the question is *did things go better
//! than they were going*, not *did things go well*. When a trial opens, the
//! opening verdict writes `TrialState` (§6.5): the `space_seq` it opened at,
//! the family's tallies *excluding* this Skill's own linked outcomes up to that
//! coordinate, the quota of linked outcomes a decision needs, and the rule that
//! will decide. The later verdict compares the Skill's linked rate against that
//! recorded baseline, so an auditor can recompute it from state alone.
//!
//! Adoption is **provisional** (rule 4): an adopted Skill stays subscribed, and
//! a rate that falls back below its basis demotes it to a new trial.
//!
//! Revocation is **never harder than adoption** (rule 3): the two bars are the
//! same margin in opposite directions, plus one asymmetry the Profile grants
//! explicitly — a single severe matching-condition failure may revoke. "A
//! lifecycle that can only acquire cannot tell a habit from a superstition."
//!
//! Every transition commits as one `lifecycle_verdict` Activity plus one
//! guarded `UPDATE`, and the Activity carries the rule identity and the outcome
//! window while the Skill's `TrialState` carries the basis, so the verdict is
//! recomputable rather than merely asserted.
//!
//! # Where the numbers live
//!
//! Three Profile facets, and the split is the point (§6.1, §6.2, §6.5):
//!
//! ```text
//! GradingState        what happened: tallies of linked graded outcomes
//! TrialState          what it is measured against: the recorded baseline
//! MnemonicState.utility   the bet on what will happen, revised by verdicts
//! ```
//!
//! `GradingState` is not utility and utility is not authority. The 2.0 draft
//! carried all three in one `SkillUtility` facet; splitting them is what stops
//! a tally from reading as a forecast.

use serde_json::{Map, Value as Json};

use crate::kip;

/// The identity of the rule below, recorded on every verdict.
///
/// Rule 2 requires a verdict to make "rule identity, comparison basis, and the
/// graded outcome set recoverable". A verdict recorded without saying which
/// rule produced it cannot be recomputed later, only believed — and the whole
/// point of taking this away from the model is that it should not have to be
/// believed. Bump this whenever the thresholds or the tally mapping change.
pub const VERDICT_RULE: &str = "anda-brain/skill-verdict@2";

/// Graded outcomes a trial needs before any verdict may move it.
///
/// Rule: "no single success suffices". Five is small enough that a Skill can
/// earn its standing inside one working period and large enough that a lucky
/// pair cannot promote a coin flip.
const TRIAL_MIN_OUTCOMES: u64 = 5;

/// How far the trial's rate must beat (or trail) its basis.
///
/// One margin, used in both directions — that is rule 3 in arithmetic. Making
/// this asymmetric is exactly how a lifecycle acquires habits it cannot drop.
const VERDICT_MARGIN: f64 = 0.1;

/// `OutcomeRecord.magnitude` at or above which one matching-condition failure
/// may revoke on its own (rule: "one high-severity matching-condition failure
/// MAY suffice"). Below it, a failure is one tally against the Skill like any
/// other.
const HIGH_SEVERITY: f64 = 0.8;

/// How many Outcome Evidence records one verdict pass reads per Skill.
const OUTCOME_WINDOW: usize = 200;

/// How many Skills one pass evaluates.
const SKILL_SCAN_LIMIT: usize = 50;

/// The deployment-local attribute this module keeps on a Skill.
///
/// `Skill.attributes` is `open`, and this is the one piece of bookkeeping no
/// Profile facet has a slot for: the highest outcome coordinate already counted
/// into the tallies, which is what keeps a replayed pass from counting a run
/// twice. `TrialState.basis_seq` is a different number — where the *trial*
/// opened — so it cannot stand in for this one.
const CURSOR_ATTR: &str = "verdict_cursor";

/// The lifecycle states, as the Profile enumerates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillStatus {
    Proposed,
    Trialed,
    Adopted,
    Revoked,
}

impl SkillStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "proposed" => Some(Self::Proposed),
            "trialed" => Some(Self::Trialed),
            "adopted" => Some(Self::Adopted),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Trialed => "trialed",
            Self::Adopted => "adopted",
            Self::Revoked => "revoked",
        }
    }
}

/// One Skill as the scan returns it, with what the rule needs to judge it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SkillRow {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub status: SkillStatus,
    pub task_family: String,
    /// Highest Outcome Evidence `space_seq` already counted into the tallies.
    pub cursor: u64,
    /// `TrialState` (§6.5) — the basis an open trial is measured against.
    ///
    /// `None` for a Skill no trial has ever opened on, and for one whose
    /// facet is missing the members the comparison needs.
    pub trial: Option<TrialBasis>,
    /// `GradingState` (§6.2) — linked graded outcomes only.
    pub success_count: u64,
    pub failure_count: u64,
    pub graded_count: u64,
}

/// The recorded comparison basis of an open trial (Profile §6.5).
///
/// The baseline is stored as tallies rather than as the rate they imply. A rate
/// is derived, and a verdict whose basis was only ever a rounded number cannot
/// be recomputed — rule 2 asks for the comparison to be recoverable, not
/// merely rememberable.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TrialBasis {
    /// The `space_seq` the trial opened at.
    pub basis_seq: u64,
    /// The family's outcomes up to `basis_seq` that were *not* linked to this
    /// Skill — the rest of the stream, which is what "how things were going"
    /// means.
    pub baseline_success: u64,
    pub baseline_failure: u64,
    pub baseline_graded: u64,
    /// Linked graded outcomes the rule needs before it will decide.
    pub quota: u64,
}

impl TrialBasis {
    /// The rate the family was running at without this Skill, or `None` when
    /// nothing in the baseline came out one way or the other.
    ///
    /// An empty baseline is an honest `None`, not a zero: a family this Skill
    /// is the first to be tried in has no "how things were going", and reading
    /// that as 0.0 would let any success at all clear the bar.
    pub fn rate(&self) -> Option<f64> {
        let decided = self.baseline_success + self.baseline_failure;
        (decided > 0).then(|| self.baseline_success as f64 / decided as f64)
    }
}

/// The whole family's graded outcomes up to a coordinate, linked or not.
///
/// Read as one grouped aggregate rather than as a window of rows: the baseline
/// is a count over a stream that may be far larger than any page this pass
/// would read, and a baseline truncated by a `LIMIT` would quietly become a
/// comparison against the most recent few runs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct FamilyTally {
    pub success: u64,
    pub failure: u64,
    pub graded: u64,
}

/// A tally over one window of Outcome Evidence.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Tally {
    pub success: u64,
    pub failure: u64,
    /// Everything the instruments graded — success, partial, failure, aborted.
    /// `unknown` is not a grade.
    pub graded: u64,
    /// A matching-condition failure severe enough to revoke on its own.
    pub severe_failure: bool,
    /// The highest `space_seq` seen, which becomes the next cursor.
    pub cursor: u64,
    /// The Evidence this window graded, which the verdict Activity cites as
    /// its `inputs` (Profile §12: "inputs: the graded Outcome Evidence").
    pub evidence: Vec<String>,
}

impl Tally {
    /// The share of decided runs that succeeded, or `None` when nothing was
    /// decided either way.
    ///
    /// `partial` and `aborted` are in `graded` but not in this denominator: a
    /// rate is about runs that came out one way or the other, and counting a
    /// partial as half a failure would be the rule inventing a grade the
    /// instruments did not report.
    pub fn success_rate(&self) -> Option<f64> {
        let decided = self.success + self.failure;
        (decided > 0).then(|| self.success as f64 / decided as f64)
    }
}

/// What the rule decided about one Skill.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Verdict {
    /// `None` when the Skill stays where it is and only its tallies move.
    pub transition: Option<SkillStatus>,
    /// Why, in one auditable line recorded on the Activity.
    pub rationale: String,
    /// The `TrialState` to write when this verdict opens (or re-opens) a trial.
    pub trial: Option<TrialBasis>,
}

/// The deterministic rule. No model, no clock, no author assertion.
///
/// Returns `None` when nothing at all happened — no new *linked* graded
/// outcome — so a pass over an idle Space, or over a family whose runs nobody
/// attributed to this Skill, writes nothing.
///
/// `family` is the whole stream up to the window's end, and it is read only to
/// build a baseline when a trial opens: the baseline is the family minus this
/// Skill's own linked outcomes (§6.5).
pub(crate) fn decide(skill: &SkillRow, window: &Tally, family: &FamilyTally) -> Option<Verdict> {
    if window.graded == 0 {
        return None;
    }

    // Tallies accumulate across passes; the verdict reads the whole trial, not
    // just the newest window.
    let total = Tally {
        success: skill.success_count + window.success,
        failure: skill.failure_count + window.failure,
        graded: skill.graded_count + window.graded,
        severe_failure: window.severe_failure,
        cursor: window.cursor,
        evidence: Vec::new(),
    };
    let rate = total.success_rate();
    // Only meaningful where a trial opens; built here so every arm that opens
    // one records the same baseline for the same window.
    let opening = TrialBasis {
        basis_seq: window.cursor,
        baseline_success: family.success.saturating_sub(total.success),
        baseline_failure: family.failure.saturating_sub(total.failure),
        baseline_graded: family.graded.saturating_sub(total.graded),
        quota: TRIAL_MIN_OUTCOMES,
    };

    match skill.status {
        // A trial opens as soon as an attributed outcome arrives, and records
        // what the rest of the family was already running at. That recorded
        // baseline is what makes the later verdict comparative rather than
        // absolute (rule 2).
        SkillStatus::Proposed => Some(Verdict {
            transition: Some(SkillStatus::Trialed),
            rationale: format!(
                "trial opened on {} linked graded outcome(s) under `{}`; baseline {} over {} \
                 unlinked outcome(s)",
                window.graded,
                skill.task_family,
                describe_rate(opening.rate()),
                opening.baseline_graded,
            ),
            trial: Some(opening),
        }),

        SkillStatus::Trialed => {
            // The Profile's one sanctioned asymmetry, and it favours demotion.
            if window.severe_failure {
                return Some(Verdict {
                    transition: Some(SkillStatus::Revoked),
                    rationale: format!(
                        "revoked on a high-severity matching-condition failure \
                         (magnitude ≥ {HIGH_SEVERITY}) under `{}`",
                        skill.task_family
                    ),
                    trial: None,
                });
            }
            let trial = skill.trial.unwrap_or(opening);
            let basis = trial.rate().unwrap_or(0.0);
            let (Some(rate), true) = (rate, total.graded >= trial.quota.max(1)) else {
                return Some(tally_only(&total, skill, "trial still gathering outcomes"));
            };
            if rate >= basis + VERDICT_MARGIN {
                Some(Verdict {
                    transition: Some(SkillStatus::Adopted),
                    rationale: format!(
                        "adopted: {rate:.3} over {} linked graded outcome(s) beats the recorded \
                         baseline {basis:.3} by at least {VERDICT_MARGIN}",
                        total.graded
                    ),
                    trial: None,
                })
            } else if rate <= basis - VERDICT_MARGIN {
                Some(Verdict {
                    transition: Some(SkillStatus::Revoked),
                    rationale: format!(
                        "revoked: {rate:.3} over {} linked graded outcome(s) trails the recorded \
                         baseline {basis:.3} by at least {VERDICT_MARGIN}",
                        total.graded
                    ),
                    trial: None,
                })
            } else {
                Some(tally_only(
                    &total,
                    skill,
                    "trial inconclusive against its baseline",
                ))
            }
        }

        // Adoption is provisional: the stream keeps grading, and a rate that
        // falls back demotes to a new trial rather than to nothing (rule 4).
        // The re-trial writes a fresh `TrialState`, because the baseline it
        // will be judged against is the family as it stands now.
        SkillStatus::Adopted => {
            if window.severe_failure {
                return Some(Verdict {
                    transition: Some(SkillStatus::Revoked),
                    rationale: format!(
                        "revoked: a high-severity matching-condition failure under `{}` \
                         does not wait for a re-verdict",
                        skill.task_family
                    ),
                    trial: None,
                });
            }
            let trial = skill.trial.unwrap_or(opening);
            let basis = trial.rate().unwrap_or(0.0);
            match rate {
                Some(rate) if total.graded >= trial.quota.max(1) && rate < basis => Some(Verdict {
                    transition: Some(SkillStatus::Trialed),
                    rationale: format!(
                        "demoted to re-trial: {rate:.3} has fallen back to its \
                             pre-adoption baseline {basis:.3}"
                    ),
                    trial: Some(opening),
                }),
                _ => Some(tally_only(&total, skill, "adoption still holding")),
            }
        }

        // Re-entry starts a new trial; nothing resurrects silently. It takes
        // fresh evidence, which is what a new linked graded outcome is.
        SkillStatus::Revoked => Some(Verdict {
            transition: Some(SkillStatus::Trialed),
            rationale: format!(
                "re-entry: {} new linked graded outcome(s) under `{}` open a fresh trial",
                window.graded, skill.task_family
            ),
            trial: Some(opening),
        }),
    }
}

/// A rate as a rationale spells it, or `none` when nothing decided it.
fn describe_rate(rate: Option<f64>) -> String {
    rate.map_or("none".to_string(), |rate| format!("{rate:.3}"))
}

/// A verdict that moves the tallies and the cursor but not the standing.
fn tally_only(total: &Tally, skill: &SkillRow, why: &str) -> Verdict {
    Verdict {
        transition: None,
        rationale: format!(
            "{why}: {} success / {} failure over {} linked graded under `{}`",
            total.success, total.failure, total.graded, skill.task_family
        ),
        trial: None,
    }
}

/// The Skills this Space holds, with the state the rule needs.
pub(crate) fn skills_request() -> anda_kip::Request {
    // Each facet is projected by name rather than as the whole `facets` object:
    // a projected `?s.facets` comes back keyed by full schema ref
    // (`kip://profiles/cognitive-memory@2.0.0/GradingState`), and a reader that
    // looked up the local name would silently find nothing and grade every
    // Skill from zero. Naming them lets the engine resolve the symbols.
    kip::request(format!(
        r#"FIND(?s.id, ?s.name, ?s.attributes, ?s.facets["GradingState"], ?s.facets["TrialState"], ?s._system.plane_versions.attributes)
WHERE {{ ?s CONCEPT {{type: "Skill"}} }}
LIMIT {SKILL_SCAN_LIMIT}"#
    ))
}

/// Reads the Skill scan.
///
/// A Skill without a `task_family` is skipped: the Profile requires
/// consolidation to attach one and refuses to emit a Skill without it, so one
/// that arrived anyway names no stream that could grade it and this rule has
/// nothing to read. It is left alone rather than judged on no evidence.
pub(crate) fn skill_rows(result: &Json) -> Vec<SkillRow> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let columns = row.as_array()?;
            let attributes = columns.get(2);
            let attribute = |name: &str| attributes.and_then(|a| a.get(name));
            let grading = columns.get(3);
            let tally = |name: &str| {
                grading
                    .and_then(|g| g.get(name))
                    .and_then(Json::as_u64)
                    .unwrap_or(0)
            };
            let trial = columns.get(4).and_then(read_trial_state);
            let task_family = attribute("task_family")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            if task_family.is_empty() {
                return None;
            }
            Some(SkillRow {
                id: columns.first().and_then(Json::as_str)?.to_string(),
                name: columns
                    .get(1)
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                version: columns.get(5).and_then(Json::as_u64)?,
                status: SkillStatus::parse(
                    attribute("status")
                        .and_then(Json::as_str)
                        .unwrap_or_default(),
                )?,
                task_family,
                cursor: attribute(CURSOR_ATTR).and_then(Json::as_u64).unwrap_or(0),
                trial,
                success_count: tally("success_count"),
                failure_count: tally("failure_count"),
                graded_count: tally("graded_count"),
            })
        })
        .collect()
}

/// Reads a `TrialState` facet into the basis a verdict compares against.
///
/// `basis_seq` is the one member the Profile makes required, so a facet
/// without it is a trial nobody opened and answers `None` — the next verdict
/// then opens one properly instead of judging against zeroes it invented.
fn read_trial_state(facet: &Json) -> Option<TrialBasis> {
    let count = |name: &str| facet.get(name).and_then(Json::as_u64).unwrap_or(0);
    Some(TrialBasis {
        basis_seq: facet.get("basis_seq").and_then(Json::as_u64)?,
        baseline_success: count("baseline_success_count"),
        baseline_failure: count("baseline_failure_count"),
        baseline_graded: count("baseline_graded_count"),
        quota: facet
            .get("quota")
            .and_then(Json::as_u64)
            .unwrap_or(TRIAL_MIN_OUTCOMES),
    })
}

/// The Outcome Evidence **linked to a decision that applied this Skill** and
/// not yet counted — the treatment set (Profile §8.1, §14 rule 7).
///
/// The two hops are the attribution, and neither is optional. An instrument
/// writes an `outcome_observation` Activity naming the `action_gate` decision
/// it observed among its `inputs` and the Outcome Evidence among its
/// `outputs`; the gate names the Skill it applied among its own `inputs`. A
/// query that joined on `task_family` alone would let one Skill be promoted by
/// another Skill's runs, which is the failure rule 7 exists to name.
///
/// The family filter stays on top of the link so the treatment set and the
/// baseline are drawn from one stream: comparing a Skill's runs against a
/// different family's would answer a question nobody asked.
///
/// The cursor is a Space sequence coordinate, not a timestamp: it is the same
/// monotonic counter the engine stamps on every element, so "the outcomes that
/// landed since I last judged" is exact. Without it a replayed pass would
/// count the same run twice and promote on arithmetic rather than evidence.
pub(crate) fn outcomes_request(skill: &str, task_family: &str, after: u64) -> anda_kip::Request {
    kip::request_with(
        format!(
            r#"FIND(?e.id, ?e._system.space_seq, ?e.facets["OutcomeRecord"])
WHERE {{
  ?gate ACTIVITY {{activity_class: "action_gate"}}
  STRUCTURAL (?gate, "inputs", :skill)
  ?obs ACTIVITY {{activity_class: "outcome_observation"}}
  STRUCTURAL (?obs, "inputs", ?gate)
  ?e EVIDENCE {{evidence_class: "outcome"}}
  STRUCTURAL (?obs, "outputs", ?e)
  FILTER(?e.facets["OutcomeRecord"].task_family == :family)
  FILTER(?e._system.space_seq > :after)
}}
ORDER BY ?e._system.space_seq
LIMIT {OUTCOME_WINDOW}"#
        ),
        Map::from_iter([
            ("skill".to_string(), Json::from(skill)),
            ("family".to_string(), Json::from(task_family)),
            ("after".to_string(), Json::from(after)),
        ]),
    )
}

/// The whole family's graded outcomes up to a coordinate — the stream a trial's
/// baseline is drawn from (Profile §6.5).
///
/// One grouped aggregate (§44.6) rather than a page of rows: the baseline is a
/// count, the family may hold far more outcomes than one window, and a
/// truncated baseline would silently become "the most recent two hundred runs"
/// while still calling itself how things were going.
///
/// It counts the linked outcomes too; the caller subtracts this Skill's own,
/// which is cheaper and more exact than asking the engine for a negation.
pub(crate) fn family_tally_request(task_family: &str, upto: u64) -> anda_kip::Request {
    kip::request_with(
        r#"FIND(?e.facets["OutcomeRecord"].outcome_status, COUNT(?e))
WHERE {
  ?e EVIDENCE {evidence_class: "outcome"}
  FILTER(?e.facets["OutcomeRecord"].task_family == :family)
  FILTER(?e._system.space_seq <= :upto)
}"#,
        Map::from_iter([
            ("family".to_string(), Json::from(task_family)),
            ("upto".to_string(), Json::from(upto)),
        ]),
    )
}

/// Reads the grouped family aggregate.
///
/// `unknown` is not a grade, so it is counted nowhere — the same rule the
/// window tally applies, because a baseline graded on a different vocabulary
/// than the treatment set is not a comparison.
pub(crate) fn family_tally(result: &Json) -> FamilyTally {
    let Some(rows) = result.as_array() else {
        return FamilyTally::default();
    };
    let mut family = FamilyTally::default();
    for row in rows {
        let Some(columns) = row.as_array() else {
            continue;
        };
        let status = columns.first().and_then(Json::as_str).unwrap_or("unknown");
        let count = columns.get(1).and_then(Json::as_u64).unwrap_or(0);
        match status {
            "success" => {
                family.success += count;
                family.graded += count;
            }
            "failure" => {
                family.failure += count;
                family.graded += count;
            }
            "partial" | "aborted" => family.graded += count,
            _ => {}
        }
    }
    family
}

/// Grades one window of Outcome Evidence.
pub(crate) fn tally(result: &Json) -> Tally {
    let Some(rows) = result.as_array() else {
        return Tally::default();
    };
    let mut tally = Tally::default();
    for row in rows {
        let Some(columns) = row.as_array() else {
            continue;
        };
        let Some(id) = columns.first().and_then(Json::as_str) else {
            continue;
        };
        let Some(seq) = columns.get(1).and_then(Json::as_u64) else {
            continue;
        };
        let record = columns.get(2);
        let status = record
            .and_then(|record| record.get("outcome_status"))
            .and_then(Json::as_str)
            .unwrap_or("unknown");
        let magnitude = record
            .and_then(|record| record.get("magnitude"))
            .and_then(Json::as_f64);

        tally.cursor = tally.cursor.max(seq);
        if status != "unknown" {
            tally.evidence.push(id.to_string());
        }
        match status {
            "success" => {
                tally.success += 1;
                tally.graded += 1;
            }
            "failure" => {
                tally.failure += 1;
                tally.graded += 1;
                if magnitude.is_some_and(|magnitude| magnitude >= HIGH_SEVERITY) {
                    tally.severe_failure = true;
                }
            }
            // Graded, but not a verdict either way.
            "partial" => tally.graded += 1,
            // Non-matching conditions: the run never reached the procedure, so
            // it narrows applicability rather than penalizing the recipe.
            "aborted" => tally.graded += 1,
            // Not a grade at all.
            _ => {}
        }
    }
    tally
}

/// Writes one verdict: the guarded transition and its provenance, atomically.
///
/// The Activity records the rule that ran and the sequence window it read, and
/// the Skill's `TrialState` records the basis, because rule 2 requires the
/// verdict to be recomputable rather than merely asserted. `EXPECT VERSION ...
/// OF ATTRIBUTES` keeps the write safe beside a maintenance model that may be
/// revising the same Skill's applicability or failure modes. The guard names
/// the attributes plane rather than the element (§35.1) because that is the
/// plane the verdict competes for: the facets it also writes are its own, and a
/// `MnemonicState.memory_strength` sweep over the same Skill is not a reason to
/// withhold a verdict the outcome stream already decided.
///
/// Three facets, three jobs (§6.1, §6.2, §6.5): `GradingState` takes the
/// tallies of what happened, `MnemonicState.utility` takes the revised bet on
/// what will, and `TrialState` — only when this verdict opens a trial — takes
/// what the next one will measure against.
pub(crate) fn verdict_request(
    skill: &SkillRow,
    verdict: &Verdict,
    window: &Tally,
    now: &str,
) -> anda_kip::Request {
    let status = verdict
        .transition
        .map_or(skill.status, |transition| transition);
    let success = skill.success_count + window.success;
    let failure = skill.failure_count + window.failure;
    let graded = skill.graded_count + window.graded;
    // The revised admission bet: the observed share of decided linked runs that
    // worked. Procedural standing, never truth and never permission — and
    // deliberately the same arithmetic the tallies report, so the bet a reader
    // sees is one they can check against the record beside it.
    let utility = Tally {
        success,
        failure,
        ..Default::default()
    }
    .success_rate()
    .unwrap_or(0.0);

    // Rule identity and the window, pinned where the Profile says to pin them.
    // The sequence window is what makes the verdict recomputable: an auditor
    // re-reads this Skill's linked Outcome Evidence in `(from, to]` and re-runs
    // the named rule against the `TrialState` it finds on the Skill.
    let basis = verdict.trial.or(skill.trial);
    let digest = format!(
        "rule={VERDICT_RULE} family={} basis_seq={} baseline={} window=({},{}] \
         tally={}s/{}f/{}g verdict={}→{}",
        skill.task_family,
        basis.map_or("none".to_string(), |trial| trial.basis_seq.to_string()),
        basis.map_or("none".to_string(), |trial| describe_rate(trial.rate())),
        skill.cursor,
        window.cursor,
        success,
        failure,
        graded,
        skill.status.as_str(),
        status.as_str(),
    );

    let mut parameters = Map::from_iter([
        ("skill".to_string(), Json::from(skill.id.as_str())),
        ("version".to_string(), Json::from(skill.version)),
        ("status".to_string(), Json::from(status.as_str())),
        ("cursor".to_string(), Json::from(window.cursor)),
        ("utility".to_string(), Json::from(utility)),
        ("success".to_string(), Json::from(success)),
        ("failure".to_string(), Json::from(failure)),
        ("graded".to_string(), Json::from(graded)),
        ("now".to_string(), Json::from(now)),
        ("digest".to_string(), Json::from(digest)),
    ]);
    // `TrialState` is rewritten only by a verdict that opens a trial (§6.5); a
    // verdict that decides one leaves the basis it was decided against
    // standing, so the decision stays checkable after the fact.
    let trial_state = match verdict.trial {
        Some(trial) => {
            parameters.insert("basis_seq".to_string(), Json::from(trial.basis_seq));
            parameters.insert("b_success".to_string(), Json::from(trial.baseline_success));
            parameters.insert("b_failure".to_string(), Json::from(trial.baseline_failure));
            parameters.insert("b_graded".to_string(), Json::from(trial.baseline_graded));
            parameters.insert("quota".to_string(), Json::from(trial.quota));
            parameters.insert("rule".to_string(), Json::from(VERDICT_RULE));
            r#"
    SET FACET "TrialState" {
      opened_at: :now,
      basis_seq: :basis_seq,
      baseline_success_count: :b_success,
      baseline_failure_count: :b_failure,
      baseline_graded_count: :b_graded,
      quota: :quota,
      rule_id: :rule
    }"#
        }
        None => "",
    };

    // `inputs: the linked Outcome Evidence it graded; outputs: the Skill whose
    // lifecycle it moved` (Profile §9). Getting these the wrong way round would
    // make the verdict read as though the Skill caused the outcomes.
    let inputs: String = window
        .evidence
        .iter()
        .enumerate()
        .map(|(index, id)| {
            parameters.insert(format!("e{index}"), Json::from(id.as_str()));
            format!("(\"inputs\", :e{index}) ")
        })
        .collect();

    kip::request_with(
        format!(
            r#"MUTATE {{
  UPDATE :skill
    SET ATTRIBUTES {{ status: :status, {CURSOR_ATTR}: :cursor }}
    SET FACET "GradingState" {{
      success_count: :success,
      failure_count: :failure,
      graded_count: :graded,
      last_verdict_at: :now
    }}
    SET FACET "MnemonicState" {{ utility: :utility }}{trial_state}
    EXPECT VERSION :version OF ATTRIBUTES

  CREATE ACTIVITY ?verdict {{
    SET FIELDS {{
      activity_class: "lifecycle_verdict",
      status: "completed",
      parameters_digest: :digest
    }}
    SET STRUCTURAL {{ {inputs}("outputs", :skill) }}
  }}
}}"#
        ),
        parameters,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A Skill whose open trial was measured against a baseline running at
    /// `basis`, expressed as the tallies `TrialState` actually stores.
    fn skill(status: SkillStatus, basis: Option<f64>) -> SkillRow {
        SkillRow {
            id: "C-1".to_string(),
            name: "Redeploy after a schema change".to_string(),
            version: 2,
            status,
            task_family: "deploy".to_string(),
            cursor: 10,
            trial: basis.map(baseline_at),
            success_count: 0,
            failure_count: 0,
            graded_count: 0,
        }
    }

    /// A hundred baseline runs at the given rate — enough that the rate is the
    /// number and not an artefact of a small denominator.
    fn baseline_at(rate: f64) -> TrialBasis {
        let success = (rate * 100.0).round() as u64;
        TrialBasis {
            basis_seq: 10,
            baseline_success: success,
            baseline_failure: 100 - success,
            baseline_graded: 100,
            quota: TRIAL_MIN_OUTCOMES,
        }
    }

    /// A family whose whole recorded stream is this Skill's own linked runs,
    /// which makes the baseline it derives empty.
    fn only_linked(window: &Tally) -> FamilyTally {
        FamilyTally {
            success: window.success,
            failure: window.failure,
            graded: window.graded,
        }
    }

    fn window(success: u64, failure: u64) -> Tally {
        Tally {
            success,
            failure,
            graded: success + failure,
            severe_failure: false,
            cursor: 42,
            evidence: vec!["E-1".to_string()],
        }
    }

    #[test]
    fn an_actors_own_success_report_is_not_an_outcome() {
        // Only `evidence_class: "outcome"` is read, and the query says so.
        // The acting model's account of how its own action went is an
        // `agent_statement`: citable as context, never as a grade.
        let command = outcomes_request("C-1", "deploy", 0).operations[0]
            .command
            .clone()
            .unwrap();
        assert!(command.contains(r#"EVIDENCE {evidence_class: "outcome"}"#));
        assert!(command.contains(r#"OutcomeRecord"#));
        // Projected by name, so the engine resolves the symbol: a whole
        // `?e.facets` comes back keyed by full schema ref and a local-name
        // lookup would find nothing.
        assert!(command.contains(r#"?e.facets["OutcomeRecord"]"#));
    }

    #[test]
    fn grading_separates_matching_failure_from_a_run_that_never_started() {
        let graded = tally(&json!([
            ["E-1", 11, {"outcome_status": "success"}],
            ["E-2", 12, {"outcome_status": "failure"}],
            ["E-3", 13, {"outcome_status": "partial"}],
            // Non-matching conditions: narrows applicability, does not
            // penalize the procedure.
            ["E-4", 14, {"outcome_status": "aborted"}],
            // Not a grade at all.
            ["E-5", 15, {"outcome_status": "unknown"}],
        ]));
        assert_eq!(graded.success, 1);
        assert_eq!(graded.failure, 1);
        assert_eq!(graded.graded, 4, "unknown is not a grade");
        assert!(!graded.severe_failure);
        assert_eq!(graded.cursor, 15, "the cursor covers what was read");
        assert_eq!(graded.success_rate(), Some(0.5));
        // The verdict cites what it graded, and an ungraded row is not
        // something it read.
        assert_eq!(graded.evidence, ["E-1", "E-2", "E-3", "E-4"]);
    }

    #[test]
    fn a_trial_opens_with_the_basis_it_will_later_be_judged_against() {
        // Rule 2: adoption answers "did things go better than they were
        // going", so the trial has to record what "going" was — and rule 7
        // says "going" is the family *minus* this Skill's own linked runs.
        let window = window(1, 1);
        let family = FamilyTally {
            success: 21,
            failure: 41,
            graded: 62,
        };
        let verdict = decide(&skill(SkillStatus::Proposed, None), &window, &family).unwrap();
        assert_eq!(verdict.transition, Some(SkillStatus::Trialed));
        let trial = verdict
            .trial
            .expect("the opening verdict writes TrialState");
        assert_eq!(trial.basis_seq, 42, "the coordinate the trial opened at");
        assert_eq!(trial.baseline_success, 20, "the Skill's own run comes out");
        assert_eq!(trial.baseline_failure, 40);
        assert_eq!(trial.baseline_graded, 60);
        assert_eq!(trial.rate(), Some(1.0 / 3.0));
        assert_eq!(trial.quota, TRIAL_MIN_OUTCOMES);
    }

    #[test]
    fn a_family_this_skill_is_the_first_tried_in_has_no_baseline() {
        // An empty baseline is `None`, not zero. Reading it as 0.0 would make
        // any success at all clear the bar, which is adoption on absolute
        // performance — the thing rule 2 refuses.
        let window = window(2, 0);
        let verdict = decide(
            &skill(SkillStatus::Proposed, None),
            &window,
            &only_linked(&window),
        )
        .unwrap();
        let trial = verdict.trial.unwrap();
        assert_eq!(trial.baseline_graded, 0);
        assert_eq!(trial.rate(), None);
        assert!(verdict.rationale.contains("baseline none"), "{verdict:?}");
    }

    #[test]
    fn an_outcome_that_only_shares_the_family_is_baseline_and_never_a_grade() {
        // Rule 7. The treatment set comes off the two-hop link, so a family
        // with a hundred other runs in it grades this Skill on the two that
        // were attributed to it — and the rest become what it is compared
        // against.
        let command = outcomes_request("C-1", "deploy", 0).operations[0]
            .command
            .clone()
            .unwrap();
        assert!(
            command.contains(r#"?gate ACTIVITY {activity_class: "action_gate"}"#),
            "{command}"
        );
        assert!(
            command.contains(r#"STRUCTURAL (?gate, "inputs", :skill)"#),
            "{command}"
        );
        assert!(
            command.contains(r#"?obs ACTIVITY {activity_class: "outcome_observation"}"#),
            "{command}"
        );
        assert!(
            command.contains(r#"STRUCTURAL (?obs, "inputs", ?gate)"#),
            "{command}"
        );
        assert!(
            command.contains(r#"STRUCTURAL (?obs, "outputs", ?e)"#),
            "{command}"
        );

        // And the baseline is read as a count over the whole stream, not as a
        // page of it: a truncated baseline would be a comparison against the
        // most recent few runs while still calling itself the family.
        let baseline = family_tally_request("deploy", 900).operations[0]
            .command
            .clone()
            .unwrap();
        assert!(baseline.contains("COUNT(?e)"), "{baseline}");
        assert!(!baseline.contains("LIMIT"), "{baseline}");
        let counted = family_tally(&json!([
            ["success", 8],
            ["failure", 2],
            ["partial", 1],
            ["aborted", 1],
            ["unknown", 5],
        ]));
        assert_eq!(counted.success, 8);
        assert_eq!(counted.failure, 2);
        assert_eq!(counted.graded, 12, "unknown is not a grade here either");
    }

    #[test]
    fn no_single_success_promotes() {
        let window = window(1, 0);
        let verdict = decide(
            &skill(SkillStatus::Trialed, Some(0.2)),
            &window,
            &only_linked(&window),
        )
        .unwrap();
        assert_eq!(verdict.transition, None, "{verdict:?}");
        assert!(verdict.rationale.contains("gathering"), "{verdict:?}");
    }

    #[test]
    fn adoption_is_comparative_not_absolute() {
        // A 60% rate is not "good", but against a baseline of 20% it is better
        // than things were going — which is the question rule 2 asks.
        let window = window(6, 4);
        let verdict = decide(
            &skill(SkillStatus::Trialed, Some(0.2)),
            &window,
            &only_linked(&window),
        )
        .unwrap();
        assert_eq!(
            verdict.transition,
            Some(SkillStatus::Adopted),
            "{verdict:?}"
        );
        assert_eq!(
            verdict.trial, None,
            "a deciding verdict leaves the basis it was decided against standing"
        );

        // The same rate against a baseline of 80% is a regression, and the
        // same margin in the other direction revokes it.
        let verdict = decide(
            &skill(SkillStatus::Trialed, Some(0.8)),
            &window,
            &only_linked(&window),
        )
        .unwrap();
        assert_eq!(
            verdict.transition,
            Some(SkillStatus::Revoked),
            "{verdict:?}"
        );
    }

    #[test]
    fn revocation_is_never_harder_than_adoption() {
        // Rule 3, as arithmetic: the same margin, the same minimum sample,
        // mirrored. A lifecycle that can only acquire cannot tell a habit from
        // a superstition.
        let basis = 0.5;
        let up = window(8, 2);
        let down = window(2, 8);
        let promote = decide(
            &skill(SkillStatus::Trialed, Some(basis)),
            &up,
            &only_linked(&up),
        );
        let demote = decide(
            &skill(SkillStatus::Trialed, Some(basis)),
            &down,
            &only_linked(&down),
        );
        assert_eq!(promote.unwrap().transition, Some(SkillStatus::Adopted));
        assert_eq!(demote.unwrap().transition, Some(SkillStatus::Revoked));
    }

    #[test]
    fn one_severe_matching_failure_revokes_without_waiting() {
        // The Profile's one sanctioned asymmetry, and it favours demotion.
        let severe = Tally {
            failure: 1,
            graded: 1,
            severe_failure: true,
            cursor: 42,
            ..Default::default()
        };
        for status in [SkillStatus::Trialed, SkillStatus::Adopted] {
            let verdict =
                decide(&skill(status, Some(0.9)), &severe, &only_linked(&severe)).unwrap();
            assert_eq!(
                verdict.transition,
                Some(SkillStatus::Revoked),
                "{status:?} {verdict:?}"
            );
        }

        // Severity comes off the instrument's magnitude, not from a count.
        let graded = tally(&json!([
            ["E-1", 11, {"outcome_status": "failure", "magnitude": 0.9}]
        ]));
        assert!(graded.severe_failure);
        let mild = tally(&json!([
            ["E-1", 11, {"outcome_status": "failure", "magnitude": 0.1}]
        ]));
        assert!(!mild.severe_failure);
    }

    #[test]
    fn adoption_is_provisional_and_demotes_to_a_new_trial() {
        // Rule 4: an adopted Skill stays subscribed to its stream. Falling
        // back is a re-trial, not amnesty and not deletion — and the re-trial
        // records a fresh basis, because the family has moved on since the
        // first one.
        let window = window(2, 8);
        let family = FamilyTally {
            success: 32,
            failure: 18,
            graded: 50,
        };
        let verdict = decide(&skill(SkillStatus::Adopted, Some(0.8)), &window, &family).unwrap();
        assert_eq!(
            verdict.transition,
            Some(SkillStatus::Trialed),
            "{verdict:?}"
        );
        let trial = verdict
            .trial
            .expect("the re-trial writes a fresh TrialState");
        assert_eq!(trial.baseline_success, 30);
        assert_eq!(trial.baseline_failure, 10);
        assert_eq!(trial.rate(), Some(0.75));
    }

    #[test]
    fn nothing_resurrects_silently() {
        let window = window(3, 0);
        let verdict = decide(
            &skill(SkillStatus::Revoked, None),
            &window,
            &only_linked(&window),
        )
        .unwrap();
        assert_eq!(verdict.transition, Some(SkillStatus::Trialed));
        assert!(verdict.rationale.contains("fresh trial"), "{verdict:?}");
        assert!(verdict.trial.is_some(), "and it opens with a new basis");
    }

    #[test]
    fn an_idle_stream_produces_no_verdict_and_no_write() {
        assert_eq!(
            decide(
                &skill(SkillStatus::Adopted, Some(0.9)),
                &Tally::default(),
                &FamilyTally::default()
            ),
            None
        );
    }

    #[test]
    fn a_skill_with_no_task_family_is_left_alone() {
        // The Profile requires consolidation to attach one and to refuse to
        // emit a Skill without it. One that arrived anyway names no stream
        // that could prove it wrong, so this rule has nothing to read — and
        // judging it on no evidence is exactly what the lifecycle forbids.
        let rows = json!([
            ["C-1", "No family", {"status": "proposed"}, null, null, 1],
            ["C-2", "Graded", {"status": "proposed", "task_family": "deploy"}, null, null, 1]
        ]);
        let parsed = skill_rows(&rows);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "C-2");
    }

    #[test]
    fn a_verdict_records_what_would_be_needed_to_recompute_it() {
        // Rule 2: rule identity, comparison basis, and the graded outcome set
        // must be recoverable. A verdict that cannot be recomputed can only be
        // believed, and not having to believe it is the point.
        let skill = skill(SkillStatus::Proposed, None);
        let window = window(2, 1);
        let family = FamilyTally {
            success: 12,
            failure: 9,
            graded: 21,
        };
        let verdict = decide(&skill, &window, &family).unwrap();
        let request = verdict_request(&skill, &verdict, &window, "2026-08-31T00:00:00Z");
        let command = request.operations[0].command.as_deref().unwrap();

        assert!(command.contains("EXPECT VERSION :version OF ATTRIBUTES"));
        assert!(command.contains(r#"activity_class: "lifecycle_verdict""#));
        assert!(command.contains("parameters_digest: :digest"));
        // The basis lives on the Skill, where an auditor can read it beside
        // the tallies it will be compared against (§6.5).
        assert!(command.contains(r#"SET FACET "TrialState""#), "{command}");
        assert!(command.contains("rule_id: :rule"), "{command}");
        // `inputs: the linked Outcome Evidence; outputs: the Skill whose
        // lifecycle it moved`. The other way round would read as though the
        // Skill caused the runs that graded it.
        assert!(command.contains(r#"("inputs", :e0)"#), "{command}");
        assert!(command.contains(r#"("outputs", :skill)"#), "{command}");

        let parameters = request.parameters.as_ref().unwrap();
        assert_eq!(parameters["status"], Json::from("trialed"));
        assert_eq!(parameters["e0"], Json::from("E-1"));
        assert_eq!(parameters["rule"], Json::from(VERDICT_RULE));
        assert_eq!(parameters["basis_seq"], Json::from(42u64));
        assert_eq!(parameters["b_success"], Json::from(10u64));
        assert_eq!(parameters["quota"], Json::from(TRIAL_MIN_OUTCOMES));
        // The digest pins rule identity and the comparison basis, and the
        // sequence window is what makes the verdict recomputable: an auditor
        // re-reads this Skill's linked outcomes in `(from, to]` and re-runs the
        // rule against the `TrialState` on the Skill.
        let digest = parameters["digest"].as_str().unwrap();
        assert!(digest.contains(VERDICT_RULE), "{digest}");
        assert!(digest.contains("family=deploy"), "{digest}");
        assert!(digest.contains("basis_seq=42"), "{digest}");
        assert!(digest.contains("window=(10,42]"), "{digest}");
        // The cursor advances to the end of the window, so a replayed pass
        // cannot count the same run twice.
        assert_eq!(parameters["cursor"], Json::from(42u64));
    }

    #[test]
    fn a_deciding_verdict_leaves_the_trial_state_it_was_measured_against() {
        // Rewritten only by a verdict that opens a trial (§6.5): a verdict
        // that overwrote the basis on its way past would erase the one thing
        // that makes it checkable afterwards.
        let skill = skill(SkillStatus::Trialed, Some(0.2));
        let window = window(6, 4);
        let verdict = decide(&skill, &window, &only_linked(&window)).unwrap();
        assert_eq!(verdict.transition, Some(SkillStatus::Adopted));
        let request = verdict_request(&skill, &verdict, &window, "2026-08-31T00:00:00Z");
        let command = request.operations[0].command.as_deref().unwrap();
        assert!(!command.contains("TrialState"), "{command}");
        // But the digest still names the basis it was decided against, read
        // off the Skill's own recorded trial.
        let digest = request.parameters.as_ref().unwrap()["digest"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(digest.contains("baseline=0.200"), "{digest}");
    }

    #[test]
    fn the_tallies_and_the_bet_are_written_to_their_own_facets() {
        // §6.1/§6.2: `GradingState` is the record of what happened,
        // `MnemonicState.utility` the bet on what will. One facet holding both
        // is how a tally starts reading as a forecast.
        let skill = skill(SkillStatus::Trialed, Some(0.2));
        let window = window(3, 1);
        let verdict = decide(&skill, &window, &only_linked(&window)).unwrap();
        let request = verdict_request(&skill, &verdict, &window, "2026-08-31T00:00:00Z");
        let command = request.operations[0].command.as_deref().unwrap();
        assert!(command.contains(r#"SET FACET "GradingState""#), "{command}");
        assert!(
            command.contains(r#"SET FACET "MnemonicState" { utility: :utility }"#),
            "{command}"
        );
        assert!(!command.contains("SkillUtility"), "{command}");

        let parameters = request.parameters.as_ref().unwrap();
        assert_eq!(parameters["utility"], Json::from(0.75));
        assert_eq!(parameters["success"], Json::from(3u64));
        assert_eq!(parameters["failure"], Json::from(1u64));
    }
}

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
//! Grading reads `Evidence {evidence_class: "outcome"}` carrying an
//! `OutcomeRecord` facet whose `task_family` matches the Skill's. The Profile
//! (§14 rule 5) asks for four grades; `OutcomeRecord.outcome_status` supplies
//! five, and this deployment maps them:
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
//! success rate the family was already running at is recorded as the trial's
//! basis; the verdict compares the trial's own rate against it.
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
//! guarded `UPDATE`, and the Activity carries the rule identity, the basis, and
//! the outcome window, so the verdict is recomputable rather than merely
//! asserted.

use serde_json::{Map, Value as Json};

use crate::kip;

/// The identity of the rule below, recorded on every verdict.
///
/// Rule 2 requires a verdict to make "rule identity, comparison basis, and the
/// graded outcome set recoverable". A verdict recorded without saying which
/// rule produced it cannot be recomputed later, only believed — and the whole
/// point of taking this away from the model is that it should not have to be
/// believed. Bump this whenever the thresholds or the tally mapping change.
pub const VERDICT_RULE: &str = "anda-brain/skill-verdict@1";

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

/// Deployment-local attributes this module keeps on a Skill.
///
/// `Skill.attributes` is `open`, and these three are bookkeeping the closed
/// `SkillUtility` facet has no room for: the facet holds tallies, not the
/// cursor and basis the *rule* needs to stay recomputable and non-double-
/// counting.
const CURSOR_ATTR: &str = "verdict_cursor";
const BASIS_ATTR: &str = "trial_basis";
const BASIS_N_ATTR: &str = "trial_basis_n";

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
    /// The success rate the family was running at when this trial opened.
    pub basis: Option<f64>,
    pub success_count: u64,
    pub failure_count: u64,
    pub graded_count: u64,
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
    /// The basis to record when this verdict opens a trial.
    pub basis: Option<f64>,
}

/// The deterministic rule. No model, no clock, no author assertion.
///
/// Returns `None` when nothing at all happened — no new graded outcome — so a
/// pass over an idle Space writes nothing.
pub(crate) fn decide(skill: &SkillRow, window: &Tally) -> Option<Verdict> {
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

    match skill.status {
        // A trial opens as soon as the stream produces anything, and records
        // what the family was already running at. That recorded basis is what
        // makes the later verdict comparative rather than absolute (rule 2).
        SkillStatus::Proposed => Some(Verdict {
            transition: Some(SkillStatus::Trialed),
            rationale: format!(
                "trial opened on {} graded outcome(s) under `{}`; basis {}",
                window.graded,
                skill.task_family,
                rate.map_or("none".to_string(), |basis| format!("{basis:.3}"))
            ),
            basis: rate,
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
                    basis: None,
                });
            }
            let basis = skill.basis.unwrap_or(0.0);
            let (Some(rate), true) = (rate, total.graded >= TRIAL_MIN_OUTCOMES) else {
                return Some(tally_only(&total, skill, "trial still gathering outcomes"));
            };
            if rate >= basis + VERDICT_MARGIN {
                Some(Verdict {
                    transition: Some(SkillStatus::Adopted),
                    rationale: format!(
                        "adopted: {rate:.3} over {} graded outcome(s) beats the recorded \
                         basis {basis:.3} by at least {VERDICT_MARGIN}",
                        total.graded
                    ),
                    basis: None,
                })
            } else if rate <= basis - VERDICT_MARGIN {
                Some(Verdict {
                    transition: Some(SkillStatus::Revoked),
                    rationale: format!(
                        "revoked: {rate:.3} over {} graded outcome(s) trails the recorded \
                         basis {basis:.3} by at least {VERDICT_MARGIN}",
                        total.graded
                    ),
                    basis: None,
                })
            } else {
                Some(tally_only(
                    &total,
                    skill,
                    "trial inconclusive against its basis",
                ))
            }
        }

        // Adoption is provisional: the stream keeps grading, and a rate that
        // falls back demotes to a new trial rather than to nothing (rule 4).
        SkillStatus::Adopted => {
            if window.severe_failure {
                return Some(Verdict {
                    transition: Some(SkillStatus::Revoked),
                    rationale: format!(
                        "revoked: a high-severity matching-condition failure under `{}` \
                         does not wait for a re-verdict",
                        skill.task_family
                    ),
                    basis: None,
                });
            }
            let basis = skill.basis.unwrap_or(0.0);
            match rate {
                Some(rate) if total.graded >= TRIAL_MIN_OUTCOMES && rate < basis => Some(Verdict {
                    transition: Some(SkillStatus::Trialed),
                    rationale: format!(
                        "demoted to re-trial: {rate:.3} has fallen back to its \
                             pre-adoption basis {basis:.3}"
                    ),
                    basis: Some(rate),
                }),
                _ => Some(tally_only(&total, skill, "adoption still holding")),
            }
        }

        // Re-entry starts a new trial; nothing resurrects silently. It takes
        // fresh evidence, which is what a new graded outcome under the family
        // is.
        SkillStatus::Revoked => Some(Verdict {
            transition: Some(SkillStatus::Trialed),
            rationale: format!(
                "re-entry: {} new graded outcome(s) under `{}` open a fresh trial",
                window.graded, skill.task_family
            ),
            basis: rate,
        }),
    }
}

/// A verdict that moves the tallies and the cursor but not the standing.
fn tally_only(total: &Tally, skill: &SkillRow, why: &str) -> Verdict {
    Verdict {
        transition: None,
        rationale: format!(
            "{why}: {} success / {} failure over {} graded under `{}`",
            total.success, total.failure, total.graded, skill.task_family
        ),
        basis: None,
    }
}

/// The Skills this Space holds, with the state the rule needs.
pub(crate) fn skills_request() -> anda_kip::Request {
    // The facet is projected by name rather than as the whole `facets` object:
    // a projected `?s.facets` comes back keyed by full schema ref
    // (`kip://profiles/cognitive-memory@2.0.0/SkillUtility`), and a reader that
    // looked up the local name would silently find nothing and grade every
    // Skill from zero. Naming it lets the engine resolve the symbol.
    kip::request(format!(
        r#"FIND(?s.id, ?s.name, ?s.attributes, ?s.facets["SkillUtility"], ?s._system.version)
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
            let utility = columns.get(3);
            let tally = |name: &str| {
                utility
                    .and_then(|u| u.get(name))
                    .and_then(Json::as_u64)
                    .unwrap_or(0)
            };
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
                version: columns.get(4).and_then(Json::as_u64)?,
                status: SkillStatus::parse(
                    attribute("status")
                        .and_then(Json::as_str)
                        .unwrap_or_default(),
                )?,
                task_family,
                cursor: attribute(CURSOR_ATTR).and_then(Json::as_u64).unwrap_or(0),
                basis: attribute(BASIS_ATTR).and_then(Json::as_f64),
                success_count: tally("success_count"),
                failure_count: tally("failure_count"),
                graded_count: tally("graded_count"),
            })
        })
        .collect()
}

/// The Outcome Evidence for one task family that this Skill has not counted.
///
/// The cursor is a Space sequence coordinate, not a timestamp: it is the same
/// monotonic counter the engine stamps on every element, so "the outcomes that
/// landed since I last judged" is exact. Without it a replayed pass would
/// count the same run twice and promote on arithmetic rather than evidence.
pub(crate) fn outcomes_request(task_family: &str, after: u64) -> anda_kip::Request {
    kip::request_with(
        format!(
            r#"FIND(?e.id, ?e._system.space_seq, ?e.facets["OutcomeRecord"])
WHERE {{
  ?e EVIDENCE {{evidence_class: "outcome"}}
  FILTER(?e.facets["OutcomeRecord"].task_family == :family)
  FILTER(?e._system.space_seq > :after)
}}
ORDER BY ?e._system.space_seq
LIMIT {OUTCOME_WINDOW}"#
        ),
        Map::from_iter([
            ("family".to_string(), Json::from(task_family)),
            ("after".to_string(), Json::from(after)),
        ]),
    )
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
/// The Activity records the rule that ran, the basis it compared against, and
/// the sequence window it read, because rule 2 requires the verdict to be
/// recomputable rather than merely asserted. `EXPECT VERSION` keeps the write
/// safe beside a maintenance model that may be revising the same Skill's
/// applicability or failure modes.
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
    // `utility` is the observed share of decided runs that worked. It is
    // procedural standing, never truth and never permission.
    let utility = Tally {
        success,
        failure,
        ..Default::default()
    }
    .success_rate()
    .unwrap_or(0.0);

    // Rule identity and comparison basis, pinned where the Profile says to pin
    // them. The sequence window is what actually makes the verdict
    // recomputable: an auditor re-reads the Outcome Evidence for this family
    // in `(from, to]` and re-runs the named rule.
    let digest = format!(
        "rule={VERDICT_RULE} family={} basis={} window=({},{}] tally={}s/{}f/{}g verdict={}→{}",
        skill.task_family,
        verdict
            .basis
            .or(skill.basis)
            .map_or("none".to_string(), |basis| format!("{basis:.3}")),
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
    // The basis travels with the trial it opened, so a later verdict compares
    // against the number this one actually recorded.
    let basis_assignment = match verdict.basis {
        Some(basis) => {
            parameters.insert("basis".to_string(), Json::from(basis));
            parameters.insert("basis_n".to_string(), Json::from(window.graded));
            format!(", {BASIS_ATTR}: :basis, {BASIS_N_ATTR}: :basis_n")
        }
        None => String::new(),
    };

    // `inputs: the graded Outcome Evidence; outputs: the Skill whose lifecycle
    // it moved` (Profile §12). Getting these the wrong way round would make
    // the verdict read as though the Skill caused the outcomes.
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
    EXPECT VERSION :version
    SET ATTRIBUTES {{ status: :status, {CURSOR_ATTR}: :cursor{basis_assignment} }}
    SET FACET "SkillUtility" {{
      utility: :utility,
      success_count: :success,
      failure_count: :failure,
      graded_count: :graded,
      last_verdict_at: :now
    }}

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

    fn skill(status: SkillStatus, basis: Option<f64>) -> SkillRow {
        SkillRow {
            id: "C-1".to_string(),
            name: "Redeploy after a schema change".to_string(),
            version: 2,
            status,
            task_family: "deploy".to_string(),
            cursor: 10,
            basis,
            success_count: 0,
            failure_count: 0,
            graded_count: 0,
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
        let command = outcomes_request("deploy", 0).operations[0]
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
        // going", so the trial has to record what "going" was.
        let verdict = decide(&skill(SkillStatus::Proposed, None), &window(1, 1)).unwrap();
        assert_eq!(verdict.transition, Some(SkillStatus::Trialed));
        assert_eq!(verdict.basis, Some(0.5));
    }

    #[test]
    fn no_single_success_promotes() {
        let verdict = decide(&skill(SkillStatus::Trialed, Some(0.2)), &window(1, 0)).unwrap();
        assert_eq!(verdict.transition, None, "{verdict:?}");
        assert!(verdict.rationale.contains("gathering"), "{verdict:?}");
    }

    #[test]
    fn adoption_is_comparative_not_absolute() {
        // A 60% rate is not "good", but against a basis of 20% it is better
        // than things were going — which is the question rule 2 asks.
        let verdict = decide(&skill(SkillStatus::Trialed, Some(0.2)), &window(6, 4)).unwrap();
        assert_eq!(
            verdict.transition,
            Some(SkillStatus::Adopted),
            "{verdict:?}"
        );

        // The same rate against a basis of 80% is a regression, and the same
        // margin in the other direction revokes it.
        let verdict = decide(&skill(SkillStatus::Trialed, Some(0.8)), &window(6, 4)).unwrap();
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
        let promote = decide(&skill(SkillStatus::Trialed, Some(basis)), &window(8, 2));
        let demote = decide(&skill(SkillStatus::Trialed, Some(basis)), &window(2, 8));
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
            let verdict = decide(&skill(status, Some(0.9)), &severe).unwrap();
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
        // back is a re-trial, not amnesty and not deletion.
        let verdict = decide(&skill(SkillStatus::Adopted, Some(0.8)), &window(2, 8)).unwrap();
        assert_eq!(
            verdict.transition,
            Some(SkillStatus::Trialed),
            "{verdict:?}"
        );
        assert_eq!(verdict.basis, Some(0.2), "the re-trial records a new basis");
    }

    #[test]
    fn nothing_resurrects_silently() {
        let verdict = decide(&skill(SkillStatus::Revoked, None), &window(3, 0)).unwrap();
        assert_eq!(verdict.transition, Some(SkillStatus::Trialed));
        assert!(verdict.rationale.contains("fresh trial"), "{verdict:?}");
    }

    #[test]
    fn an_idle_stream_produces_no_verdict_and_no_write() {
        assert_eq!(
            decide(&skill(SkillStatus::Adopted, Some(0.9)), &Tally::default()),
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
            ["C-1", "No family", {"status": "proposed"}, null, 1],
            ["C-2", "Graded", {"status": "proposed", "task_family": "deploy"}, null, 1]
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
        let verdict = decide(&skill, &window).unwrap();
        let request = verdict_request(&skill, &verdict, &window, "2026-08-31T00:00:00Z");
        let command = request.operations[0].command.as_deref().unwrap();

        assert!(command.contains("EXPECT VERSION :version"));
        assert!(command.contains(r#"activity_class: "lifecycle_verdict""#));
        assert!(command.contains("parameters_digest: :digest"));
        assert!(
            command.contains(BASIS_ATTR),
            "the trial's basis is recorded"
        );
        // `inputs: the graded Outcome Evidence; outputs: the Skill whose
        // lifecycle it moved`. The other way round would read as though the
        // Skill caused the runs that graded it.
        assert!(command.contains(r#"("inputs", :e0)"#), "{command}");
        assert!(command.contains(r#"("outputs", :skill)"#), "{command}");

        let parameters = request.parameters.as_ref().unwrap();
        assert_eq!(parameters["status"], Json::from("trialed"));
        assert_eq!(parameters["e0"], Json::from("E-1"));
        // The digest pins rule identity and the comparison basis, and the
        // sequence window is what makes the verdict recomputable: an auditor
        // re-reads this family's outcomes in `(from, to]` and re-runs the rule.
        let digest = parameters["digest"].as_str().unwrap();
        assert!(digest.contains(VERDICT_RULE), "{digest}");
        assert!(digest.contains("family=deploy"), "{digest}");
        assert!(digest.contains("window=(10,42]"), "{digest}");
        // The cursor advances to the end of the window, so a replayed pass
        // cannot count the same run twice.
        assert_eq!(parameters["cursor"], Json::from(42u64));
    }

    #[test]
    fn utility_is_the_observed_rate_and_carries_no_authority() {
        let skill = skill(SkillStatus::Trialed, Some(0.2));
        let window = window(3, 1);
        let verdict = decide(&skill, &window).unwrap();
        let request = verdict_request(&skill, &verdict, &window, "2026-08-31T00:00:00Z");
        let parameters = request.parameters.as_ref().unwrap();
        assert_eq!(parameters["utility"], Json::from(0.75));
        assert_eq!(parameters["success"], Json::from(3u64));
        assert_eq!(parameters["failure"], Json::from(1u64));
    }
}

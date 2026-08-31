//! Deterministic Watch expiry: the half of Watch evaluation that is arithmetic.
//!
//! A Watch is durable attention — a declared condition under which a change,
//! or the *absence* of one, deserves the Brain's attention (Profile §5.11).
//! Evaluating one splits cleanly in two, and the split is what this module is:
//!
//! - A **delta** Watch fires when a committed change matches its `condition`.
//!   The Profile deliberately fixes no condition language, so deciding whether
//!   a change matches "the vendor replied about the renewal" is interpretation.
//!   That stays with the maintenance model, which reads `CHANGES AFTER SEQ`
//!   against the armed set.
//! - A **silence** Watch fires when `due_at` passes and nothing matched. By the
//!   time the deadline arrives, "nothing matched" is not a judgement at all: a
//!   Watch still `armed` is one no evaluation has fired. Thursday arriving is
//!   arithmetic, and arithmetic should not wait for a model to be scheduled,
//!   have context budget, and notice.
//!
//! So the runtime fires the silence half, on the same footing as the retention
//! sweep: the host decides *when* forgetting and noticing happen, never *what*
//! they mean. Firing writes a `watch_fire` Activity and moves the Watch to
//! `fired`; it creates no SleepTask and takes no outward action, because
//! **a fired Watch authorizes nothing**. What to do about it — act, ask, defer,
//! or deliberately stay silent — is the action gate, and that is cognition:
//! the next maintenance cycle receives the fired set and records its decision
//! as an `action_gate` Activity.
//!
//! Without this, "waiting is active" (reference README principle 17) was not
//! true here: a Commitment whose trigger was a silence Watch waited forever,
//! because nothing in the system noticed a date passing.

use anda_kip::Response;
use serde_json::{Map, Value as Json};

use crate::{kip, types::ArmedWatch};

/// How many Watches one sweep may fire.
///
/// A backlog larger than this drains across cycles rather than turning one
/// sweep into an unbounded write burst. Watches are fired oldest-due first, so
/// the backlog drains in the order the deadlines passed.
const WATCH_SWEEP_LIMIT: usize = 20;

/// Reads the silence Watches whose deadline has passed.
///
/// `due_at` is compared as a string because graph timestamps are RFC3339 and
/// lexicographically ordered — the same comparison the retention sweep makes.
/// A Watch with no `due_at` never matches, which is correct: a silence Watch
/// without a deadline has declared no moment at which silence becomes
/// meaningful.
pub(crate) fn due_silence_watches_request(now: &str) -> anda_kip::Request {
    kip::request_with(
        format!(
            r#"FIND(?w.id, ?w.name, ?w.attributes, ?w._system.version)
WHERE {{
  ?w CONCEPT {{type: "Watch"}}
  FILTER(?w.attributes.status == "armed")
  FILTER(?w.attributes.watch_class == "silence")
  FILTER(IS_NOT_NULL(?w.attributes.due_at))
  FILTER(?w.attributes.due_at <= :now)
}}
ORDER BY ?w.attributes.due_at
LIMIT {WATCH_SWEEP_LIMIT}"#
        ),
        kip::param("now", now),
    )
}

/// One due Watch, as the scan returns it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DueWatch {
    pub id: String,
    pub version: u64,
    pub watch: ArmedWatch,
}

/// Reads the scan rows into due Watches.
///
/// A row missing its id or version is skipped rather than fired: the guarded
/// update needs both, and firing without the version would overwrite a
/// concurrent edit instead of yielding to it.
pub(crate) fn due_watches(result: &Json) -> Vec<DueWatch> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let columns = row.as_array()?;
            let id = columns.first().and_then(Json::as_str)?.to_string();
            let version = columns.get(3).and_then(Json::as_u64)?;
            let attribute = |name: &str| {
                columns
                    .get(2)
                    .and_then(|attributes| attributes.get(name))
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            Some(DueWatch {
                watch: ArmedWatch {
                    id: id.clone(),
                    name: columns
                        .get(1)
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    watch_class: attribute("watch_class"),
                    condition: attribute("condition"),
                    summary: attribute("summary"),
                    due_at: attribute("due_at"),
                },
                id,
                version,
            })
        })
        .collect()
}

/// Fires one silence Watch: the transition and its provenance, atomically.
///
/// Three things are deliberately absent.
///
/// No SleepTask: the Profile says firing produces "the SleepTask **or wake
/// signal** it produces", and the wake signal here is the fired set reaching
/// the next cycle's assessment. Minting a SleepTask would also have meant
/// choosing a `task_class` from an enum that has no entry for "a deadline
/// passed and somebody must decide" — a wrong label is worse than no label.
///
/// No `action_gate` Activity: that outcome is `act`, `ask`, `defer` or
/// `silence`, and every one of them is a judgement about what this deadline
/// means. The runtime knows the date passed; it does not know what to do about
/// it, and a host that recorded `defer` on the model's behalf would be
/// fabricating a decision nobody made.
///
/// No outward action of any kind. A fired Watch grants nothing.
///
/// `EXPECT VERSION` is what makes the sweep safe to run beside the maintenance
/// model: if the model disarmed or re-armed this Watch since the scan, the
/// write is refused rather than applied over its work.
pub(crate) fn fire_watch_request(watch: &DueWatch, now: &str) -> anda_kip::Request {
    let parameters = Map::from_iter([
        ("watch".to_string(), Json::from(watch.id.as_str())),
        ("version".to_string(), Json::from(watch.version)),
        ("now".to_string(), Json::from(now)),
    ]);
    kip::request_with(
        r#"MUTATE {
  UPDATE :watch
    EXPECT VERSION :version
    SET ATTRIBUTES { status: "fired", fired_at: :now }

  CREATE ACTIVITY ?fire {
    SET FIELDS { activity_class: "watch_fire", status: "completed" }
    SET STRUCTURAL { ("inputs", :watch) }
  }
}"#,
        parameters,
    )
}

/// Whether a fire failed because the Watch moved under the sweep.
///
/// A version conflict is the ordinary outcome of racing the maintenance model,
/// not a fault: the Watch stays armed and the next sweep re-reads it. Anything
/// else is worth a log line.
pub(crate) fn is_version_conflict(response: &Response) -> bool {
    kip::error_of(response)
        .map(|error| error.code.to_string())
        .is_some_and(|code| code.contains("Version") || code.contains("Precondition"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_scan_asks_only_for_silence_watches_whose_deadline_has_passed() {
        let request = due_silence_watches_request("2026-08-31T00:00:00Z");
        let command = request.operations[0].command.as_deref().unwrap();
        // A delta Watch is the model's to evaluate: its condition is prose the
        // Profile deliberately does not fix a language for.
        assert!(command.contains(r#"watch_class == "silence""#));
        assert!(command.contains(r#"status == "armed""#));
        // A silence Watch with no deadline has declared no moment at which
        // silence becomes meaningful, so it never comes due.
        assert!(command.contains("IS_NOT_NULL(?w.attributes.due_at)"));
        assert!(command.contains("due_at <= :now"));
        // Oldest deadline first, so a backlog drains in the order it accrued.
        assert!(command.contains("ORDER BY ?w.attributes.due_at"));
    }

    #[test]
    fn the_scan_reads_rows_positionally_and_skips_what_it_cannot_guard() {
        let rows = json!([
            [
                "C-7",
                "Vendor reply",
                {
                    "watch_class": "silence",
                    "condition": "no message from the vendor",
                    "summary": "Escalate if nothing by Thursday",
                    "due_at": "2026-08-30T00:00:00Z",
                    "status": "armed"
                },
                3
            ],
            // No version: the guarded update could not be built, and firing
            // unguarded would overwrite whatever moved it.
            ["C-8", "No version", {"watch_class": "silence"}, null],
            // Not a row at all.
            "nonsense"
        ]);

        let due = due_watches(&rows);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "C-7");
        assert_eq!(due[0].version, 3);
        assert_eq!(due[0].watch.due_at, "2026-08-30T00:00:00Z");
        assert_eq!(due[0].watch.summary, "Escalate if nothing by Thursday");
    }

    #[test]
    fn firing_records_attention_and_grants_nothing() {
        let watch = DueWatch {
            id: "C-7".to_string(),
            version: 3,
            watch: ArmedWatch::default(),
        };
        let request = fire_watch_request(&watch, "2026-08-31T00:00:00Z");
        let command = request.operations[0].command.as_deref().unwrap();

        // The transition and its provenance commit together: a Watch marked
        // fired with no Activity saying why is a state change nobody can audit.
        assert!(command.contains(r#"status: "fired""#));
        assert!(command.contains(r#"activity_class: "watch_fire""#));
        // Guarded, so racing the maintenance model yields instead of clobbers.
        assert!(command.contains("EXPECT VERSION :version"));

        // What firing must NOT do. The outward decision is the action gate's,
        // and the gate is cognition — the runtime knows the date passed, not
        // what to do about it.
        assert!(!command.contains("action_gate"), "{command}");
        assert!(!command.contains("SleepTask"), "{command}");
    }
}

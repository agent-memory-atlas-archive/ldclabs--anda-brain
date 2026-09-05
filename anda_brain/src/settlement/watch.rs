//! Deterministic Watch evaluation: the half of Watch evaluation that is
//! arithmetic, and the guard on the half that is not.
//!
//! A Watch is durable attention — a declared condition under which a change,
//! or the *absence* of one, deserves the Brain's attention (Profile §5.11).
//! Evaluating one splits in two, and the split is what this module is:
//!
//! - A **structured** `condition` — `{element | slot | type, ops, touched}`,
//!   the baseline form §5.11 gives it — selects Change Envelope entries
//!   (Spec §36.1) without reading payload, which is exactly what makes it the
//!   runtime's to evaluate. Every sweep reads `CHANGES AFTER SEQ` from where
//!   the Watch was last evaluated and matches the entries: a `delta` Watch
//!   that matches fires, a `silence` Watch that matches is disarmed, because
//!   the thing it was waiting on has arrived and its silence can no longer
//!   happen. What it read through is stamped on the Watch as
//!   `evaluated_seq`, so the next sweep continues rather than re-reads.
//! - A **prose** `condition` — a bare string, or a structured form carrying
//!   members this runtime does not understand — is interpretation. Deciding
//!   whether a change matches "the vendor replied about the renewal" stays
//!   with the maintenance model, which reads the same stream against the
//!   armed set.
//!
//! A `silence` Watch fires when `due_at` passes and nothing matched — and
//! §5.11 is explicit that the clock alone proves nothing: the evaluator MUST
//! have consumed the Change Stream through the coordinate current at `due_at`
//! before it may conclude silence, because a matching change committed before
//! the deadline may still be on its way to the evaluator. So:
//!
//! - a structured silence Watch fires in the sweep that evaluated it through
//!   the Space's head and found nothing;
//! - a prose silence Watch is stamped `due_seen_seq` — the head the sweep
//!   first saw its deadline passed at — and fires only once the Brain's own
//!   consumption record (`consumed_seq`, written when a maintenance cycle
//!   completes) has reached that coordinate. One cycle of latency, spent on
//!   the evaluation the Profile requires.
//!
//! Firing writes a `watch_fire` Activity and moves the Watch to `fired`; it
//! creates no SleepTask and takes no outward action, because **a fired Watch
//! authorizes nothing**. What to do about it — act, ask, defer, or
//! deliberately stay silent — is the action gate, and that is cognition: the
//! next maintenance cycle receives the fired set and records its decision as
//! an `action_gate` Activity.

use anda_kip::Response;
use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

use crate::{kip, types::ArmedWatch};

/// How many Watches one sweep reads per scan.
///
/// A backlog larger than this drains across cycles rather than turning one
/// sweep into an unbounded write burst. Due Watches are read oldest-deadline
/// first, so the backlog drains in the order the deadlines passed.
pub(crate) const WATCH_SWEEP_LIMIT: usize = 20;

/// How many Change Envelopes one stream page carries.
pub(crate) const CHANGES_PAGE_LIMIT: usize = 200;

/// How many pages one sweep reads before it stops and records where it got
/// to. A Space that moved further than this between two cycles is evaluated
/// across several sweeps rather than in one unbounded read.
pub(crate) const CHANGES_MAX_PAGES: usize = 5;

/// The columns every Watch scan projects, in the order the readers use them.
const WATCH_COLUMNS: &str = "?w.id, ?w.name, ?w.attributes, ?w._system.plane_versions.attributes";

/// Reads the Watches in one status, oldest deadline first.
///
/// One shape for the sweep and the assessment: the armed set the sweep
/// evaluates is the armed set the maintenance model is shown, and two readers
/// of one row layout would be two places for it to drift.
pub(crate) fn watches_request(status: &str) -> anda_kip::Request {
    kip::request_with(
        format!(
            r#"FIND({WATCH_COLUMNS})
WHERE {{
  ?w CONCEPT {{type: "Watch"}}
  FILTER(?w.attributes.status == :status)
}}
ORDER BY ?w.attributes.due_at
LIMIT {WATCH_SWEEP_LIMIT}"#
        ),
        kip::param("status", status),
    )
}

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
            r#"FIND({WATCH_COLUMNS})
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

/// One Watch, as the scan returns it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WatchRow {
    pub id: String,
    /// The attributes-plane version the guarded writes expect.
    pub version: u64,
    /// The condition as written — structured or prose.
    pub condition: Json,
    /// Through which coordinate the runtime has evaluated this Watch.
    pub evaluated_seq: Option<u64>,
    /// The head at which a sweep first saw a prose silence Watch's deadline
    /// passed; the guard the Brain's consumption has to reach.
    pub due_seen_seq: Option<u64>,
    /// The Watch as the maintenance prompt receives it.
    pub watch: ArmedWatch,
}

impl WatchRow {
    pub fn is_silence(&self) -> bool {
        self.watch.watch_class == "silence"
    }

    /// Whether the deadline has passed, as the scan compares it.
    pub fn is_due(&self, now: &str) -> bool {
        !self.watch.due_at.is_empty() && self.watch.due_at.as_str() <= now
    }
}

/// The coordinate at which the stream last shows the Watch being armed: the
/// entry that created it, or the last one that touched its attributes — which
/// is what a model's `status: "armed"` does and a Facet sweep does not.
///
/// This, and not the Watch's own `_system.space_seq`, is where a Watch nobody
/// has stamped starts: the metabolism sweep writes a Facet on every Concept,
/// Watches included, so that coordinate follows the sweep to the head and would
/// step over the very changes the Watch was armed for.
pub(crate) fn armed_at(envelopes: &[Json], id: &str) -> Option<u64> {
    envelopes
        .iter()
        .filter_map(|envelope| {
            let seq = envelope.get("space_seq").and_then(Json::as_u64)?;
            let arming = envelope
                .get("changes")
                .and_then(Json::as_array)?
                .iter()
                .any(|entry| {
                    entry.get("id").and_then(Json::as_str) == Some(id)
                        && (entry.get("op").and_then(Json::as_str) == Some("create")
                            || entry
                                .get("touched")
                                .and_then(Json::as_array)
                                .is_some_and(|paths| {
                                    paths.iter().filter_map(Json::as_str).any(|path| {
                                        path.starts_with("attributes")
                                    })
                                }))
                });
            arming.then_some(seq)
        })
        .max()
}

/// Reads scan rows into Watches.
///
/// A row missing its id or version is skipped rather than acted on: every
/// write here is guarded, and writing without the version would overwrite a
/// concurrent edit instead of yielding to it.
pub(crate) fn read_watch_rows(result: &Json) -> Vec<WatchRow> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let columns = row.as_array()?;
            let id = columns.first().and_then(Json::as_str)?.to_string();
            let version = columns.get(3).and_then(Json::as_u64)?;
            let attributes = columns.get(2).and_then(Json::as_object);
            let attribute = |name: &str| {
                attributes
                    .and_then(|attributes| attributes.get(name))
                    .map(crate::types::attribute_text)
                    .unwrap_or_default()
            };
            let seq_attribute = |name: &str| {
                attributes
                    .and_then(|attributes| attributes.get(name))
                    .and_then(Json::as_u64)
            };
            Some(WatchRow {
                version,
                condition: attributes
                    .and_then(|attributes| attributes.get("condition"))
                    .cloned()
                    .unwrap_or(Json::Null),
                evaluated_seq: seq_attribute("evaluated_seq"),
                due_seen_seq: seq_attribute("due_seen_seq"),
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
            })
        })
        .collect()
}

/// A structured condition this runtime can evaluate (Profile §5.11).
///
/// `element`, `slot` and `type` select what is watched — at least one, and
/// every one present has to hold; `ops` and `touched` narrow which entries
/// count, and an empty list means any.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChangeFilter {
    pub element: Option<String>,
    /// `(subject id, predicate local name)`.
    pub slot: Option<(String, String)>,
    pub type_name: Option<String>,
    pub ops: Vec<String>,
    pub touched: Vec<String>,
}

/// The runtime-evaluable reading of a condition, or `None` when it is the
/// Brain's.
///
/// `None` for prose, for a structured form with no selector (a Watch that
/// carries only `text` is Brain-evaluated by definition), and for any member
/// this runtime cannot read the way §5.11 means it. Refusing the last case is
/// the point: a selector half-understood is a Watch that fires on the wrong
/// change or never, and either is worse than leaving it to the model.
pub(crate) fn change_filter(condition: &Json) -> Option<ChangeFilter> {
    let members = condition.as_object()?;
    let mut filter = ChangeFilter::default();
    let strings = |value: &Json| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect()
    };
    for (name, value) in members {
        match name.as_str() {
            "element" => filter.element = Some(value.as_str()?.to_string()),
            "type" => filter.type_name = Some(value.as_str()?.to_string()),
            "slot" => {
                let slot = value.as_object()?;
                filter.slot = Some((
                    slot.get("subject")?.as_str()?.to_string(),
                    slot.get("predicate")?.as_str()?.to_string(),
                ));
            }
            "ops" => filter.ops = strings(value)?,
            "touched" => filter.touched = strings(value)?,
            // The fallback the Brain interprets when the structured members
            // cannot express the condition; ignored beside a selector.
            "text" => {}
            _ => return None,
        }
    }
    if filter.element.is_none() && filter.slot.is_none() && filter.type_name.is_none() {
        return None;
    }
    Some(filter)
}

/// The local name of a symbol reference, or the name itself when bare.
fn local_name(symbol: &str) -> &str {
    symbol.rsplit('/').next().unwrap_or(symbol)
}

/// The Propositions each watched slot names, resolved before matching.
pub(crate) type SlotIndex = BTreeMap<(String, String), BTreeSet<String>>;

/// Reads the Propositions of one slot, so an Assertion entry — which carries
/// only `refs.proposition` — can be matched against `slot`.
pub(crate) fn slot_request(subject: &str, predicate: &str) -> anda_kip::Request {
    kip::request_with(
        "FIND(?p.id) WHERE { ?p (:subject, :predicate, ?o) } LIMIT 100",
        Map::from_iter([
            ("subject".to_string(), serde_json::json!({"id": subject})),
            ("predicate".to_string(), Json::from(predicate)),
        ]),
    )
}

/// Whether one Change Envelope entry (§36.1) is what the filter watches.
pub(crate) fn entry_matches(filter: &ChangeFilter, entry: &Json, slots: &SlotIndex) -> bool {
    let text = |name: &str| entry.get(name).and_then(Json::as_str);
    if let Some(element) = &filter.element
        && text("id") != Some(element.as_str())
    {
        return false;
    }
    if let Some(type_name) = &filter.type_name {
        // Only a Concept entry carries `schema_ref`; the type of anything
        // else is not what a `type` selector watches.
        let Some(schema_ref) = text("schema_ref") else {
            return false;
        };
        if local_name(schema_ref) != type_name {
            return false;
        }
    }
    if let Some((subject, predicate)) = &filter.slot {
        let refs = entry.get("refs");
        let reference = |name: &str| refs.and_then(|refs| refs.get(name)).and_then(Json::as_str);
        let in_slot = match text("kind") {
            Some("proposition") => {
                reference("subject") == Some(subject.as_str())
                    && reference("predicate_ref").is_some_and(|p| local_name(p) == predicate)
            }
            Some("assertion") => reference("proposition").is_some_and(|proposition| {
                slots
                    .get(&(subject.clone(), predicate.clone()))
                    .is_some_and(|propositions| propositions.contains(proposition))
            }),
            _ => false,
        };
        if !in_slot {
            return false;
        }
    }
    if !filter.ops.is_empty() && !text("op").is_some_and(|op| filter.ops.iter().any(|o| o == op)) {
        return false;
    }
    if !filter.touched.is_empty() {
        let touched = entry
            .get("touched")
            .and_then(Json::as_array)
            .map(|paths| paths.iter().filter_map(Json::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        if !filter.touched.iter().any(|path| touched.contains(&path.as_str())) {
            return false;
        }
    }
    true
}

/// One page of the Change Stream.
pub(crate) fn changes_request(after: u64) -> anda_kip::Request {
    kip::request_with(
        format!("CHANGES AFTER SEQ :after LIMIT {CHANGES_PAGE_LIMIT}"),
        kip::param("after", after),
    )
}

/// The first coordinate in `envelopes` after `from` whose entries the filter
/// matches.
pub(crate) fn first_match(
    filter: &ChangeFilter,
    from: u64,
    envelopes: &[Json],
    slots: &SlotIndex,
) -> Option<u64> {
    envelopes.iter().find_map(|envelope| {
        let seq = envelope.get("space_seq").and_then(Json::as_u64)?;
        if seq <= from {
            return None;
        }
        envelope
            .get("changes")
            .and_then(Json::as_array)?
            .iter()
            .any(|entry| entry_matches(filter, entry, slots))
            .then_some(seq)
    })
}

/// Why a Watch fires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Fire {
    /// The deadline passed with nothing matched.
    Silence,
    /// A committed change matched, at this coordinate.
    Delta(u64),
}

/// Fires one Watch: the transition and its provenance, atomically.
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
/// `CLIENT KEY` is what makes the firing idempotent under concurrent
/// evaluators (Profile §5.11): the key is `watch_fire:<watch id>:silence:<due
/// at>` or `watch_fire:<watch id>:delta:<matched seq>`, so a second evaluator
/// that saw the same deadline or the same change resolves to the first mint
/// (§52.1) instead of writing a second `watch_fire` for one event. The
/// deadline or the coordinate is in the key rather than the wall clock,
/// because two evaluators firing the same Watch are firing it for the same
/// reason.
///
/// `EXPECT VERSION ... OF ATTRIBUTES` is what makes the sweep safe to run
/// beside the maintenance model: if the model disarmed or re-armed this Watch
/// since the scan, the write is refused rather than applied over its work.
/// Guarding the attributes plane rather than the whole element (§35.1) is what
/// keeps a `MnemonicState` decay sweep over the same Concept from spoiling a
/// fire it did not touch — a conflict there would leave the Watch armed past
/// its deadline for a reason having nothing to do with the Watch.
pub(crate) fn fire_watch_request(
    watch: &WatchRow,
    now: &str,
    fire: Fire,
    evaluated_seq: Option<u64>,
) -> anda_kip::Request {
    let mut parameters = Map::from_iter([
        ("watch".to_string(), Json::from(watch.id.as_str())),
        ("version".to_string(), Json::from(watch.version)),
        ("now".to_string(), Json::from(now)),
    ]);
    let mut members = vec!["status: \"fired\"", "fired_at: :now"];
    match fire {
        Fire::Silence => {
            parameters.insert(
                "fire_key".to_string(),
                Json::from(format!(
                    "watch_fire:{}:silence:{}",
                    watch.id, watch.watch.due_at
                )),
            );
        }
        Fire::Delta(seq) => {
            parameters.insert(
                "fire_key".to_string(),
                Json::from(format!("watch_fire:{}:delta:{seq}", watch.id)),
            );
            parameters.insert("matched_seq".to_string(), Json::from(seq));
            members.push("matched_seq: :matched_seq");
        }
    }
    if let Some(seq) = evaluated_seq {
        parameters.insert("evaluated_seq".to_string(), Json::from(seq));
        members.push("evaluated_seq: :evaluated_seq");
    }
    kip::request_with(
        format!(
            r#"MUTATE {{
  UPDATE :watch
    SET ATTRIBUTES {{ {} }}
    EXPECT VERSION :version OF ATTRIBUTES

  CREATE ACTIVITY ?fire {{
    CLIENT KEY :fire_key
    SET FIELDS {{ activity_class: "watch_fire", status: "completed" }}
    SET STRUCTURAL {{ ("inputs", :watch) }}
  }}
}}"#,
            members.join(", ")
        ),
        parameters,
    )
}

/// Disarms a silence Watch whose awaited change arrived before its deadline.
///
/// Not a fire: the Watch was waiting for *silence*, and the change is the
/// opposite of what it was armed for. The coordinate the change committed at
/// is kept on the Watch, so the reader who wonders why a silence Watch stood
/// down finds the transaction that answered it.
pub(crate) fn disarm_matched_request(
    watch: &WatchRow,
    now: &str,
    matched_seq: u64,
) -> anda_kip::Request {
    kip::request_with(
        r#"MUTATE {
  UPDATE :watch
    SET ATTRIBUTES { status: "disarmed", disarmed_at: :now, matched_seq: :matched_seq, evaluated_seq: :matched_seq }
    EXPECT VERSION :version OF ATTRIBUTES
}"#,
        Map::from_iter([
            ("watch".to_string(), Json::from(watch.id.as_str())),
            ("version".to_string(), Json::from(watch.version)),
            ("now".to_string(), Json::from(now)),
            ("matched_seq".to_string(), Json::from(matched_seq)),
        ]),
    )
}

/// Records a coordinate on a Watch: `evaluated_seq` (the runtime read the
/// stream through here) or `due_seen_seq` (the head at which a sweep first
/// saw the deadline passed).
///
/// The name is fixed by the caller, never by data: the two are the only
/// attributes the sweep owns on a Watch.
pub(crate) fn stamp_request(watch: &WatchRow, attribute: &str, seq: u64) -> anda_kip::Request {
    kip::request_with(
        format!(
            r#"MUTATE {{
  UPDATE :watch
    SET ATTRIBUTES {{ {attribute}: :seq }}
    EXPECT VERSION :version OF ATTRIBUTES
}}"#
        ),
        Map::from_iter([
            ("watch".to_string(), Json::from(watch.id.as_str())),
            ("version".to_string(), Json::from(watch.version)),
            ("seq".to_string(), Json::from(seq)),
        ]),
    )
}

/// Whether a write failed because the Watch moved under the sweep.
///
/// A version conflict is the ordinary outcome of racing the maintenance model,
/// not a fault: the Watch stays where it was and the next sweep re-reads it.
/// Anything else is worth a log line.
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
        assert!(command.contains(r#"watch_class == "silence""#));
        assert!(command.contains(r#"status == "armed""#));
        // A silence Watch with no deadline has declared no moment at which
        // silence becomes meaningful, so it never comes due.
        assert!(command.contains("IS_NOT_NULL(?w.attributes.due_at)"));
        assert!(command.contains("due_at <= :now"));
        // Oldest deadline first, so a backlog drains in the order it accrued.
        assert!(command.contains("ORDER BY ?w.attributes.due_at"));
        // The assessment's read projects the same columns, so one reader
        // serves both.
        let armed = watches_request("armed");
        let armed = armed.operations[0].command.as_deref().unwrap();
        assert!(armed.contains(WATCH_COLUMNS) && command.contains(WATCH_COLUMNS));
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
                    "status": "armed",
                    "evaluated_seq": 40,
                    "due_seen_seq": 41
                },
                3,
                12
            ],
            // No version: the guarded update could not be built, and writing
            // unguarded would overwrite whatever moved it.
            ["C-8", "No version", {"watch_class": "silence"}, null, 13],
            // Not a row at all.
            "nonsense"
        ]);

        let read = read_watch_rows(&rows);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].id, "C-7");
        assert_eq!(read[0].version, 3);
        assert_eq!(read[0].evaluated_seq, Some(40));
        assert_eq!(read[0].due_seen_seq, Some(41));
        assert_eq!(read[0].watch.due_at, "2026-08-30T00:00:00Z");
        assert_eq!(read[0].watch.summary, "Escalate if nothing by Thursday");
        // Prose is the Brain's.
        assert!(change_filter(&read[0].condition).is_none());
        assert!(read[0].is_due("2026-08-31T00:00:00Z"));
        assert!(!read[0].is_due("2026-08-29T00:00:00Z"));
    }

    #[test]
    fn a_watch_starts_at_its_arming_not_at_its_last_facet_write() {
        let envelopes = vec![
            json!({"space_seq": 3, "changes": [{"op": "create", "kind": "concept", "id": "W-1"}]}),
            // The model re-armed it: an attributes write.
            json!({"space_seq": 7, "changes": [
                {"op": "update", "kind": "concept", "id": "W-1", "touched": ["attributes.status"]}
            ]}),
            // The metabolism sweep: a Facet write, which is not an arming.
            json!({"space_seq": 9, "changes": [
                {"op": "update", "kind": "concept", "id": "W-1", "touched": ["facets.MnemonicState"]}
            ]}),
            json!({"space_seq": 11, "changes": [{"op": "create", "kind": "concept", "id": "W-2"}]}),
        ];
        assert_eq!(armed_at(&envelopes, "W-1"), Some(7));
        assert_eq!(armed_at(&envelopes, "W-2"), Some(11));
        assert_eq!(armed_at(&envelopes, "W-3"), None);
    }

    #[test]
    fn a_structured_condition_survives_being_read() {
        // §5.11 gave `condition` a baseline structured form, so it is
        // `string | object` now. Reading only the string case would render a
        // structured condition as the empty string — "this Watch declares no
        // condition", which is the one thing a Watch always does.
        let rows = json!([[
            "C-9",
            "Migration reply",
            {
                "watch_class": "silence",
                "condition": {
                    "slot": {"subject": "C-1", "predicate": "replied_about"},
                    "ops": ["create"]
                },
                "summary": "Escalate if nothing by Thursday",
                "due_at": "2026-08-30T00:00:00Z",
                "status": "armed"
            },
            2,
            50
        ]]);
        let read = read_watch_rows(&rows);
        assert_eq!(read.len(), 1);
        assert!(read[0].watch.condition.contains("replied_about"), "{read:?}");
        assert_eq!(
            change_filter(&read[0].condition),
            Some(ChangeFilter {
                slot: Some(("C-1".into(), "replied_about".into())),
                ops: vec!["create".into()],
                ..Default::default()
            })
        );
        // Never evaluated: the sweep finds its arming in the stream instead.
        assert_eq!(read[0].evaluated_seq, None);
    }

    #[test]
    fn only_a_condition_this_runtime_can_read_whole_is_the_runtimes() {
        // A selector is required: `text` alone is Brain-evaluated (§5.11).
        assert!(change_filter(&json!({"text": "any reply from Alice"})).is_none());
        // A member this runtime does not know is a condition it does not
        // understand, not one it understands partly.
        assert!(change_filter(&json!({"element": "C-1", "since": "yesterday"})).is_none());
        // A malformed selector is refused rather than ignored.
        assert!(change_filter(&json!({"slot": {"subject": "C-1"}})).is_none());
        assert!(change_filter(&json!({"element": 7})).is_none());
        // `text` beside a selector is the human-readable gloss, nothing more.
        assert_eq!(
            change_filter(&json!({"element": "C-1", "text": "Alice's profile changes"})),
            Some(ChangeFilter {
                element: Some("C-1".into()),
                ..Default::default()
            })
        );
    }

    #[test]
    fn entries_match_on_element_type_slot_ops_and_touched() {
        let slots = SlotIndex::from([(
            ("C-1".to_string(), "timezone".to_string()),
            BTreeSet::from(["P-11".to_string()]),
        )]);
        let concept = json!({
            "op": "update", "kind": "concept", "id": "C-42",
            "schema_ref": "kip://anda-brain/memory@2.0.3/Commitment",
            "touched": ["attributes.status"]
        });
        let assertion = json!({
            "op": "create", "kind": "assertion", "id": "A-3",
            "refs": {"proposition": "P-11"}, "touched": []
        });
        let proposition = json!({
            "op": "create", "kind": "proposition", "id": "P-12",
            "refs": {"subject": "C-1", "predicate_ref": "kip://x@1.0.0/timezone"}, "touched": []
        });

        let by_element = ChangeFilter {
            element: Some("C-42".into()),
            ..Default::default()
        };
        assert!(entry_matches(&by_element, &concept, &slots));
        assert!(!entry_matches(&by_element, &assertion, &slots));

        // The type is read off the local name of the entry's `schema_ref`.
        let by_type = ChangeFilter {
            type_name: Some("Commitment".into()),
            ..Default::default()
        };
        assert!(entry_matches(&by_type, &concept, &slots));
        assert!(!entry_matches(&by_type, &assertion, &slots));

        // A slot matches the Proposition entry by its refs, and an Assertion
        // entry through the Propositions the slot was resolved to.
        let by_slot = ChangeFilter {
            slot: Some(("C-1".into(), "timezone".into())),
            ..Default::default()
        };
        assert!(entry_matches(&by_slot, &assertion, &slots));
        assert!(entry_matches(&by_slot, &proposition, &slots));
        assert!(!entry_matches(&by_slot, &concept, &slots));

        // `ops` and `touched` narrow; every selector present has to hold.
        let narrowed = ChangeFilter {
            element: Some("C-42".into()),
            ops: vec!["lifecycle".into()],
            ..Default::default()
        };
        assert!(!entry_matches(&narrowed, &concept, &slots));
        let touched = ChangeFilter {
            type_name: Some("Commitment".into()),
            touched: vec!["attributes.status".into()],
            ..Default::default()
        };
        assert!(entry_matches(&touched, &concept, &slots));
        let untouched = ChangeFilter {
            touched: vec!["attributes.due_at".into()],
            ..touched
        };
        assert!(!entry_matches(&untouched, &concept, &slots));

        // The stream is matched past the coordinate already evaluated.
        let envelopes = vec![
            json!({"space_seq": 5, "changes": [concept.clone()]}),
            json!({"space_seq": 9, "changes": [assertion.clone()]}),
        ];
        assert_eq!(first_match(&by_slot, 4, &envelopes, &slots), Some(9));
        assert_eq!(first_match(&by_element, 4, &envelopes, &slots), Some(5));
        assert_eq!(first_match(&by_element, 5, &envelopes, &slots), None);
    }

    fn row(id: &str, watch_class: &str) -> WatchRow {
        WatchRow {
            id: id.to_string(),
            version: 3,
            condition: Json::Null,
            evaluated_seq: None,
            due_seen_seq: None,
            watch: ArmedWatch {
                id: id.to_string(),
                watch_class: watch_class.to_string(),
                due_at: "2026-08-30T00:00:00Z".to_string(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn firing_records_attention_and_grants_nothing() {
        let watch = row("C-7", "silence");
        let request = fire_watch_request(&watch, "2026-08-31T00:00:00Z", Fire::Silence, Some(44));
        let command = request.operations[0].command.as_deref().unwrap();

        // Idempotent under concurrent evaluators: two sweeps that saw the same
        // passed deadline resolve to one `watch_fire`, not two (§5.11).
        assert!(command.contains("CLIENT KEY :fire_key"), "{command}");
        let parameters = request.parameters.as_ref().unwrap();
        assert_eq!(
            parameters["fire_key"],
            Json::from("watch_fire:C-7:silence:2026-08-30T00:00:00Z")
        );
        // What the sweep read through travels with the fire, so the record
        // says silence was concluded over a consumed stream, not a clock.
        assert!(command.contains("evaluated_seq: :evaluated_seq"), "{command}");
        assert_eq!(parameters["evaluated_seq"], Json::from(44));

        // The transition and its provenance commit together: a Watch marked
        // fired with no Activity saying why is a state change nobody can audit.
        assert!(command.contains(r#"status: "fired""#));
        assert!(command.contains(r#"activity_class: "watch_fire""#));
        // Guarded, so racing the maintenance model yields instead of clobbers.
        assert!(command.contains("EXPECT VERSION :version OF ATTRIBUTES"));

        // What firing must NOT do. The outward decision is the action gate's,
        // and the gate is cognition — the runtime knows the date passed, not
        // what to do about it.
        assert!(!command.contains("action_gate"), "{command}");
        assert!(!command.contains("SleepTask"), "{command}");

        // A delta fire is keyed by the change it fired on.
        let delta = fire_watch_request(&row("C-8", "delta"), "2026-08-31T00:00:00Z", Fire::Delta(52), None);
        let parameters = delta.parameters.as_ref().unwrap();
        assert_eq!(parameters["fire_key"], Json::from("watch_fire:C-8:delta:52"));
        assert_eq!(parameters["matched_seq"], Json::from(52));
        assert!(!delta.operations[0].command.as_deref().unwrap().contains("evaluated_seq"));
    }

    #[test]
    fn a_matched_silence_watch_stands_down_without_firing() {
        let request = disarm_matched_request(&row("C-9", "silence"), "2026-08-31T00:00:00Z", 52);
        let command = request.operations[0].command.as_deref().unwrap();
        assert!(command.contains(r#"status: "disarmed""#));
        assert!(!command.contains("watch_fire"), "{command}");
        assert!(command.contains("EXPECT VERSION :version OF ATTRIBUTES"));
        assert_eq!(request.parameters.as_ref().unwrap()["matched_seq"], Json::from(52));

        let stamp = stamp_request(&row("C-9", "silence"), "due_seen_seq", 61);
        let command = stamp.operations[0].command.as_deref().unwrap();
        assert!(command.contains("due_seen_seq: :seq"), "{command}");
        assert!(command.contains("EXPECT VERSION :version OF ATTRIBUTES"));
    }
}

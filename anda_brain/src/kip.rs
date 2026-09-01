//! The KIP 2.0 envelope seam.
//!
//! KIP 2.0 replaced the 1.x request/response pair — one `command` string in, an
//! `Ok`/`Err` enum out — with a batch envelope: `operations[]` in, a
//! [`Response`] carrying a request-level status plus one
//! [`OperationResult`](anda_kip::OperationResult) per operation out. Almost
//! every read and write the brain issues is one deterministic, code-built
//! command, so this module keeps that case a one-liner instead of spreading
//! envelope construction and two-level error handling over thirty call sites.
//!
//! Two distinctions the helpers deliberately preserve rather than flatten:
//!
//! - a failure lives at the operation level for an ordinary error and at the
//!   request level only for an envelope error, so reading just one of them
//!   would turn half of the failures into an empty success;
//! - [`TopLevelStatus::OutcomeUnknown`] is not a failure. A write may have
//!   committed, so [`succeeded`] answers `false` without licensing a caller to
//!   redo the work: the settlement passes recover by re-running an idempotent
//!   write, never by treating the memory as unwritten.

use anda_kip::{
    Command, ErrorObject, Executor, Json, KipError, KipValue, Map, MutationClause, Request,
    Response, Scalar, TopLevelStatus, execute_request,
};

/// Builds a single-operation request from one command string.
pub fn request(command: impl Into<String>) -> Request {
    Request::single(command)
}

/// Builds a single-operation request with its parameter bindings.
///
/// Parameters are bound structurally to complete value positions, never spliced
/// into the command text: that is what keeps a value that came from a model or
/// a user from being read as syntax.
pub fn request_with(command: impl Into<String>, parameters: Map<String, Json>) -> Request {
    Request {
        parameters: Some(parameters),
        ..Request::single(command)
    }
}

/// Builds the parameter map for a single binding.
pub fn param(name: &str, value: impl Into<Json>) -> Map<String, Json> {
    Map::from_iter([(name.to_string(), value.into())])
}

/// Whether every operation in the response committed or answered.
///
/// `outcome_unknown` is not success and not failure; see the module docs.
pub fn succeeded(response: &Response) -> bool {
    response.status == TopLevelStatus::Succeeded
}

/// The error a response reports, whichever level it sits at.
pub fn error_of(response: &Response) -> Option<&ErrorObject> {
    response
        .error
        .as_ref()
        .or_else(|| response.results.first().and_then(|r| r.error.as_ref()))
}

/// The first operation's result value, or `None` when the request did not
/// succeed.
///
/// A succeeded operation that produced no value (`no_effect`) also answers
/// `None`: there is nothing to read either way, and the caller that wants to
/// tell them apart asks [`succeeded`].
pub fn ok_result(response: &Response) -> Option<&Json> {
    if !succeeded(response) {
        return None;
    }
    response.results.first().and_then(|r| r.result.as_ref())
}

/// Renders whatever a response failed with as one log-friendly line.
pub fn error_message(response: &Response) -> String {
    match error_of(response) {
        Some(error) => format!("{}: {}", error.code, error.message),
        None => format!("{:?}", response.status),
    }
}

/// How many elements a committed mutation changed under the given op.
///
/// A KML result is `{handles, changes: [{id, kind, op, version}, ...]}`; there
/// is no scalar "updated" count, and a sweep that reported the whole `changes`
/// length would count the elements a single statement touched incidentally.
pub fn changed(response: &Response, op: &str) -> u64 {
    let Some(result) = ok_result(response) else {
        return 0;
    };
    result
        .get("changes")
        .and_then(Json::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter(|change| change.get("op").and_then(Json::as_str) == Some(op))
                .count() as u64
        })
        .unwrap_or(0)
}

/// Runs a whole request envelope on a read-only path.
///
/// [`anda_kip::execute_readonly`] gates a single command; a batch needs the
/// same gate applied to every operation before any of them runs. The rejection
/// is on what each command parses to, never on the `language` label an
/// operation declares, so no envelope field can talk a write past this
/// boundary.
pub async fn execute_readonly_request(executor: &impl Executor, request: &Request) -> Response {
    match request.parse_operations() {
        Ok(commands) => {
            if commands.iter().any(|command| command.is_mutation()) {
                return Response::from(KipError::readonly_violation(
                    "this endpoint executes KQL and META only; KML mutations must go through the \
                     state-capable path",
                ))
                .with_request_id(request.request_id.clone());
            }
        }
        Err(err) => return Response::from(err).with_request_id(request.request_id.clone()),
    }

    execute_request(executor, request).await
}

/// The most elements one gated mutation clause may select.
///
/// Matches `MAX_MAINTENANCE_SELECTION` in the Worker's `src/kip.ts`: the two
/// engines have to agree on a bound both deployment contracts quote.
pub const MAX_GATED_SELECTION: u64 = 20;

/// Runs a whole request envelope on the cognition-only path.
///
/// Formation encodes what it observed: Concepts, the Propositions relating
/// them, the Evidence they rest on, the Assertions that take a stance, and the
/// corrections that revise one — which in KIP 2.0 is a *new* Assertion plus
/// supersession, so `SUPERSEDE` and `CORRECT EVIDENCE` belong here even though
/// they change a claim's standing.
///
/// What is refused is refused deliberately. `UPDATE`, `SET RETENTION`,
/// `ARCHIVE`, `TOMBSTONE`, `PURGE`, `PURGE PAYLOAD` and `MERGE CONCEPT` act on
/// memory in bulk from a selection, and a pass whose entire input is an
/// untrusted conversation is the last thing that should hold them. The
/// reference policy assumes an authority model this deployment cannot express
/// — every agent runs as the Space's system Principal, and the KIP request
/// envelope carries no Principal of its own, so Governance grants cannot
/// separate the agents — and this gate is where that separation lives instead.
/// [`execute_maintenance_request`] is the wider half of it.
///
/// Reads are untouched: Formation has to ground before it writes (reference
/// policy §11), so KQL and META pass through.
///
/// Like [`execute_readonly_request`], the decision is on what each command
/// parses to, never on a label the caller attached to it.
pub async fn execute_cognition_request(executor: &impl Executor, request: &Request) -> Response {
    let commands = match request.parse_operations() {
        Ok(commands) => commands,
        Err(err) => return Response::from(err).with_request_id(request.request_id.clone()),
    };
    for command in &commands {
        if let Some(refusal) = cognition_refusal(command, request.parameters.as_ref()) {
            return Response::from(KipError::not_authorized(refusal))
                .with_request_id(request.request_id.clone());
        }
    }

    execute_request(executor, request).await
}

/// Why the cognition-only path refuses this command, if it does.
fn cognition_refusal(command: &Command, parameters: Option<&Map<String, Json>>) -> Option<String> {
    let Command::Kml(statement) = command else {
        return None;
    };
    for clause in &statement.clauses {
        let verb = match clause {
            // `RETRACT ASSERTION` is the only one of these that selects, and
            // the shared bound below is what keeps an unbounded
            // `RETRACT ASSERTION ?a WHERE { ?a ASSERTION {} }` from
            // withdrawing every claim in the Space in one statement.
            MutationClause::CreateConcept(_)
            | MutationClause::UpsertConcept(_)
            | MutationClause::EnsureProposition(_)
            | MutationClause::CreateEvidence(_)
            | MutationClause::CreateAssertion(_)
            | MutationClause::CreateActivity(_)
            | MutationClause::SupersedeAssertion(_)
            | MutationClause::CorrectEvidence(_)
            | MutationClause::TransitionActivity(_)
            | MutationClause::RetractAssertion(_) => {
                if let Some(refusal) = unbounded_selection(clause, parameters) {
                    return Some(refusal);
                }
                continue;
            }

            MutationClause::Update(_) => "UPDATE",
            MutationClause::SetRetention(_) => "SET RETENTION",
            MutationClause::Archive(_) => "ARCHIVE",
            MutationClause::Tombstone(_) => "TOMBSTONE",
            MutationClause::Purge(_) => "PURGE",
            MutationClause::PurgePayload(_) => "PURGE PAYLOAD",
            MutationClause::MergeConcept(_) => "MERGE CONCEPT",
        };

        return Some(format!(
            "formation cannot issue {verb}; it writes cognition (CREATE / UPSERT / ENSURE / \
             ASSERT) and corrects it (SUPERSEDE / RETRACT / CORRECT EVIDENCE). Administering \
             memory in bulk belongs to maintenance"
        ));
    }
    None
}

/// Runs a whole request envelope on the maintenance path.
///
/// Maintenance administers memory, so it keeps the verbs Formation does not:
/// `UPDATE`, `SET RETENTION`, `ARCHIVE`, `TOMBSTONE` and `MERGE CONCEPT` are
/// the custodial work the reference policy asks it for. Three things it still
/// does not get, and each for its own reason:
///
/// - **`PURGE` and `PURGE PAYLOAD`.** Erasure is irreversible, and a model
///   reading a snapshot of its own graph is not where "this should stop having
///   existed" gets decided. This deployment already erases — the right-to-be-
///   forgotten path in [`crate::space`] issues `PURGE` itself, deterministically,
///   from a request a person made. `PURGE PAYLOAD` is refused on the same
///   grounds rather than lesser ones: it leaves the Evidence record, its digest
///   and its citations standing and destroys the bytes underneath them, so an
///   Assertion keeps citing an observation whose content is gone.
/// - **`legal_hold`, in either direction.** A hold blocks erasure for everyone
///   (Spec §60.3), so a plan that could place one could make its own cognition
///   undeletable, and one that could lift one could unblock an erasure somebody
///   placed a hold to stop. Setting a retention class and an `expires_at` is
///   ordinary lifecycle judgement and stays; the hold is not.
/// - **An unbounded selection.** `ARCHIVE ?e WHERE { ?e CONCEPT {} }` and
///   `UPDATE ?e SET ATTRIBUTES {…} WHERE { ?e CONCEPT {} }` are one hazard
///   wearing two verbs, so the bound is on the selection rather than on a verb
///   by name.
///
/// `MERGE CONCEPT` takes no `LIMIT` and needs none: both engines resolve each
/// operand to exactly one Concept and refuse a pattern that binds several,
/// which is a better answer than anything this gate could give.
///
/// Matches `assertMaintenanceOperations` in the Worker's `src/kip.ts`. The two
/// deployments run different engines and the same policy, and a verb one of
/// them refuses is not a verb the other may quietly keep.
pub async fn execute_maintenance_request(executor: &impl Executor, request: &Request) -> Response {
    let commands = match request.parse_operations() {
        Ok(commands) => commands,
        Err(err) => return Response::from(err).with_request_id(request.request_id.clone()),
    };
    for command in &commands {
        if let Some(refusal) = maintenance_refusal(command, request.parameters.as_ref()) {
            return Response::from(KipError::not_authorized(refusal))
                .with_request_id(request.request_id.clone());
        }
    }

    execute_request(executor, request).await
}

/// Why the maintenance path refuses this command, if it does.
fn maintenance_refusal(
    command: &Command,
    parameters: Option<&Map<String, Json>>,
) -> Option<String> {
    let Command::Kml(statement) = command else {
        return None;
    };
    for clause in &statement.clauses {
        match clause {
            MutationClause::Purge(_) | MutationClause::PurgePayload(_) => {
                return Some(
                    "maintenance cannot issue PURGE or PURGE PAYLOAD; erasure is irreversible \
                     and this deployment runs it deterministically from a person's forget \
                     request, never from a maintenance plan. ARCHIVE and TOMBSTONE are yours"
                        .to_string(),
                );
            }

            // Checked on the member name, which the grammar fixes, so a
            // parameterised value cannot smuggle it past: `{legal_hold: :x}`
            // is refused on the name alone, before anything is evaluated.
            MutationClause::SetRetention(retention)
                if retention
                    .values
                    .iter()
                    .any(|(name, _)| name == "legal_hold") =>
            {
                return Some(
                    "maintenance cannot place or lift a legal hold; set a retention class and \
                     an expires_at, and leave the hold to a person"
                        .to_string(),
                );
            }

            _ => {}
        }

        if let Some(refusal) = unbounded_selection(clause, parameters) {
            return Some(refusal);
        }
    }
    None
}

/// Why this clause's selection is too wide, if it is.
///
/// `None` for a clause that names its target outright: the bound exists to
/// stop a *pattern* from reaching further than the writer meant, and
/// `ARCHIVE "C-7"` reaches exactly one element by construction.
fn unbounded_selection(
    clause: &MutationClause,
    parameters: Option<&Map<String, Json>>,
) -> Option<String> {
    let (verb, where_clauses, limit) = match clause {
        MutationClause::Update(c) => ("UPDATE", &c.where_clauses, &c.limit),
        MutationClause::RetractAssertion(c) => ("RETRACT ASSERTION", &c.where_clauses, &c.limit),
        MutationClause::SetRetention(c) => ("SET RETENTION", &c.where_clauses, &c.limit),
        MutationClause::Archive(c) => ("ARCHIVE", &c.where_clauses, &c.limit),
        MutationClause::Tombstone(c) => ("TOMBSTONE", &c.where_clauses, &c.limit),
        MutationClause::Purge(c) => ("PURGE", &c.where_clauses, &c.limit),
        MutationClause::PurgePayload(c) => ("PURGE PAYLOAD", &c.where_clauses, &c.limit),
        _ => return None,
    };
    if where_clauses
        .as_ref()
        .is_none_or(|clauses| clauses.is_empty())
    {
        return None;
    }
    match selection_limit(limit.as_ref(), parameters) {
        Some(limit) if limit <= MAX_GATED_SELECTION => None,
        _ => Some(format!(
            "a {verb} that selects with WHERE must carry LIMIT {MAX_GATED_SELECTION} or less"
        )),
    }
}

/// The positive integer a `LIMIT` slot holds, resolving a `:parameter` against
/// the request's own bindings.
///
/// Refusing every parameterised limit would push a model toward splicing the
/// number into the command text, which is the habit binding exists to break.
fn selection_limit(limit: Option<&Scalar>, parameters: Option<&Map<String, Json>>) -> Option<u64> {
    match limit? {
        Scalar::Literal(KipValue::Number(number)) => number.as_u64(),
        Scalar::Param(name) => parameters?.get(name)?.as_u64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_kip::{KipError, OperationResult};

    #[test]
    fn a_single_command_request_carries_one_operation() {
        let req = request("DESCRIBE PRIMER");
        assert_eq!(req.kip, "2.0");
        assert_eq!(req.operations.len(), 1);
        assert_eq!(
            req.operations[0].command.as_deref(),
            Some("DESCRIBE PRIMER")
        );
        req.validate().unwrap();
    }

    #[test]
    fn parameters_ride_the_envelope_rather_than_the_command_text() {
        let req = request_with("FIND(?x) WHERE { ?x {type: :t} }", param("t", "Person"));
        assert_eq!(req.parameters.as_ref().unwrap()["t"], Json::from("Person"));
        req.validate().unwrap();
    }

    #[test]
    fn an_operation_level_failure_is_not_read_as_an_empty_success() {
        let response = Response::from_results(vec![OperationResult::failed(
            KipError::not_found_or_not_visible("nothing here"),
        )]);
        assert!(response.error.is_none(), "the failure is per-operation");
        assert!(!succeeded(&response));
        assert_eq!(ok_result(&response), None);
        assert!(error_of(&response).is_some());
    }

    #[test]
    fn an_unknown_outcome_does_not_read_as_success() {
        // Spec §80.3: the write may have committed, so this is not a failure
        // either — a settlement pass answers it by retrying an idempotent
        // write, never by treating the memory as unwritten.
        let response = Response::outcome_unknown(KipError::outcome_unknown("connection dropped"));
        assert!(!succeeded(&response));
        assert_eq!(response.status, TopLevelStatus::OutcomeUnknown);
    }

    /// The one command in a request, parsed.
    fn parsed(command: &str) -> Command {
        request(command)
            .parse_operations()
            .expect("the command parses")
            .remove(0)
    }

    #[test]
    fn the_cognition_gate_admits_what_formation_encodes() {
        for command in [
            // Reads: Formation grounds before it writes (§11).
            r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person"} } LIMIT 5"#,
            "LIST PREDICATES LIMIT 100",
            "DESCRIBE PRIMER",
            // The everyday write, and its desugaring.
            r#"MUTATE { ASSERT (:alice, "prefers", :dark) { by: :alice, mode: "stated" } }"#,
            r#"MUTATE {
  CREATE EVIDENCE ?e { SET FIELDS { evidence_class: "user_statement", payload: :p } }
  UPSERT CONCEPT ?a { MATCH {type: "Person", key: :k} SET FIELDS {name: :n} }
  ENSURE PROPOSITION ?p2 (?a, "prefers", :dark)
  CREATE ASSERTION ?as { SET FIELDS { proposition: ?p2, asserted_by: ?a, stance: "support" } }
  CREATE ACTIVITY ?act { SET FIELDS { activity_class: "extraction", status: "completed" } }
}"#,
            // Correction preserves history, so its verbs are cognition too.
            r#"MUTATE { ASSERT (:alice, "prefers", :light) { by: :alice, mode: "stated" } SUPERSEDING :old }"#,
            r#"CORRECT EVIDENCE :old BY :new"#,
            r#"TRANSITION ACTIVITY :act TO "completed""#,
            // A bounded retraction of one's own claim.
            r#"RETRACT ASSERTION ?a WHERE { ?a ASSERTION {asserted_by: :alice} } LIMIT 5"#,
            r#"RETRACT ASSERTION :a"#,
        ] {
            assert_eq!(
                cognition_refusal(&parsed(command), None),
                None,
                "should be admitted: {command}"
            );
        }
    }

    #[test]
    fn the_cognition_gate_refuses_administering_memory_in_bulk() {
        // Every one of these is reachable from a conversation the Brain did
        // not write, and every one of them acts on memory the conversation
        // never mentioned.
        for (command, verb) in [
            (
                r#"UPDATE ?c SET FACET "MnemonicState" {salience: 0.1} WHERE { ?c CONCEPT {} } LIMIT 5"#,
                "UPDATE",
            ),
            (
                r#"SET RETENTION ?c { retention_class: "standard" } WHERE { ?c CONCEPT {} } LIMIT 5"#,
                "SET RETENTION",
            ),
            (r#"ARCHIVE ?c WHERE { ?c CONCEPT {} } LIMIT 5"#, "ARCHIVE"),
            (
                r#"TOMBSTONE ?c WHERE { ?c CONCEPT {} } LIMIT 5"#,
                "TOMBSTONE",
            ),
            (
                r#"PURGE ?c WHERE { ?c CONCEPT {} } LIMIT 5 CONFIRM "PURGE""#,
                "PURGE",
            ),
            (
                r#"PURGE PAYLOAD ?e WHERE { ?e EVIDENCE {} } LIMIT 5 CONFIRM "PURGE""#,
                "PURGE PAYLOAD",
            ),
            (r#"MERGE CONCEPT :dup INTO :canonical"#, "MERGE CONCEPT"),
        ] {
            let refusal = cognition_refusal(&parsed(command), None)
                .unwrap_or_else(|| panic!("should be refused: {command}"));
            assert!(refusal.contains(verb), "{refusal}");
        }
    }

    #[test]
    fn a_selecting_retraction_must_say_how_much_it_may_withdraw() {
        // Allowed by verb, refused by blast radius: without a bound this
        // withdraws every claim in the Space in one statement.
        let unbounded = r#"RETRACT ASSERTION ?a WHERE { ?a ASSERTION {} }"#;
        assert!(cognition_refusal(&parsed(unbounded), None).is_some());

        let over_budget = r#"RETRACT ASSERTION ?a WHERE { ?a ASSERTION {} } LIMIT 500"#;
        assert!(cognition_refusal(&parsed(over_budget), None).is_some());

        // A parameterised limit resolves against the envelope's own bindings,
        // so a model is not pushed into splicing the number into the text.
        let bound = r#"RETRACT ASSERTION ?a WHERE { ?a ASSERTION {} } LIMIT :n"#;
        assert_eq!(
            cognition_refusal(&parsed(bound), Some(&param("n", 5))),
            None
        );
        assert!(cognition_refusal(&parsed(bound), Some(&param("n", 5000))).is_some());
        // An unbound `:n` is not a bound at all.
        assert!(cognition_refusal(&parsed(bound), None).is_some());
    }

    #[test]
    fn the_cognition_gate_reads_the_command_not_the_label() {
        // The refusal is on what the text parses to. A `MUTATE` that opens
        // with a legal clause and closes with `PURGE` is the purge it is.
        let smuggled = r#"MUTATE {
  CREATE ACTIVITY ?a { SET FIELDS { activity_class: "extraction", status: "completed" } }
  PURGE :victim CONFIRM "PURGE"
}"#;
        let refusal = cognition_refusal(&parsed(smuggled), None).expect("refused");
        assert!(refusal.contains("PURGE"), "{refusal}");
    }

    #[test]
    fn the_maintenance_gate_admits_the_custodial_verbs() {
        for command in [
            // Reads, and the vocabulary review §15 asks for.
            r#"FIND(?c.id) WHERE { ?c CONCEPT {} } LIMIT 20"#,
            "LIST PREDICATES LIMIT 500",
            // The custodial work the reference policy asks maintenance for.
            r#"UPDATE ?c SET FACET "MnemonicState" {salience: 0.9} WHERE { ?c CONCEPT {} } LIMIT 20"#,
            r#"ARCHIVE ?c WHERE { ?c CONCEPT {} } LIMIT 20"#,
            r#"TOMBSTONE ?c WHERE { ?c CONCEPT {} } LIMIT 20"#,
            // Verbatim from the deployment contract: `STRUCTURAL` over a Core
            // reference field is how §16 and §26 find what a corrected
            // observation was resting under.
            r#"ARCHIVE ?a WHERE { STRUCTURAL (?a, "evidence", :e) } LIMIT 20"#,
            // Retention without a hold: §20's judgement, which is a model's.
            r#"SET RETENTION ?e { retention_class: "standard", expires_at: "2027-01-01T00:00:00Z" } WHERE { ?e EVIDENCE {} } LIMIT 20"#,
            // Naming the target outright reaches one element by construction,
            // so there is nothing for a bound to do.
            "ARCHIVE \"C-7\"",
            // No LIMIT slot, and none needed: both engines refuse an operand
            // that binds more than one Concept.
            r#"MERGE CONCEPT ?dup INTO ?canonical WHERE { ?dup CONCEPT {key: "alice-2"} ?canonical CONCEPT {key: "alice"} }"#,
        ] {
            assert_eq!(
                maintenance_refusal(&parsed(command), None),
                None,
                "should be admitted: {command}"
            );
        }
    }

    #[test]
    fn the_maintenance_gate_refuses_erasure_and_holds() {
        for (command, expected) in [
            // Erasure is a person's decision, run deterministically.
            (
                r#"PURGE ?c WHERE { ?c CONCEPT {} } LIMIT 5 CONFIRM "PURGE""#,
                "PURGE",
            ),
            (r#"PURGE :victim CONFIRM "PURGE""#, "PURGE"),
            (r#"PURGE PAYLOAD :e CONFIRM "PURGE""#, "PURGE PAYLOAD"),
            // Placing a hold and lifting one are the same gate: `SET
            // RETENTION` replaces the block rather than patching it.
            (
                r#"SET RETENTION :e { retention_class: "standard", legal_hold: true }"#,
                "legal hold",
            ),
            (
                r#"SET RETENTION :e { retention_class: "standard", legal_hold: false }"#,
                "legal hold",
            ),
            // The member name is fixed by the grammar, so a parameterised
            // value never reaches evaluation.
            (
                r#"SET RETENTION :e { legal_hold: :whatever }"#,
                "legal hold",
            ),
            // One hazard, several verbs: the bound is on the selection.
            (r#"ARCHIVE ?c WHERE { ?c CONCEPT {} }"#, "LIMIT 20 or less"),
            (
                r#"UPDATE ?c SET FACET "MnemonicState" {salience: 0.9} WHERE { ?c CONCEPT {} } LIMIT 500"#,
                "LIMIT 20 or less",
            ),
        ] {
            let refusal = maintenance_refusal(&parsed(command), None)
                .unwrap_or_else(|| panic!("should be refused: {command}"));
            assert!(refusal.contains(expected), "{command}: {refusal}");
        }
    }

    /// The two gates are one policy read from two sides, so a verb the wider
    /// one holds back must not be reachable through the narrower one either.
    #[test]
    fn nothing_formation_may_write_is_something_maintenance_may_not() {
        for command in [
            r#"MUTATE { ASSERT (:alice, "prefers", :dark) { by: :alice, mode: "stated" } }"#,
            r#"CORRECT EVIDENCE :old BY :new"#,
            r#"RETRACT ASSERTION ?a WHERE { ?a ASSERTION {asserted_by: :alice} } LIMIT 5"#,
        ] {
            let command = parsed(command);
            assert_eq!(cognition_refusal(&command, None), None);
            assert_eq!(maintenance_refusal(&command, None), None);
        }
    }
}

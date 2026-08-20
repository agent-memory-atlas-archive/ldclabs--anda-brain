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
    ErrorObject, Executor, Json, KipError, Map, Request, Response, TopLevelStatus, execute_request,
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
}

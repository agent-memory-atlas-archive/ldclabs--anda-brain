//! Typed read observations. Retrieval, record existence and projected belief
//! are different questions; no row count or LLM verdict may stand in for BELIEF.

#[cfg(test)]
mod tests;

use anda_core::{BoxError, Json};
use anda_kip::{
    AggregationFunction, BeliefStatus, Command, FindExpression, MetaCommand, OperationStatus,
    Projection, Request, Response, TopLevelStatus, WhereClause,
};

/// The successful value of one read operation. Partial/unknown envelopes and
/// operation-level failures are not empty successful reads.
pub fn single_read_result(response: &Response) -> Result<&Json, BoxError> {
    if response.kip != "2.0"
        || response.status != TopLevelStatus::Succeeded
        || response.error.is_some()
        || response.results.len() != 1
    {
        return Err(format!("read envelope did not succeed: {response:?}").into());
    }
    let operation = &response.results[0];
    if operation.status != OperationStatus::Succeeded || operation.error.is_some() {
        return Err(format!("read operation did not succeed: {operation:?}").into());
    }
    operation
        .result
        .as_ref()
        .ok_or_else(|| "read operation returned no result".into())
}

/// What a SEARCH actually returned, without assigning epistemic meaning.
#[derive(Debug)]
pub struct SearchObservation<'a> {
    pub hits: &'a [Json],
    /// Missing coverage is unknown, not an assertion that the index was complete.
    /// A remaining page always makes this observation non-exhaustive.
    pub exhaustive: Option<bool>,
}

/// Reads the KIP 2 SEARCH container, retaining unknown coverage. Even an
/// exhaustive empty search says nothing about the truth or falsity of a claim.
pub fn search_observation(response: &Response) -> Result<SearchObservation<'_>, BoxError> {
    let result = single_read_result(response)?;
    let hits = result
        .get("hits")
        .and_then(Json::as_array)
        .ok_or("SEARCH result is missing its hits array")?;
    let _context = result
        .get("search_context")
        .and_then(Json::as_object)
        .ok_or("SEARCH result is missing its search_context")?;
    for hit in hits {
        let id = hit.get("id").and_then(Json::as_str);
        let element = hit.get("element").and_then(Json::as_object);
        if id.is_none()
            || element.is_none()
            || element
                .and_then(|element| element.get("id"))
                .and_then(Json::as_str)
                != id
        {
            return Err("SEARCH hit is missing its matching element".into());
        }
    }
    let exhaustive = match result.get("exhaustive") {
        None => None,
        Some(Json::Bool(value)) => Some(*value),
        Some(_) => return Err("SEARCH exhaustive coverage must be a boolean".into()),
    };
    Ok(SearchObservation {
        hits,
        exhaustive: if has_cursor(response) {
            Some(false)
        } else {
            exhaustive
        },
    })
}

#[derive(Debug)]
pub enum ProbeObservation {
    /// Explicit raw FIND/COUNT: record rows, never accepted belief.
    Rows { count: usize, complete: bool },
    /// SEARCH has no truth verdict, even when it returns one matching Concept.
    Search { hit_count: usize },
    /// The engine's full single-Proposition projection, including its basis.
    Belief(Box<Projection>),
}

impl ProbeObservation {
    pub fn hit_count(&self) -> usize {
        match self {
            Self::Rows { count, .. } => *count,
            Self::Search { hit_count } => *hit_count,
            Self::Belief(_) => 1,
        }
    }

    /// Whether an explicitly raw FIND/COUNT returned records. This is not a
    /// claim about any Proposition's accepted truth.
    pub fn raw_presence(&self) -> Option<bool> {
        match self {
            Self::Rows { count, complete } if *count > 0 || *complete => Some(*count > 0),
            _ => None,
        }
    }

    /// Only the engine's final belief status can decide this. Neither raw
    /// record existence nor SEARCH supplies a belief verdict.
    pub fn belief_holds(&self) -> Option<bool> {
        match self {
            Self::Belief(projection) => match projection.status {
                BeliefStatus::Accepted => Some(true),
                BeliefStatus::Rejected => Some(false),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Interprets only the actual parsed operation. A field named `status` in a
/// raw Concept or its attributes cannot masquerade as an engine projection.
pub fn probe_observation(
    request: &Request,
    response: &Response,
) -> Result<ProbeObservation, BoxError> {
    request.validate()?;
    if request.operations.len() != 1 {
        return Err("one expectation requires exactly one read operation".into());
    }
    let result = single_read_result(response)?;
    match request.operations[0].parse()? {
        Command::Meta(MetaCommand::Search(_)) => Ok(ProbeObservation::Search {
            hit_count: search_observation(response)?.hits.len(),
        }),
        Command::Kql(query) => {
            if query.where_clauses.iter().any(has_belief) {
                let [FindExpression::Variable(projected)] =
                    query.find_clause.expressions.as_slice()
                else {
                    return Err(
                        "a belief expectation must return one full BELIEF projection".into(),
                    );
                };
                if !projected.path.is_empty()
                    || !query.where_clauses.iter().any(|clause| {
                        matches!(clause,
                        WhereClause::Belief { variable, .. } if variable == &projected.var)
                    })
                {
                    return Err(
                        "a belief expectation must directly project its BELIEF variable".into(),
                    );
                }
                if has_cursor(response) {
                    return Err(
                        "BELIEF probe has additional candidate rows; its target is ambiguous"
                            .into(),
                    );
                }
                let rows = result
                    .as_array()
                    .ok_or("BELIEF query did not return rows")?;
                let [row] = rows.as_slice() else {
                    return Err(
                        "BELIEF query did not return one unambiguous projection; state is unknown"
                            .into(),
                    );
                };
                let projection: Projection = serde_json::from_value(row.clone())?;
                if projection.basis.is_none()
                    || projection.policy.is_none()
                    || projection
                        .slot_status
                        .is_some_and(|status| status != projection.status)
                {
                    return Err("BELIEF projection is missing its basis or carries inconsistent final status".into());
                }
                return Ok(ProbeObservation::Belief(Box::new(projection)));
            }
            let rows = result
                .as_array()
                .ok_or("raw FIND query did not return rows")?;
            let count = if matches!(
                query.find_clause.expressions.as_slice(),
                [FindExpression::Aggregation {
                    func: AggregationFunction::Count,
                    ..
                }]
            ) {
                let [count] = rows.as_slice() else {
                    return Err("COUNT query did not return one count".into());
                };
                usize::try_from(
                    count
                        .as_u64()
                        .ok_or("COUNT query did not return a nonnegative integer")?,
                )?
            } else {
                rows.len()
            };
            Ok(ProbeObservation::Rows {
                count,
                complete: !has_cursor(response),
            })
        }
        _ => Err(
            "an existence expectation requires raw FIND; a truth expectation requires BELIEF"
                .into(),
        ),
    }
}

fn has_belief(clause: &WhereClause) -> bool {
    match clause {
        WhereClause::Belief { .. } | WhereClause::BeliefSlot { .. } => true,
        WhereClause::Not(clauses)
        | WhereClause::Optional(clauses)
        | WhereClause::Union(clauses) => clauses.iter().any(has_belief),
        _ => false,
    }
}

fn has_cursor(response: &Response) -> bool {
    response.next_cursor.is_some()
        || response
            .results
            .iter()
            .any(|operation| operation.next_cursor.is_some())
}

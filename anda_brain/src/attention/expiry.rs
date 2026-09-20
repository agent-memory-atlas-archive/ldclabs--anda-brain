//! Recognize only directly returned, present-time native BELIEF projections.
//! Arbitrary stored JSON or historical/what-if queries cannot create timers.
use super::*;
use anda_kip::{
    Command, FindExpression, Projection, ProjectionBasis, Request, Response, WhereClause,
};

pub(super) fn earliest_basis(request: &Request, response: &Response) -> Option<ProjectionBasis> {
    if !crate::kip::succeeded(response) {
        return None;
    }
    let mut earliest: Option<(u64, ProjectionBasis)> = None;
    let now = anda_engine::unix_ms();
    for (operation, result) in request.operations.iter().zip(&response.results).take(16) {
        let Ok(Command::Kql(query)) = operation.parse() else {
            continue;
        };
        if query.as_of.is_some() || query.for_time.is_some() {
            continue;
        }
        let Some(rows) = result.result.as_ref().and_then(|r| r.as_array()) else {
            continue;
        };
        for (column, expression) in query.find_clause.expressions.iter().enumerate().take(32) {
            let FindExpression::Variable(projected) = expression else {
                continue;
            };
            if !projected.path.is_empty() || !query.where_clauses.iter().any(|c|
                matches!(c, WhereClause::Belief { variable, .. } if variable == &projected.var)) { continue; }
            for row in rows.iter().take(200) {
                let value = if query.find_clause.expressions.len() == 1 {
                    row
                } else {
                    &row[column]
                };
                let Ok(projection) = serde_json::from_value::<Projection>(value.clone()) else {
                    continue;
                };
                if projection.policy.is_none() || projection.temporal.is_none() {
                    continue;
                }
                let Some(basis) = projection.basis else {
                    continue;
                };
                let Some(due) = basis
                    .next_invalid_at
                    .as_deref()
                    .and_then(|t| time_ms(t).ok())
                else {
                    continue;
                };
                if due > now && earliest.as_ref().is_none_or(|(old, _)| due < *old) {
                    earliest = Some((due, basis));
                }
            }
        }
    }
    earliest.map(|(_, basis)| basis)
}

use super::*;
use anda_cognitive_nexus::{
    governance::{Permission, ResourceContext},
    nexus::{DEFAULT_SPACE, Session},
};
use serde_json::json;

/// Full current visibility is required before copying graph-derived material
/// out of a protected host journal. Masked records are not an authorization proof.
pub(crate) async fn full_read(session: &Session, id: &str) -> RuntimeResult<Json> {
    let id: anda_cognitive_nexus::ElementId = id.parse().map_err(|_| RuntimeError::NotFound)?;
    let row = session.nexus().store.get_element(id).await?;
    if row.space() != DEFAULT_SPACE
        || row.state() != anda_cognitive_nexus::store::rows::state::ACTIVE
    {
        return Err(RuntimeError::NotFound);
    }
    let auth = session.effective_authority(DEFAULT_SPACE).await?;
    auth.authorize(
        Permission::Read,
        &ResourceContext::of_element(&row),
        session.auth(),
    )
    .into_result()?;
    if !auth
        .may_read(&row, session.auth())
        .is_some_and(|v| v.content && v.constraints.fields.is_empty())
    {
        return Err(RuntimeError::NotFound);
    }
    let reference = id.to_string();
    let kind = match reference.as_bytes()[0] {
        b'C' => "CONCEPT",
        b'P' => "PROPOSITION",
        b'A' => "ASSERTION",
        b'E' => "EVIDENCE",
        b'X' => "ACTIVITY",
        _ => return Err(RuntimeError::NotFound),
    };
    let pattern = if kind == "PROPOSITION" {
        format!("{kind}(id: :id)")
    } else {
        format!("{kind} {{id: :id}}")
    };
    let value = query(
        session,
        &format!("FIND(?x) WHERE {{?x {pattern}}} LIMIT 1"),
        json!({"id":reference}),
    )
    .await?;
    value
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .ok_or(RuntimeError::NotFound)
}
pub(crate) async fn query(
    session: &Session,
    command: &str,
    parameters: Json,
) -> RuntimeResult<Json> {
    let request = crate::kip::request_with(
        command,
        parameters
            .as_object()
            .ok_or_else(|| RuntimeError::Invalid("invalid query parameters".into()))?
            .clone(),
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::kip::execute_readonly_request(session, &request),
    )
    .await
    .map_err(|_| RuntimeError::Unavailable("runtime read timed out".into()))?;
    if let Some(error) = crate::kip::error_of(&response) {
        return Err(anda_kip::KipError::new(
            error
                .parsed_code()
                .unwrap_or(anda_kip::KipErrorCode::InternalError),
            error.message.clone(),
        )
        .into());
    }
    if response
        .results
        .first()
        .is_some_and(|r| r.next_cursor.is_some())
    {
        return Err(RuntimeError::Unavailable(
            "required runtime read is incomplete".into(),
        ));
    }
    crate::kip::ok_result(&response)
        .cloned()
        .ok_or(RuntimeError::NotFound)
}

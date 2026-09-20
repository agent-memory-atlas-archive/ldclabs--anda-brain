use super::*;
use crate::runtime_api::{AttentionQuery, AttentionResponse, RuntimeError, RuntimeStatus};

pub(crate) fn runtime_error(error: RuntimeError) -> AppError {
    use http::StatusCode;
    let status = match &error {
        RuntimeError::Invalid(_) => StatusCode::BAD_REQUEST,
        RuntimeError::Unauthorized => StatusCode::UNAUTHORIZED,
        RuntimeError::Forbidden => StatusCode::FORBIDDEN,
        RuntimeError::NotFound => StatusCode::NOT_FOUND,
        RuntimeError::Conflict(_) => StatusCode::CONFLICT,
        RuntimeError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        RuntimeError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    AppError::with_status(status, error.to_string())
}

pub async fn get_attention(
    State(app): State<AppState>,
    AppPath(space_id): AppPath<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    AppQuery(input): AppQuery<AttentionQuery>,
) -> Result<impl IntoResponse, AppError> {
    let (space, credential) = crate::authz::runtime_credentials(
        &app,
        &space_id,
        &token,
        Some(sharding),
        TokenScope::Read,
    )
    .await
    .map_err(runtime_error)?;
    let runtime = space.memory_runtime().ok_or_else(|| {
        runtime_error(RuntimeError::Unavailable(
            "runtime bindings are not installed".into(),
        ))
    })?;
    let caller = runtime.map_credential(&credential).map_err(runtime_error)?;
    let page = runtime.inbox(&caller, input).await.map_err(runtime_error)?;
    Ok(ct.response(RpcResponse::success(page)))
}

pub async fn post_attention_response(
    State(app): State<AppState>,
    AppPath((space_id, id)): AppPath<(String, String)>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    AppBytes(body): AppBytes,
) -> Result<impl IntoResponse, AppError> {
    let (space, credential) = crate::authz::runtime_credentials(
        &app,
        &space_id,
        &token,
        Some(sharding),
        TokenScope::Write,
    )
    .await
    .map_err(runtime_error)?;
    let runtime = space.memory_runtime().ok_or_else(|| {
        runtime_error(RuntimeError::Unavailable(
            "runtime bindings are not installed".into(),
        ))
    })?;
    let caller = runtime.map_credential(&credential).map_err(runtime_error)?;
    let input = ct
        .parse_body::<AttentionResponse>(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("structured JSON/CBOR response required"))?;
    let result = runtime
        .respond(caller, id, input)
        .await
        .map_err(runtime_error)?;
    Ok(ct.response(RpcResponse::success(result)))
}

pub async fn post_outcome(
    State(app): State<AppState>,
    AppPath(space_id): AppPath<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
    AppBytes(body): AppBytes,
) -> Result<impl IntoResponse, AppError> {
    let (space, credential) = crate::authz::runtime_credentials(
        &app,
        &space_id,
        &token,
        Some(sharding),
        TokenScope::Write,
    )
    .await
    .map_err(runtime_error)?;
    let runtime = space.memory_runtime().ok_or_else(|| {
        runtime_error(RuntimeError::Unavailable(
            "observer bindings are not installed".into(),
        ))
    })?;
    let caller = runtime.map_credential(&credential).map_err(runtime_error)?;
    if !caller.observer {
        return Err(runtime_error(RuntimeError::Forbidden));
    }
    let input = ct
        .parse_body::<crate::consequence::OutcomeInput>(&body)
        .map_err(AppError::bad_request)?
        .value()
        .map_err(|_| AppError::bad_request("structured JSON/CBOR observation required"))?;
    let receipt = runtime
        .submit_outcome(caller, input)
        .await
        .map_err(runtime_error)?;
    Ok(ct.response(RpcResponse::success(receipt)))
}

pub async fn get_runtime_status(
    State(app): State<AppState>,
    AppPath(space_id): AppPath<String>,
    Accept(ct, _): Accept,
    HeaderVals(token, sharding): HeaderVals,
) -> Result<impl IntoResponse, AppError> {
    let (space, credential) = crate::authz::runtime_credentials(
        &app,
        &space_id,
        &token,
        Some(sharding),
        TokenScope::Read,
    )
    .await
    .map_err(runtime_error)?;
    let status = if let Some(runtime) = space.memory_runtime() {
        let caller = runtime.map_credential(&credential).map_err(runtime_error)?;
        runtime
            .status(&caller, app.runtime_cwt_verifier_enabled())
            .await
            .map_err(runtime_error)?
    } else {
        RuntimeStatus::unconfigured()
    };
    Ok(ct.response(RpcResponse::success(status)))
}

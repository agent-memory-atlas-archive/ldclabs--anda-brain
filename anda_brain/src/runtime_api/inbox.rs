use super::*;
use anda_cognitive_nexus::{
    attention::{WakeRecord, WakeResume, WakeState},
    nexus::DEFAULT_SPACE,
};
use ic_auth_types::ByteBufB64;
use serde_json::json;
use std::str::FromStr;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    expires_ms: u64,
    native: Option<String>,
    pending: Vec<String>,
    page_next: Option<String>,
    page_complete: bool,
}
impl MemoryRuntime {
    pub async fn inbox(
        &self,
        caller: &RuntimeCaller,
        input: AttentionQuery,
    ) -> RuntimeResult<AttentionPage> {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.inbox_inner(caller, input),
        )
        .await
        .map_err(|_| RuntimeError::Unavailable("attention read budget exhausted".into()))?
    }
    async fn inbox_inner(
        &self,
        caller: &RuntimeCaller,
        input: AttentionQuery,
    ) -> RuntimeResult<AttentionPage> {
        if !self.bindings.audience.contains(&caller.auth.principal_id) {
            return Err(RuntimeError::Forbidden);
        }
        self.nexus
            .session(caller.auth.clone())
            .effective_authority(DEFAULT_SPACE)
            .await?
            .authorize(
                anda_cognitive_nexus::governance::Permission::Read,
                &anda_cognitive_nexus::governance::ResourceContext::default(),
                &caller.auth,
            )
            .into_result()?;
        let limit = input.limit.unwrap_or(20);
        if !(1..=50).contains(&limit) {
            return Err(RuntimeError::Invalid(
                "attention limit must be 1..=50".into(),
            ));
        }
        let mut cursor = if let Some(token) = input.cursor {
            self.decode_cursor(caller, &token)?
        } else {
            Cursor {
                expires_ms: anda_engine::unix_ms() + 300_000,
                ..Default::default()
            }
        };
        if cursor.pending.is_empty() {
            let page = self
                .nexus
                .system_session()
                .list_wakes(DEFAULT_SPACE, cursor.native.as_deref(), 50)
                .await?;
            cursor.pending = page.items.into_iter().map(|w| w.wake_ref).collect();
            cursor.page_next = page.next_cursor;
            cursor.page_complete = page.complete;
        }
        let mut items = Vec::new();
        let mut bytes = 0;
        while let Some(reference) = cursor.pending.first().cloned() {
            let wake = match self
                .nexus
                .system_session()
                .read_wake(DEFAULT_SPACE, &reference)
                .await
            {
                Ok(w) => w,
                Err(e)
                    if matches!(
                        e.code,
                        anda_kip::KipErrorCode::NotFoundOrNotVisible
                            | anda_kip::KipErrorCode::NotAuthorized
                    ) =>
                {
                    cursor.pending.remove(0);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let item = match self.item(caller, &wake).await {
                Ok(item) => item,
                Err(RuntimeError::Forbidden | RuntimeError::NotFound) => None,
                Err(e) => return Err(e),
            };
            if let Some(item) = item {
                let size = serde_json::to_vec(&item)
                    .map_err(|e| RuntimeError::Storage(e.into()))?
                    .len();
                if size > 131_072 {
                    return Err(RuntimeError::Unavailable(
                        "attention item exceeds read budget".into(),
                    ));
                }
                if !items.is_empty() && (items.len() >= limit || bytes + size > 262_144) {
                    break;
                }
                bytes += size;
                items.push(item);
            }
            cursor.pending.remove(0);
            if items.len() >= limit {
                break;
            }
        }
        let complete = cursor.pending.is_empty() && cursor.page_complete;
        if cursor.pending.is_empty() {
            cursor.native = cursor.page_next.take();
        }
        let next_cursor = if complete {
            None
        } else {
            Some(self.encode_cursor(caller, &cursor)?)
        };
        Ok(AttentionPage {
            scope: self.scope.clone(),
            items,
            next_cursor,
            complete,
        })
    }
    pub(super) async fn item(
        &self,
        caller: &RuntimeCaller,
        wake: &WakeRecord,
    ) -> RuntimeResult<Option<AttentionItem>> {
        if wake.scope != self.scope || !self.bindings.audience.contains(&caller.auth.principal_id) {
            return Ok(None);
        }
        let session = self.nexus.session(caller.auth.clone());
        let watch = full_read(&session, &wake.fire.watch_ref).await?;
        full_read(&session, &wake.fire_activity_ref).await?;
        let inspected = if let Some(action) = self.attention.actions() {
            action.inspect(wake, &caller.auth).await?
        } else {
            crate::action::Inspection::default()
        };
        let recipient = inspected
            .recipient
            .as_ref()
            .or(self.bindings.inbox_recipient.as_ref());
        if !caller.audit_recipients && recipient.is_some_and(|p| p != &caller.auth.principal_id) {
            return Ok(None);
        }
        let (state, reason) = match &wake.state {
            WakeState::Pending { .. } => ("pending", None),
            WakeState::Running { .. } => ("running", None),
            WakeState::Completed { .. } => ("completed", None),
            WakeState::Cancelled { .. } => ("cancelled", None),
            WakeState::Blocked { retry } => (
                "blocked",
                Some(match &retry.resume {
                    WakeResume::At { not_before_ms } => {
                        format!("{}; retry at {not_before_ms}", retry.reason)
                    }
                    WakeResume::OnChange { .. } => retry.reason.clone(),
                }),
            ),
        };
        let status = inspected.status;
        let mut delivery = None;
        if let Some(request) = &inspected.request
            && let Some(row) = self
                .directory
                .read::<config::Delivery>(&config::delivery_key(&self.scope, &request.attempt_id)?)
                .await?
        {
            let row = row.value;
            if row.scope == self.scope
                && (row.recipient == caller.auth.principal_id || caller.audit_recipients)
            {
                if row.request_digest != anda_cognitive_nexus::content_digest(&json!(request))?
                    || row.attempt_id != request.attempt_id
                    || row.gate_wake_ref != request.gate_wake_ref
                    || status.as_ref().and_then(|s| s.attempt_ref.as_deref())
                        != Some(row.attempt_ref.as_str())
                {
                    return Err(RuntimeError::NotFound);
                }
                full_read(&session, &row.attempt_ref).await?;
                delivery = Some(
                    json!({"attempt_id":row.attempt_id,"committed_at_ms":row.committed_at_ms,"request_digest":row.request_digest,"payload":row.payload}),
                );
            }
        }
        Ok(Some(AttentionItem {
            id: item_id(&wake.wake_ref)?,
            wake_ref: wake.wake_ref.clone(),
            parent_id: wake.parent_ref.as_deref().map(item_id).transpose()?,
            watch_ref: wake.fire.watch_ref.clone(),
            fire_activity_ref: wake.fire_activity_ref.clone(),
            summary: watch["attributes"]["summary"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(4096)
                .collect(),
            state: state.into(),
            reason: reason.or_else(|| {
                status
                    .as_ref()
                    .and_then(|s| s.reason.as_ref())
                    .map(|_| "host_work_requires_review".into())
            }),
            decision: inspected.decision,
            decision_ref: status.as_ref().and_then(|s| s.decision_ref.clone()),
            attempt_ref: status.as_ref().and_then(|s| s.attempt_ref.clone()),
            dispatch_ref: status.as_ref().and_then(|s| s.dispatch_ref.clone()),
            clarification: inspected.clarification,
            delivery,
        }))
    }
    fn aad(&self, caller: &RuntimeCaller) -> RuntimeResult<Vec<u8>> {
        serde_json::to_vec(&json!({"format":FORMAT,"scope":self.scope,"principal":caller.auth.principal_id,"config":self.bindings.pin,"auditor":caller.audit_recipients})).map_err(|e|RuntimeError::Storage(e.into()))
    }
    fn encode_cursor(&self, caller: &RuntimeCaller, cursor: &Cursor) -> RuntimeResult<String> {
        let nonce = rand::random::<[u8; 12]>();
        let bytes = serde_json::to_vec(cursor).map_err(|e| RuntimeError::Storage(e.into()))?;
        let encrypted = ic_cose_types::cose::aes::aes256_gcm_encrypt(
            &self.cursor_key,
            &nonce,
            &self.aad(caller)?,
            &bytes,
        )
        .map_err(|_| RuntimeError::Unavailable("cursor encryption unavailable".into()))?;
        let mut token = nonce.to_vec();
        token.extend(encrypted);
        Ok(ByteBufB64::from(token).to_string())
    }
    fn decode_cursor(&self, caller: &RuntimeCaller, token: &str) -> RuntimeResult<Cursor> {
        let bad = || RuntimeError::Invalid("invalid, expired or foreign attention cursor".into());
        if token.len() > 16_384 {
            return Err(bad());
        }
        let bytes = ByteBufB64::from_str(token).map_err(|_| bad())?;
        if bytes.len() < 28 {
            return Err(bad());
        }
        let nonce = <[u8; 12]>::try_from(&bytes[..12]).map_err(|_| bad())?;
        let plaintext = ic_cose_types::cose::aes::aes256_gcm_decrypt(
            &self.cursor_key,
            &nonce,
            &self.aad(caller)?,
            &bytes[12..],
        )
        .map_err(|_| bad())?;
        let cursor: Cursor = serde_json::from_slice(&plaintext).map_err(|_| bad())?;
        if cursor.expires_ms <= anda_engine::unix_ms() || cursor.pending.len() > 50 {
            return Err(bad());
        }
        Ok(cursor)
    }
}

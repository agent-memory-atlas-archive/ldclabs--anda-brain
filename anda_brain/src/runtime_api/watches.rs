//! Narrow, recipient-owned record watches. No arbitrary action or cron adapter.
use super::*;
use anda_cognitive_nexus::{
    governance::{Permission, ResourceContext},
    nexus::DEFAULT_SPACE,
};
use serde_json::json;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordWatch {
    pub operation_id: String,
    pub watch_id: String,
    pub target_id: String,
    pub state: String,
    pub digest: String,
}

impl MemoryRuntime {
    pub async fn record_watch(
        &self,
        caller: &RuntimeCaller,
        operation_id: &str,
    ) -> Result<RecordWatch, BoxError> {
        self.authorize_record_watch(caller).await?;
        let storage = key(
            &self.scope,
            "record-watches",
            &format!("{}:{operation_id}", caller.auth.principal_id),
        )?;
        let mut row = self
            .directory
            .read::<RecordWatch>(&storage)
            .await?
            .ok_or("not_found")?
            .value;
        row.state = watch_state(&self.nexus.session(caller.auth.clone()), &row.watch_id)
            .await?
            .0;
        Ok(row)
    }

    async fn authorize_record_watch(&self, caller: &RuntimeCaller) -> Result<(), BoxError> {
        if self.bindings.inbox_recipient.as_deref() != Some(&caller.auth.principal_id)
            || !self.bindings.audience.contains(&caller.auth.principal_id)
        {
            return Err(RuntimeError::Forbidden.into());
        }
        self.nexus
            .session(caller.auth.clone())
            .effective_authority(DEFAULT_SPACE)
            .await?
            .authorize(Permission::Read, &ResourceContext::default(), &caller.auth)
            .into_result()?;
        Ok(())
    }

    pub async fn create_record_watch(
        self: &Arc<Self>,
        caller: RuntimeCaller,
        operation_id: String,
        target: String,
        summary: String,
    ) -> Result<RecordWatch, BoxError> {
        self.authorize_record_watch(&caller).await?;
        let id: anda_cognitive_nexus::ElementId = target.parse()?;
        if id.kind != anda_kip::ElementKind::Assertion
            || operation_id.is_empty()
            || operation_id.len() > 128
            || summary.is_empty()
            || summary.len() > 4096
        {
            return Err("invalid record watch".into());
        }
        let this = self.clone();
        self.tasks.run(async move {
            let _guard=this.gate.lock().await;
            this.authorize_record_watch(&caller).await?;
            let session=this.nexus.session(caller.auth.clone());
            full_read(&session,&target).await?;
            let storage=key(&this.scope,"record-watches",&format!("{}:{operation_id}",caller.auth.principal_id))?;
            let digest=anda_cognitive_nexus::content_digest(&json!({"target":target,"summary":summary}))?;
            if let Some(existing)=this.directory.read::<RecordWatch>(&storage).await? {
                if existing.value.digest!=digest {return Err("idempotency_conflict".into())}
                let mut row=existing.value;
                row.state=watch_state(&session,&row.watch_id).await?.0;
                return Ok(row);
            }
            this.attention.register_work().await?;
            let mut request=crate::kip::request_with(r#"CREATE CONCEPT ?watch {TYPE "Watch" CLIENT KEY :client_key SET ATTRIBUTES {watch_class:"delta",summary: :summary,status:"disarmed",condition:{element: :target}}}"#,serde_json::Map::from_iter([("client_key".into(),json!(storage)),("summary".into(),json!(summary)),("target".into(),json!(target))]));
            request.operations[0].idempotency_key=Some(storage.clone());
            let response=anda_kip::execute_request(&session,&request).await;
            let watch=crate::kip::ok_result(&response).and_then(|result|result["handles"]["watch"].as_str()).ok_or_else(||crate::kip::error_message(&response))?.to_string();
            let current=full_read(&session,&watch).await?;
            let version=current["_system"]["version"].as_u64().ok_or("watch version missing")?;
            if version==1 {this.authorize_record_watch(&caller).await?;this.attention.provision_watch_cancellation(watch.clone()).await?;this.attention.arm_watch(watch.clone(),version).await?;}
            // A retry after arming/disarming must never re-arm an old Watch.
            let current=full_read(&session,&watch).await?;
            let row=RecordWatch {operation_id,watch_id:watch,target_id:target,state:current["attributes"]["status"].as_str().unwrap_or("unknown").into(),digest};
            this.directory.put(&storage,&row,object_store::PutMode::Create).await?;
            Ok(row)
        }).await
    }

    pub async fn cancel_record_watch(
        self: &Arc<Self>,
        caller: RuntimeCaller,
        operation_id: String,
    ) -> Result<RecordWatch, BoxError> {
        self.authorize_record_watch(&caller).await?;
        let this = self.clone();
        self.tasks
            .run(async move {
                let _guard = this.gate.lock().await;
                this.authorize_record_watch(&caller).await?;
                let storage = key(
                    &this.scope,
                    "record-watches",
                    &format!("{}:{operation_id}", caller.auth.principal_id),
                )?;
                let mut row = this
                    .directory
                    .read::<RecordWatch>(&storage)
                    .await?
                    .ok_or("not_found")?;
                let session = this.nexus.session(caller.auth.clone());
                let (state, version) = watch_state(&session, &row.value.watch_id).await?;
                if state != "cancelled" {
                    this.attention
                        .archive_watch(row.value.watch_id.clone(), version)
                        .await?;
                }
                row.value.state = watch_state(&session, &row.value.watch_id).await?.0;
                this.directory
                    .put(
                        &storage,
                        &row.value,
                        object_store::PutMode::Update(row.version),
                    )
                    .await?;
                Ok(row.value)
            })
            .await
    }
}

// Cancellation archives the native element, preserving its protected generation
// and checkpoint. Native scanners and advance both require active storage state.
async fn watch_state(
    session: &anda_cognitive_nexus::nexus::Session,
    id: &str,
) -> Result<(String, u64), BoxError> {
    let row = session.nexus().store.get_element(id.parse()?).await?;
    if row.space() != DEFAULT_SPACE {
        return Err(RuntimeError::NotFound.into());
    }
    let authority = session.effective_authority(DEFAULT_SPACE).await?;
    authority
        .authorize(
            Permission::Read,
            &ResourceContext::of_element(&row),
            session.auth(),
        )
        .into_result()?;
    if !authority
        .may_read(&row, session.auth())
        .is_some_and(|v| v.content && v.constraints.fields.is_empty())
    {
        return Err(RuntimeError::NotFound.into());
    }
    if row.state() == anda_cognitive_nexus::store::rows::state::ARCHIVED {
        return Ok(("cancelled".into(), row.version()));
    }
    let value = full_read(session, id).await?;
    Ok((
        value["attributes"]["status"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        row.version(),
    ))
}

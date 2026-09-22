//! Durable, conditional memory changes for trusted embedding hosts.
use super::{MemoryRecord, RecordSource, SourceIdentity};
use crate::{
    agents::{FormationAgent, MaintenanceAgent, RecallAgent, SELF_USER_ID},
    kip,
    space::Space,
};
use anda_cognitive_nexus::{ElementId, nexus::DEFAULT_SPACE, store::Element};
use anda_core::{BoxError, Principal, RequestMeta, Tool};
use anda_engine::extension::note::{NoteArgs, NoteTool};
use anda_kip::{Request, TopLevelStatus};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Correct,
    Suppress,
    Delete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeInput {
    pub operation_id: String,
    pub record_id: String,
    pub expected_revision: u64,
    pub kind: ChangeKind,
    pub new_value: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Target {
    pub id: String,
    pub revision: u64,
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChangePreview {
    pub record: MemoryRecord,
    pub new_value: Option<String>,
    pub targets: Vec<Target>,
    pub excluded_sources: BTreeSet<String>,
    pub resets_processing_context: bool,
    pub scope: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChangeReceipt {
    pub schema_version: u32,
    pub operation_id: String,
    pub operation_key: String,
    pub caller: String,
    pub state: String,
    pub preview_digest: String,
    pub expires_at: u64,
    pub preview: ChangePreview,
    pub replacement_record: Option<String>,
    pub source_evidence: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct StoredChange {
    input: ChangeInput,
    input_digest: String,
    receipt: ChangeReceipt,
    requests: Vec<Request>,
    completed_targets: BTreeSet<String>,
}

impl StoredChange {
    fn clear_record_content(&mut self) {
        let record = &mut self.receipt.preview.record;
        record.text.clear();
        record.subject_label.clear();
        record.object_label.clear();
        record.subject = Value::Null;
        record.object = Value::Null;
    }

    fn clear_content(&mut self) {
        self.clear_record_content();
        self.receipt.error = None;
        self.receipt.preview.new_value = None;
        self.input.new_value = None;
        self.requests.clear();
    }
}

fn operation_key(caller: Principal, id: &str) -> Result<String, BoxError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid_request".into());
    }
    Ok(anda_cognitive_nexus::content_digest(
        &json!({"caller":caller.to_string(),"operation_id":id}),
    )?[7..]
        .into())
}

impl Space {
    pub async fn product_prepare(
        self: &Arc<Self>,
        caller: Principal,
        input: ChangeInput,
    ) -> Result<ChangeReceipt, BoxError> {
        let this = self.clone();
        self.product_control
            .tasks
            .run(async move { this.prepare_change_inner(caller, input).await })
            .await
    }

    async fn prepare_change_inner(
        &self,
        caller: Principal,
        input: ChangeInput,
    ) -> Result<ChangeReceipt, BoxError> {
        if caller == Principal::anonymous() {
            return Err("unauthorized".into());
        }
        let key = operation_key(caller, &input.operation_id)?;
        let digest = anda_cognitive_nexus::content_digest(&serde_json::to_value(&input)?)?;
        let _guard = self.product_control.gate.lock().await;
        let path = format!("changes/{key}");
        if self
            .product_control
            .journal
            .read::<bool>(&format!("discarded/{key}"))
            .await?
            .is_some()
        {
            return Err("preview_expired".into());
        }
        if let Some(old) = self
            .product_control
            .journal
            .read::<StoredChange>(&path)
            .await?
        {
            if old.value.input_digest != digest {
                return Err("idempotency_conflict".into());
            }
            return Ok(old.value.receipt);
        }
        if !self.product_control.available() {
            return Err("memory_change_pending".into());
        }
        let record = self.product_record(&input.record_id).await?;
        if record.revision != input.expected_revision {
            return Err("revision_conflict".into());
        }
        if !record.sources_complete {
            return Err("unsupported_scope".into());
        }
        let mut excluded_sources = self.source_keys_for_record(&record).await?;
        let targets = if input.kind != ChangeKind::Correct {
            self.deletion_targets(&record, &mut excluded_sources)
                .await?
        } else {
            if record.status != "active"
                || record.storage_state != "active"
                || record.actor_key.as_deref() != Some(&caller.to_string())
                || record.stance != "support"
            {
                return Err("unsupported_scope".into());
            }
            vec![Target {
                id: record.id.clone(),
                revision: record.revision,
                kind: "assertion".into(),
            }]
        };
        if input.kind == ChangeKind::Correct
            && input.new_value.as_deref().is_none_or(|text| {
                text.trim().is_empty() || text.len() > 8192 || text == record.object_label
            })
        {
            return Err("invalid_request".into());
        }
        if input.kind != ChangeKind::Correct && input.new_value.is_some() {
            return Err("invalid_request".into());
        }
        let preview=ChangePreview {record,new_value:input.new_value.clone(),targets,excluded_sources,resets_processing_context:true,scope:"Selected claims, their cited input messages and recorded dependents. Background Notes/history are reset; source conversations stop contributing memory. Other independent records, Bot chat/files/logs/backups and already-delivered context are not erased.".into()};
        let now = anda_engine::unix_ms();
        let requests = self.change_requests(&key, &input, &preview, now).await?;
        for request in &requests {
            request.parse_operations()?;
        }
        // Validate the generated correction against the live schema before
        // admitting a durable change. Do not consume its idempotency key.
        if input.kind == ChangeKind::Correct {
            let mut dry = requests[0].clone();
            dry.options.get_or_insert_default().dry_run = Some(true);
            dry.operations[0].idempotency_key = None;
            let response = anda_kip::execute_request(self.memory.nexus().as_ref(), &dry).await;
            if !kip::succeeded(&response) {
                return Err("unsupported_correction".into());
            }
        }
        let receipt = ChangeReceipt {
            schema_version: 1,
            operation_id: input.operation_id.clone(),
            operation_key: key.clone(),
            caller: caller.to_string(),
            state: "prepared".into(),
            preview_digest: anda_cognitive_nexus::content_digest(&serde_json::to_value(&preview)?)?,
            expires_at: now.saturating_add(600_000),
            preview,
            replacement_record: None,
            source_evidence: None,
            error: None,
        };
        self.product_control
            .journal
            .create(
                &path,
                &StoredChange {
                    input,
                    input_digest: digest,
                    receipt: receipt.clone(),
                    requests,
                    completed_targets: BTreeSet::new(),
                },
            )
            .await?;
        Ok(receipt)
    }

    pub async fn product_discard(
        self: &Arc<Self>,
        caller: Principal,
        id: String,
    ) -> Result<(), BoxError> {
        let key = operation_key(caller, &id)?;
        let this = self.clone();
        self.product_control
            .tasks
            .run(async move {
                let _guard = this.product_control.gate.lock().await;
                if this.product_control.snapshot().pending.as_deref() == Some(&key) {
                    return Err("memory_change_pending".into());
                }
                let path = format!("changes/{key}");
                let stored = this
                    .product_control
                    .journal
                    .read::<StoredChange>(&path)
                    .await?;
                if stored.as_ref().is_some_and(|stored| {
                    !matches!(
                        stored.value.receipt.state.as_str(),
                        "prepared" | "discarded"
                    )
                }) {
                    return Err("memory_change_pending".into());
                }
                if this
                    .product_control
                    .journal
                    .read::<bool>(&format!("discarded/{key}"))
                    .await?
                    .is_none()
                {
                    this.product_control
                        .journal
                        .create(&format!("discarded/{key}"), &true)
                        .await?;
                }
                if let Some(mut stored) = stored {
                    stored.value.receipt.state = "discarded".into();
                    stored.value.clear_content();
                    this.product_control
                        .journal
                        .put(
                            &path,
                            &stored.value,
                            object_store::PutMode::Update(stored.version),
                        )
                        .await?;
                }
                Ok(())
            })
            .await
    }

    pub async fn product_change(
        &self,
        caller: Principal,
        id: &str,
    ) -> Result<ChangeReceipt, BoxError> {
        let key = operation_key(caller, id)?;
        let stored = self
            .product_control
            .journal
            .read::<StoredChange>(&format!("changes/{key}"))
            .await?
            .ok_or("not_found")?;
        if stored.value.receipt.caller != caller.to_string() {
            return Err("not_found".into());
        }
        Ok(stored.value.receipt)
    }

    pub async fn product_commit(
        self: &Arc<Self>,
        caller: Principal,
        id: String,
        preview_digest: String,
    ) -> Result<ChangeReceipt, BoxError> {
        let key = operation_key(caller, &id)?;
        let this = self.clone();
        self.product_control
            .tasks
            .run(async move {
                let _guard = this.product_control.gate.lock().await;
                let mut stored = this
                    .product_control
                    .journal
                    .read::<StoredChange>(&format!("changes/{key}"))
                    .await?
                    .ok_or("not_found")?;
                if stored.value.receipt.caller != caller.to_string() {
                    return Err("not_found".into());
                }
                if stored.value.receipt.preview_digest != preview_digest {
                    return Err("revision_conflict".into());
                }
                if !matches!(
                    stored.value.receipt.state.as_str(),
                    "prepared" | "committing" | "reconciling" | "confirmed"
                ) {
                    return Err("preview_expired".into());
                }
                if stored.value.receipt.state == "confirmed"
                    && this.product_control.snapshot().pending.is_none()
                {
                    return Ok(stored.value.receipt);
                }
                if this.product_control.snapshot().pending.as_deref() == Some(&key) {
                    return this.apply_change_locked(&key).await;
                }
                if stored.value.receipt.state == "prepared" {
                    if anda_engine::unix_ms() > stored.value.receipt.expires_at {
                        return Err("preview_expired".into());
                    }
                    let current = this.product_record(&stored.value.input.record_id).await?;
                    if current.revision != stored.value.input.expected_revision
                        || serde_json::to_value(&current)?
                            != serde_json::to_value(&stored.value.receipt.preview.record)?
                    {
                        return Err("revision_conflict".into());
                    }
                    let mut keys = this.source_keys_for_record(&current).await?;
                    let targets = if stored.value.input.kind != ChangeKind::Correct {
                        this.deletion_targets(&current, &mut keys).await?
                    } else {
                        vec![Target {
                            id: current.id.clone(),
                            revision: current.revision,
                            kind: "assertion".into(),
                        }]
                    };
                    if serde_json::to_value(&targets)?
                        != serde_json::to_value(&stored.value.receipt.preview.targets)?
                        || keys != stored.value.receipt.preview.excluded_sources
                    {
                        return Err("revision_conflict".into());
                    }
                    if !this.product_control.available() {
                        return Err("memory_change_pending".into());
                    }
                    let mut control = this.product_control.snapshot();
                    control.epoch = control
                        .epoch
                        .checked_add(1)
                        .ok_or("memory epoch exhausted")?;
                    control.pending = Some(key.clone());
                    control
                        .suppressed
                        .extend(stored.value.receipt.preview.excluded_sources.clone());
                    if control.suppressed.len() > 100_000 {
                        return Err("source suppression capacity exhausted".into());
                    }
                    this.product_control.save(control).await?;
                    stored.value.receipt.state = "committing".into();
                    this.product_control
                        .journal
                        .put(
                            &format!("changes/{key}"),
                            &stored.value,
                            object_store::PutMode::Update(stored.version),
                        )
                        .await?;
                }
                this.apply_change_locked(&key).await
            })
            .await
    }

    pub(crate) async fn recover_product_change(self: &Arc<Self>) -> Result<(), BoxError> {
        if let Some(key) = self.product_control.snapshot().pending {
            let this = self.clone();
            self.product_control
                .tasks
                .run(async move {
                    let _guard = this.product_control.gate.lock().await;
                    this.apply_change_locked(&key).await.map(|_| ())
                })
                .await?;
        }
        Ok(())
    }

    async fn apply_change_locked(&self, key: &str) -> Result<ChangeReceipt, BoxError> {
        let path = format!("changes/{key}");
        let mut stored = self
            .product_control
            .journal
            .read::<StoredChange>(&path)
            .await?
            .ok_or("memory change missing")?;
        let mut control = self.product_control.snapshot();
        if control
            .pending
            .as_deref()
            .is_some_and(|pending| pending != key)
        {
            return Err("memory_change_pending".into());
        }
        if control.pending.is_none() {
            control.epoch = control
                .epoch
                .checked_add(1)
                .ok_or("memory epoch exhausted")?;
            control.pending = Some(key.into());
            control
                .suppressed
                .extend(stored.value.receipt.preview.excluded_sources.clone());
            if control.suppressed.len() > 100_000 {
                return Err("source suppression capacity exhausted".into());
            }
            self.product_control.save(control).await?;
        }
        for (index, request) in stored.value.requests.clone().into_iter().enumerate() {
            let target = if stored.value.input.kind != ChangeKind::Correct {
                stored.value.receipt.preview.targets[index].id.clone()
            } else {
                "correction".into()
            };
            if stored.value.completed_targets.contains(&target) {
                continue;
            }
            if stored.value.input.kind != ChangeKind::Correct {
                let element = self
                    .memory
                    .nexus()
                    .store
                    .get_element(target.parse()?)
                    .await?;
                if element_state(&element) == "purged"
                    || (stored.value.input.kind == ChangeKind::Suppress
                        && element_state(&element) == "archived")
                {
                    stored.value.completed_targets.insert(target.clone());
                }
            }
            if !stored.value.completed_targets.contains(&target) {
                // DurableTasks owns this future. Never cancel a native commit
                // because a client disappeared or a UI request timed out.
                let response =
                    anda_kip::execute_request(self.memory.nexus().as_ref(), &request).await;
                if response.status != TopLevelStatus::Succeeded {
                    stored.value.receipt.state = "reconciling".into();
                    stored.value.receipt.error = Some(kip::error_message(&response));
                    self.product_control
                        .journal
                        .put(
                            &path,
                            &stored.value,
                            object_store::PutMode::Update(stored.version),
                        )
                        .await?;
                    return self
                        .product_control
                        .journal
                        .read::<StoredChange>(&path)
                        .await?
                        .map(|v| v.value.receipt)
                        .ok_or_else(|| "memory change missing".into());
                }
                if stored.value.input.kind == ChangeKind::Correct {
                    stored.value.receipt.replacement_record = kip::ok_result(&response)
                        .and_then(|r| r["handles"]["new"].as_str())
                        .map(str::to_string);
                    stored.value.receipt.source_evidence = kip::ok_result(&response)
                        .and_then(|r| r["handles"]["input"].as_str())
                        .map(str::to_string);
                }
                stored.value.completed_targets.insert(target);
            }
            self.product_control
                .journal
                .put(
                    &path,
                    &stored.value,
                    object_store::PutMode::Update(stored.version),
                )
                .await?;
            stored = self
                .product_control
                .journal
                .read(&path)
                .await?
                .ok_or("memory change missing")?;
        }
        self.reset_product_processing_notes().await?;
        if stored.value.input.kind != ChangeKind::Correct {
            for target in &stored.value.receipt.preview.targets {
                let state = if stored.value.input.kind == ChangeKind::Delete {
                    "purged"
                } else {
                    "archived"
                };
                if element_state(
                    &self
                        .memory
                        .nexus()
                        .store
                        .get_element(target.id.parse()?)
                        .await?,
                ) != state
                {
                    return Err("memory change verification incomplete".into());
                }
            }
        } else {
            let old = self.product_record(&stored.value.input.record_id).await?;
            if old.status != "retracted" || stored.value.receipt.replacement_record.is_none() {
                return Err("correction verification incomplete".into());
            }
        }
        stored.value.receipt.state = "confirmed".into();
        stored.value.receipt.error = None;
        if stored.value.input.kind == ChangeKind::Delete {
            self.clear_related_change_content(key, &stored.value.receipt.preview.targets)
                .await?;
            stored.value.clear_content();
        }
        self.product_control
            .journal
            .put(
                &path,
                &stored.value,
                object_store::PutMode::Update(stored.version),
            )
            .await?;
        let mut control = self.product_control.snapshot();
        control.read_floor = self.memory.nexus().store.current_seq(DEFAULT_SPACE).await?;
        control.pending = None;
        self.product_control.save(control).await?;
        Ok(stored.value.receipt)
    }

    /// Reuse the pending deletion's recovery boundary: if cleanup is interrupted,
    /// the next commit/reload repeats this scan before releasing the source fence.
    async fn clear_related_change_content(
        &self,
        key: &str,
        targets: &[Target],
    ) -> Result<(), BoxError> {
        let erased: BTreeSet<String> = targets.iter().map(|target| target.id.clone()).collect();
        self.clear_product_preview_content(&erased, Some(key)).await
    }

    pub(crate) async fn clear_product_preview_content(
        &self,
        erased: &BTreeSet<String>,
        except: Option<&str>,
    ) -> Result<(), BoxError> {
        let journal = &self.product_control.journal;
        let mut paths = journal.keys("changes/");
        while let Some(path) = paths.try_next().await? {
            if except.is_some_and(|key| path == format!("changes/{key}")) {
                continue;
            }
            let Some(mut stored) = journal.read::<StoredChange>(&path).await? else {
                continue;
            };
            let receipt = &stored.value.receipt;
            let record = &receipt.preview.record;
            let record_erased = erased.contains(record.id.as_str())
                || erased.contains(record.proposition_id.as_str())
                || record
                    .sources
                    .iter()
                    .any(|source| erased.contains(source.evidence_id.as_str()));
            let correction_erased = receipt
                .replacement_record
                .as_deref()
                .is_some_and(|id| erased.contains(id))
                || receipt
                    .source_evidence
                    .as_deref()
                    .is_some_and(|id| erased.contains(id));
            if !record_erased && !correction_erased {
                continue;
            }
            if stored.value.receipt.state == "prepared" {
                let discarded = format!("discarded/{}", stored.value.receipt.operation_key);
                if journal.read::<bool>(&discarded).await?.is_none() {
                    journal.create(&discarded, &true).await?;
                }
                stored.value.receipt.state = "discarded".into();
            }
            if correction_erased || stored.value.receipt.state != "confirmed" {
                stored.value.clear_content();
            } else {
                // Deleting an older claim does not erase a surviving correction's
                // independent user statement; only its copy of the old record.
                stored.value.clear_record_content();
            }
            journal
                .put(
                    &path,
                    &stored.value,
                    object_store::PutMode::Update(stored.version),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn product_correction_source(
        &self,
        caller: Principal,
        source: &RecordSource,
    ) -> Result<Option<String>, BoxError> {
        let Some(key) = &source.product_operation else {
            return Ok(None);
        };
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let Element::Evidence(evidence) = self
            .memory
            .nexus()
            .store
            .get_element(source.evidence_id.parse()?)
            .await?
        else {
            return Ok(None);
        };
        if evidence.space != DEFAULT_SPACE
            || evidence.state == "purged"
            || super::source_from_evidence(&evidence)?.payload_digest != source.payload_digest
        {
            return Ok(None);
        }
        let Some(change) = self
            .product_control
            .journal
            .read::<StoredChange>(&format!("changes/{key}"))
            .await?
        else {
            return Ok(None);
        };
        let receipt = &change.value.receipt;
        if receipt.caller != caller.to_string()
            || receipt.state != "confirmed"
            || receipt.source_evidence.as_deref() != Some(&source.evidence_id)
        {
            return Ok(None);
        }
        let statement = change
            .value
            .requests
            .first()
            .and_then(|request| request.parameters.as_ref())
            .and_then(|parameters| parameters.get("statement"));
        if statement
            .map(anda_cognitive_nexus::content_digest)
            .transpose()?
            .as_ref()
            != source.payload_digest.as_ref()
        {
            return Ok(None);
        }
        Ok(receipt.preview.new_value.clone())
    }

    async fn reset_product_processing_notes(&self) -> Result<(), BoxError> {
        for agent in [
            FormationAgent::NAME,
            MaintenanceAgent::NAME,
            RecallAgent::NAME,
        ] {
            let ctx = self
                .engine
                .ctx_with(SELF_USER_ID, agent, "", RequestMeta::default())?;
            NoteTool::new()
                .call(
                    ctx.child_base(NoteTool::NAME)?,
                    NoteArgs {
                        op: Some("set".into()),
                        items: Some(vec![]),
                    },
                    vec![],
                )
                .await?;
        }
        self.miss_cache.clear().await?;
        Ok(())
    }

    async fn source_keys_for_record(
        &self,
        record: &MemoryRecord,
    ) -> Result<BTreeSet<String>, BoxError> {
        let mut keys = BTreeSet::new();
        for source in &record.sources {
            self.add_source_keys(source, &mut keys).await?;
        }
        if keys.is_empty() {
            return Err("unsupported_scope".into());
        }
        Ok(keys)
    }

    pub(super) async fn add_source_keys(
        &self,
        source: &RecordSource,
        keys: &mut BTreeSet<String>,
    ) -> Result<(), BoxError> {
        if let Some(id) = source.formation_conversation {
            let conversation = self
                .memory
                .get_conversation(id)
                .await
                .map_err(|_| "unsupported_scope")?;
            let prompt = conversation
                .messages
                .first()
                .and_then(|message| {
                    serde_json::from_value::<anda_core::Message>(message.clone()).ok()
                })
                .and_then(|message| message.text())
                .ok_or("unsupported_scope")?;
            let input = serde_json::from_str::<crate::types::FormationInput>(&prompt)
                .unwrap_or_else(|_| crate::types::FormationInput {
                    messages: vec![anda_core::Message {
                        role: "user".into(),
                        content: vec![prompt.clone().into()],
                        ..Default::default()
                    }],
                    context: None,
                    timestamp: None,
                });
            let message = input
                .messages
                .get(source.message_index.ok_or("unsupported_scope")?)
                .ok_or("unsupported_scope")?;
            let digest = anda_cognitive_nexus::content_digest(&serde_json::to_value(message)?)?;
            let observed_at = crate::kip::observation_timestamp(
                input.timestamp.as_deref(),
                conversation.created_at,
            );
            if source.payload_digest.as_deref() != Some(digest.as_str())
                || source.observed_at.as_deref() != Some(observed_at.as_str())
            {
                return Err("unsupported_scope".into());
            }
            keys.extend(SourceIdentity::for_conversation(&conversation)?.keys());
            keys.insert(format!("formation:{id}"));
            return Ok(());
        }
        if let Some(operation) = &source.product_operation {
            let change = self
                .product_control
                .journal
                .read::<StoredChange>(&format!("changes/{operation}"))
                .await?
                .ok_or("unsupported_scope")?;
            let receipt = &change.value.receipt;
            let statement = change
                .value
                .requests
                .first()
                .and_then(|request| request.parameters.as_ref())
                .and_then(|parameters| parameters.get("statement"));
            if receipt.state != "confirmed"
                || receipt.source_evidence.as_deref() != Some(&source.evidence_id)
                || statement
                    .map(anda_cognitive_nexus::content_digest)
                    .transpose()?
                    .as_ref()
                    != source.payload_digest.as_ref()
            {
                return Err("unsupported_scope".into());
            }
            keys.insert(format!("product-change:{}", receipt.operation_key));
            return Ok(());
        }
        Err("unsupported_scope".into())
    }

    async fn deletion_targets(
        &self,
        record: &MemoryRecord,
        sources: &mut BTreeSet<String>,
    ) -> Result<Vec<Target>, BoxError> {
        let nexus = self.memory.nexus();
        let mut pending = vec![record.proposition_id.parse::<ElementId>()?];
        let mut seen = BTreeSet::new();
        let mut targets = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id.to_string()) {
                continue;
            }
            if seen.len() > 128 {
                return Err("unsupported_scope".into());
            }
            let element = nexus.store.get_element(id).await?;
            if element_state(&element) == "purged" {
                continue;
            }
            let (revision, kind, retention) = match &element {
                Element::Concept(_) => return Err("unsupported_scope".into()),
                Element::Proposition(r) => (r.version, "proposition", &r.retention),
                Element::Assertion(r) => {
                    pending.push(r.proposition_id.parse()?);
                    for evidence in &r.evidence_ids {
                        pending.push(evidence.parse()?)
                    }
                    (r.version, "assertion", &r.retention)
                }
                Element::Evidence(r) => {
                    let source = super::source_from_evidence(r)?;
                    self.add_source_keys(&source, sources).await?;
                    (r.version, "evidence", &r.retention)
                }
                Element::Activity(r) => (r.version, "activity", &r.retention),
            };
            if retention.get("legal_hold") == Some(&Value::Bool(true)) {
                return Err("unsupported_scope".into());
            }
            targets.insert(
                id.to_string(),
                Target {
                    id: id.to_string(),
                    revision,
                    kind: kind.into(),
                },
            );
            pending.extend(nexus.store.referrers(DEFAULT_SPACE, id).await?);
        }
        Ok(targets.into_values().collect())
    }

    async fn change_requests(
        &self,
        key: &str,
        input: &ChangeInput,
        preview: &ChangePreview,
        now: u64,
    ) -> Result<Vec<Request>, BoxError> {
        if input.kind != ChangeKind::Correct {
            return preview.targets.iter().map(|target|{
                let command=if input.kind==ChangeKind::Delete {"PURGE :id EXPECT VERSION :version REFERENCE POLICY \"authorized_cascade\" CONFIRM \"PURGE\""} else {"TRANSITION :id TO \"archived\" EXPECT VERSION :version"};
                let mut request=kip::request_with(command,serde_json::Map::from_iter([("id".into(),json!(target.id)),("version".into(),json!(target.revision))]));
                request.operations[0].idempotency_key=Some(format!("memory-product:{key}:{}",target.id));Ok(request)
            }).collect();
        }
        let object_id = preview
            .record
            .object
            .get("id")
            .and_then(Value::as_str)
            .ok_or("unsupported_scope")?;
        let Element::Concept(object) = self
            .memory
            .nexus()
            .store
            .get_element(object_id.parse()?)
            .await?
        else {
            return Err("unsupported_scope".into());
        };
        let mut request = kip::request_with(
            r#"MUTATE {
            TRANSITION :old TO "retracted" EXPECT VERSION :version
            CREATE EVIDENCE ?input { CLIENT KEY :source_key SET FIELDS { evidence_class:"user_statement", payload: :statement, observed_at: :at } }
            CREATE CONCEPT ?value { TYPE :object_type NAME :new_value }
            ASSERT ?new (:subject, :predicate, ?value) {by: :actor, mode:"stated", evidence:?input, at: :at, valid: {from: :at}}
            CREATE ACTIVITY ?change { SET FIELDS {activity_class:"user_memory_correction",status:"completed",started_at: :at,ended_at: :at} SET STRUCTURAL {("inputs", :old) ("inputs", ?input) ("outputs", ?new)} }
        }"#,
            serde_json::Map::from_iter([
                ("old".into(), json!(input.record_id)),
                ("version".into(), json!(input.expected_revision)),
                (
                    "source_key".into(),
                    json!(format!("memory-product:{key}:input")),
                ),
                (
                    "statement".into(),
                    json!({"kind":"user_correction","new_value":input.new_value,"previous_record":input.record_id}),
                ),
                ("at".into(), json!(kip::timestamp(now))),
                ("object_type".into(), json!(object.schema_ref)),
                ("new_value".into(), json!(input.new_value)),
                ("subject".into(), preview.record.subject.clone()),
                ("predicate".into(), json!(preview.record.predicate)),
                ("actor".into(), json!({"id":preview.record.actor_id})),
            ]),
        );
        request.operations[0].idempotency_key = Some(format!("memory-product:{key}:correct"));
        Ok(vec![request])
    }
}

fn element_state(element: &Element) -> &str {
    match element {
        Element::Concept(r) => &r.state,
        Element::Proposition(r) => &r.state,
        Element::Assertion(r) => &r.state,
        Element::Evidence(r) => &r.state,
        Element::Activity(r) => &r.state,
    }
}

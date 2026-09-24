//! Off-graph delivery receipts. A receipt proves what the host delivered;
//! it proves neither actual use, truth, causal contribution nor permission.
use anda_cognitive_nexus::{attention::RuntimeScope, content_digest};
use anda_core::{BoxError, Json, Message};
use anda_db::database::AndaDB;
use object_store::PutMode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

pub const FORMAT: &str = "anda-brain:recall-receipt-v1";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecallReceiptRef {
    pub id: String,
    pub digest: String,
    pub scope: RuntimeScope,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryPin {
    pub id: String,
    pub version: u64,
    /// Excludes volatile system projections and accessibility metadata.
    pub content_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallReceipt {
    pub format: String,
    pub scope: RuntimeScope,
    pub delivery_key: String,
    pub packet_digest: String,
    pub delivery: String,
    pub pins: Vec<MemoryPin>,
    pub item_digests: BTreeMap<String, String>,
    pub basis: Vec<Json>,
    pub coverage: Json,
    pub budget: Option<crate::recall_budget::RecallBudget>,
    pub complete_inventory: bool,
    pub semantic_complete: bool,
    pub action_ready: bool,
}
pub struct RecallReceipts {
    directory: Arc<crate::attention::Directory>,
    pub(crate) scope: RuntimeScope,
    tasks: crate::runtime::DurableTasks,
}
pub(crate) fn semantic_digest(row: &Json) -> Result<String, BoxError> {
    let mut row = row.clone();
    if let Some(map) = row.as_object_mut() {
        map.remove("_system");
        if let Some(facets) = map.get_mut("facets").and_then(Json::as_object_mut) {
            facets.remove(profile!("MnemonicState"));
        }
        if map
            .get("facets")
            .is_some_and(|v| v.as_object().is_some_and(|v| v.is_empty()))
        {
            map.remove("facets");
        }
    }
    Ok(content_digest(&row)?)
}
pub(crate) fn pin(row: &Json) -> Result<MemoryPin, BoxError> {
    let id = row["id"].as_str().ok_or("receipt element has no id")?;
    let parsed: anda_cognitive_nexus::ElementId = id.parse()?;
    if parsed.to_string() != id {
        return Err("receipt requires canonical element ids".into());
    }
    Ok(MemoryPin {
        id: id.into(),
        version: row["_system"]["version"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("receipt requires an actual element version")?,
        content_digest: semantic_digest(row)?,
    })
}
impl RecallReceipts {
    pub(crate) async fn connect(
        id: &str,
        db: &AndaDB,
        directory: Arc<crate::attention::Directory>,
    ) -> Result<Arc<Self>, BoxError> {
        let instance =
            if let Some(instance) = db.get_extension_as::<String>("recall_receipt_instance") {
                instance
            } else {
                let instance = content_digest(&json!(rand::random::<[u8; 32]>()))?;
                db.save_extension_from("recall_receipt_instance".into(), &instance)
                    .await?;
                instance
            };
        Ok(Arc::new(Self {
            directory,
            scope: RuntimeScope {
                space_id: id.into(),
                space_instance: instance,
            },
            tasks: Default::default(),
        }))
    }
    fn key(&self, id: &str) -> Result<String, BoxError> {
        crate::runtime_api::key(&self.scope, "recall-receipts", id)
    }
    pub async fn read(&self, reference: &RecallReceiptRef) -> Result<RecallReceipt, BoxError> {
        if reference.scope != self.scope || !crate::runtime_api::digest_valid(&reference.digest) {
            return Err("receipt belongs to another memory instance".into());
        }
        let receipt = self
            .directory
            .read::<RecallReceipt>(&self.key(&reference.id)?)
            .await?
            .ok_or("recall receipt unavailable")?
            .value;
        if receipt.format != FORMAT
            || receipt.scope != self.scope
            || content_digest(&json!(receipt))? != reference.digest
        {
            return Err("recall receipt digest/identity mismatch".into());
        }
        Ok(receipt)
    }
    pub async fn for_conversation(&self, id: u64) -> Result<Option<RecallReceiptRef>, BoxError> {
        Ok(self
            .directory
            .read::<RecallReceiptRef>(&crate::runtime_api::key(
                &self.scope,
                "recall-conversations",
                &id.to_string(),
            )?)
            .await?
            .map(|r| r.value))
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.tasks.is_busy()
    }
    pub(crate) async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    pub(crate) async fn issue_packet(
        self: &Arc<Self>,
        delivery_key: String,
        content: String,
        budget: crate::recall_budget::RecallBudget,
    ) -> Result<RecallReceiptRef, BoxError> {
        let packet = serde_json::from_str::<crate::recall_budget::MemoryPacket>(&content).ok();
        let mut receipt = RecallReceipt {
            format: FORMAT.into(),
            scope: self.scope.clone(),
            delivery_key,
            packet_digest: content_digest(&json!(content))?,
            delivery: "bounded_packet".into(),
            pins: vec![],
            item_digests: BTreeMap::new(),
            basis: vec![],
            coverage: json!({"unchecked":["all"]}),
            budget: Some(budget),
            complete_inventory: true,
            semantic_complete: false,
            action_ready: false,
        };
        if let Some(packet) = packet {
            receipt.coverage = json!(packet.coverage);
            if packet.status != "bounded" {
                receipt.delivery = "insufficient".into();
            }
            for item in &packet.items {
                receipt
                    .item_digests
                    .insert(item.id.clone(), content_digest(&item.content)?);
                // Only known host containers are walked. Evidence payload and
                // arbitrary attributes never manufacture additional pins.
                collect_material(&item.content, &mut receipt);
            }
        } else {
            receipt.delivery = "insufficient".into();
        }
        self.persist(receipt, None).await
    }
    pub(crate) async fn issue_legacy(
        self: &Arc<Self>,
        conversation: u64,
        content: String,
        messages: &[Json],
    ) -> Result<RecallReceiptRef, BoxError> {
        let messages: Vec<Message> = messages
            .iter()
            .filter_map(|m| serde_json::from_value(m.clone()).ok())
            .collect();
        let trace = crate::assess::RecallTrace::from_messages(&messages);
        let mut receipt = RecallReceipt {
            format: FORMAT.into(),
            scope: self.scope.clone(),
            delivery_key: format!("conversation:{conversation}"),
            packet_digest: content_digest(&json!(content))?,
            delivery: "legacy_trace_only".into(),
            pins: vec![],
            item_digests: BTreeMap::new(),
            basis: vec![],
            coverage: json!({"unchecked":["semantic_delivery"]}),
            budget: None,
            complete_inventory: false,
            semantic_complete: false,
            action_ready: false,
        };
        for tool in trace.tools {
            if tool.name == anda_engine::memory::MemoryReadonly::NAME
                && tool.is_error != Some(true)
                && let Some(output) = tool.output
            {
                collect_material(&output, &mut receipt);
            }
        }
        self.persist(receipt, Some(conversation)).await
    }
    pub(crate) async fn bind_conversation(
        self: &Arc<Self>,
        id: u64,
        reference: RecallReceiptRef,
    ) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                this.read(&reference).await?;
                this.directory
                    .put(
                        &crate::runtime_api::key(
                            &this.scope,
                            "recall-conversations",
                            &id.to_string(),
                        )?,
                        &reference,
                        PutMode::Create,
                    )
                    .await
            })
            .await
    }
    async fn persist(
        self: &Arc<Self>,
        mut receipt: RecallReceipt,
        conversation: Option<u64>,
    ) -> Result<RecallReceiptRef, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                receipt
                    .pins
                    .sort_by(|a, b| a.id.cmp(&b.id).then(a.version.cmp(&b.version)));
                receipt.pins.dedup();
                if receipt.pins.len() > 256
                    || receipt.basis.len() > 64
                    || receipt.item_digests.len() > 256
                {
                    receipt.complete_inventory = false;
                    receipt.pins.truncate(256);
                    receipt.basis.truncate(64);
                    receipt.item_digests.clear();
                }
                if serde_json::to_vec(&receipt)?.len() > 120_000 {
                    receipt.basis.clear();
                    receipt.complete_inventory = false;
                }
                let digest = content_digest(&json!(receipt))?;
                let reference = RecallReceiptRef {
                    id: content_digest(&json!([receipt.scope, receipt.delivery_key, digest]))?[7..]
                        .into(),
                    digest,
                    scope: this.scope.clone(),
                };
                this.directory
                    .put(&this.key(&reference.id)?, &receipt, PutMode::Create)
                    .await?;
                if let Some(id) = conversation {
                    this.directory
                        .put(
                            &crate::runtime_api::key(
                                &this.scope,
                                "recall-conversations",
                                &id.to_string(),
                            )?,
                            &reference,
                            PutMode::Create,
                        )
                        .await?;
                }
                Ok(reference)
            })
            .await
    }
}
fn collect_material(value: &Json, receipt: &mut RecallReceipt) {
    crate::assess::collect_entity_objects(value, &mut |_, map| {
        if let Ok(p) = pin(&Json::Object(map.clone())) {
            if receipt.pins.len() < 256 {
                receipt.pins.push(p);
            } else {
                receipt.complete_inventory = false;
            }
        } else {
            receipt.complete_inventory = false;
        }
    });
    // Action packets wrap a fully read row and a native BELIEF projection.
    if let Some(row) = value.get("element")
        && let Ok(p) = pin(row)
    {
        if receipt.pins.len() < 256 {
            receipt.pins.push(p);
        } else {
            receipt.complete_inventory = false;
        }
    }
    if let Some(basis) = value.get("belief").and_then(|v| v.get("basis")) {
        receipt.basis.push(basis.clone());
    }
    if value.get("kip").and_then(Json::as_str) == Some("2.0")
        && let Some(results) = value["results"].as_array()
    {
        for result in results {
            if result["status"] == "succeeded" {
                receipt.basis.push(result["context"].clone());
            }
        }
    }
}

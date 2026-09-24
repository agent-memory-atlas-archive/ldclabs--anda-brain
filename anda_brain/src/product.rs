//! Typed product projections for trusted Rust hosts. These methods do not
//! authenticate an end user: the embedding application must enforce its owner
//! and source visibility policy before returning any data to a client.
use crate::space::Space;
use anda_cognitive_nexus::{
    ElementId,
    nexus::DEFAULT_SPACE,
    store::{Element, rows::AssertionRow},
};
use anda_core::BoxError;
use anda_db::{
    query::{Filter, RangeQuery},
    schema::Fv,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub(crate) mod control;
pub use control::{SourceAdmissionError, SourceIdentity};
mod changes;
pub use changes::{ChangeInput, ChangeKind, ChangePreview, ChangeReceipt, Target};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordSource {
    pub evidence_id: String,
    pub payload_digest: Option<String>,
    pub formation_conversation: Option<u64>,
    pub product_operation: Option<String>,
    /// Index in the actual submitted Formation input, not the Bot transcript.
    pub message_index: Option<usize>,
    pub observed_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryRecord {
    pub id: String,
    pub revision: u64,
    pub proposition_id: String,
    pub actor_id: Option<String>,
    pub actor_key: Option<String>,
    pub subject: Value,
    pub predicate: String,
    pub object: Value,
    pub text: String,
    pub subject_label: String,
    pub object_label: String,
    pub stance: String,
    pub status: String,
    pub storage_state: String,
    pub asserted_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    /// The context set the claim was made in (`context_refs`), as Concept ids.
    /// A revision keeps it: supersession and temporal succession both compare
    /// the canonical context set (Spec §14.2, §25.4).
    #[serde(default)]
    pub context_refs: Vec<String>,
    pub updated_at: String,
    pub sources: Vec<RecordSource>,
    pub sources_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordPage {
    pub records: Vec<MemoryRecord>,
    pub next_cursor: Option<u64>,
    pub complete: bool,
}

impl Space {
    pub async fn product_source(&self, id: &str) -> Result<RecordSource, BoxError> {
        let Element::Evidence(row) = self.memory.nexus().store.get_element(id.parse()?).await?
        else {
            return Err("not_found".into());
        };
        if row.space != DEFAULT_SPACE || row.state == "purged" {
            return Err("not_found".into());
        }
        Ok(self.verified_record_source(&row).await?.0)
    }

    pub fn product_epoch(&self) -> u64 {
        self.product_control.epoch()
    }
    pub fn product_available(&self) -> bool {
        self.product_control.available()
    }
    pub fn product_source_allowed(&self, source: &SourceIdentity) -> bool {
        source.validate().is_ok() && self.product_control.source_allowed(source)
    }
    /// Assertion-backed claims, including their stance and lifecycle. Mere
    /// Proposition existence is never projected as a true personal memory.
    pub async fn product_records(
        &self,
        before: Option<u64>,
        limit: usize,
    ) -> Result<RecordPage, BoxError> {
        if !(1..=50).contains(&limit) {
            return Err("limit must be between 1 and 50".into());
        }
        let nexus = self.memory.nexus();
        let assertions = nexus.store.assertions();
        let before = before.unwrap_or_else(|| assertions.max_document_id().saturating_add(1));
        let mut ids = assertions
            .query_last_ids(
                Filter::And(vec![
                    Box::new(Filter::Field((
                        "space".into(),
                        RangeQuery::Eq(Fv::Text(DEFAULT_SPACE.into())),
                    ))),
                    Box::new(Filter::Field((
                        "_id".into(),
                        RangeQuery::Lt(Fv::U64(before)),
                    ))),
                ]),
                Some(limit),
            )
            .await?;
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let next_cursor = (ids.len() == limit).then(|| *ids.last().unwrap());
        let mut records = Vec::new();
        let mut complete = true;
        for id in ids {
            let row: AssertionRow = assertions.get_as(id).await?;
            if row.state == "purged" {
                continue;
            }
            match self.product_record(&format!("A-{id}")).await {
                Ok(record) => records.push(record),
                Err(_) => complete = false,
            }
        }
        Ok(RecordPage {
            records,
            next_cursor,
            complete: complete && next_cursor.is_none(),
        })
    }

    pub async fn product_record(&self, id: &str) -> Result<MemoryRecord, BoxError> {
        let id: ElementId = id.parse()?;
        let nexus = self.memory.nexus();
        let Element::Assertion(row) = nexus.store.get_element(id).await? else {
            return Err("Only Assertion-backed records are supported".into());
        };
        if row.space != DEFAULT_SPACE || row.state == "purged" {
            return Err("Record not found".into());
        }
        let Element::Proposition(proposition) =
            nexus.store.get_element(row.proposition_id.parse()?).await?
        else {
            return Err("Record has no Proposition".into());
        };
        if proposition.state == "purged" {
            return Err("Record Proposition was erased".into());
        }
        let actor_id = row
            .asserted_by
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let actor_key = if let Some(id) = &actor_id {
            match nexus.store.get_element(id.parse()?).await? {
                Element::Concept(actor) => Some(actor.key.clone()),
                _ => None,
            }
        } else {
            None
        };
        let mut sources = Vec::new();
        let mut sources_complete = !row.evidence_ids.is_empty();
        for reference in &row.evidence_ids {
            let Element::Evidence(evidence) = nexus.store.get_element(reference.parse()?).await?
            else {
                sources_complete = false;
                continue;
            };
            let (source, verified) = self.verified_record_source(&evidence).await?;
            if !verified {
                sources_complete = false;
            }
            sources.push(source);
        }
        let subject = endpoint_label(self, &proposition.subject).await;
        let object = endpoint_label(self, &proposition.object).await;
        let predicate = proposition
            .predicate_ref
            .rsplit('/')
            .next()
            .unwrap_or(&proposition.predicate_ref);
        Ok(MemoryRecord {
            id: id.to_string(),
            revision: row.version,
            proposition_id: row.proposition_id.clone(),
            actor_id,
            actor_key,
            subject: proposition.subject.clone(),
            predicate: proposition.predicate_ref.clone(),
            object: proposition.object.clone(),
            text: format!("{subject} · {predicate} · {object}"),
            subject_label: subject,
            object_label: object,
            stance: row.stance.clone(),
            status: row.status.clone(),
            storage_state: row.state.clone(),
            asserted_at: nonempty(&row.asserted_at),
            valid_from: nonempty(&row.valid_from),
            valid_until: nonempty(&row.valid_until),
            context_refs: row
                .context_refs
                .iter()
                .filter_map(|reference| {
                    reference
                        .as_str()
                        .or_else(|| reference.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                })
                .collect(),
            updated_at: row.updated_at.clone(),
            sources,
            sources_complete,
        })
    }

    async fn verified_record_source(
        &self,
        evidence: &anda_cognitive_nexus::store::rows::EvidenceRow,
    ) -> Result<(RecordSource, bool), BoxError> {
        let mut source = source_from_evidence(evidence)?;
        let verified = evidence.state != "purged"
            && evidence.payload_mode == "inline"
            && self
                .add_source_keys(&source, &mut std::collections::BTreeSet::new())
                .await
                .is_ok();
        if !verified {
            source.formation_conversation = None;
            source.message_index = None;
            source.product_operation = None;
        }
        Ok((source, verified))
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn formation_source(key: &str) -> Option<(u64, usize)> {
    let suffix = key.strip_prefix("formation:conversation:")?;
    let (conversation, index) = suffix.split_once(':')?;
    let conversation = conversation.parse::<u64>().ok().filter(|v| *v > 0)?;
    let index = index.parse::<usize>().ok()?.checked_sub(1)?;
    Some((conversation, index))
}

async fn endpoint_label(space: &Space, endpoint: &Value) -> String {
    if let Some(id) = endpoint
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<ElementId>().ok())
        && let Ok(Element::Concept(concept)) = space.memory.nexus().store.get_element(id).await
    {
        return concept.name.chars().take(512).collect();
    }
    endpoint.to_string().chars().take(512).collect()
}

pub(crate) fn source_from_evidence(
    evidence: &anda_cognitive_nexus::store::rows::EvidenceRow,
) -> Result<RecordSource, BoxError> {
    let binding = formation_source(&evidence.client_key);
    let (formation_conversation, message_index) =
        binding.map(|(c, i)| (Some(c), Some(i))).unwrap_or_default();
    let product_operation = evidence
        .client_key
        .strip_prefix("memory-product:")
        .and_then(|key| key.strip_suffix(":input"))
        .filter(|key| key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string);
    Ok(RecordSource {
        evidence_id: format!("E-{}", evidence._id),
        payload_digest: if evidence.state != "purged" && evidence.payload_mode == "inline" {
            Some(anda_cognitive_nexus::content_digest(
                &evidence.payload_inline,
            )?)
        } else {
            None
        },
        formation_conversation,
        product_operation,
        message_index,
        observed_at: nonempty(&evidence.observed_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_bindings_are_not_actor_names_or_unvalidated_indices() {
        assert_eq!(
            formation_source("formation:conversation:42:3"),
            Some((42, 2))
        );
        for invalid in [
            "actor:alice",
            "formation:conversation:42:0",
            "formation:conversation:42:3:other",
            "formation:conversation:0:1",
        ] {
            assert_eq!(formation_source(invalid), None);
        }
    }
}

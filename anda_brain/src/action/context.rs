use super::*;
use crate::{
    kip,
    recall_budget::{self, Channel, Coverage, MemoryItem, MemoryPacket, Priority},
};
use anda_cognitive_nexus::nexus::Session;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Capture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<crate::recall_receipt::RecallReceiptRef>,
    pub request: ContextRequest,
    pub basis: Json,
    pub pins: Vec<Json>,
    pub retrieved: Vec<String>,
    pub packet: MemoryPacket,
    pub insufficient: Option<String>,
}

pub(super) async fn query(
    session: &Session,
    command: String,
    params: Json,
    timeout_ms: u64,
) -> Result<Json, BoxError> {
    let req = kip::request_with(
        command,
        params
            .as_object()
            .ok_or("query parameters missing")?
            .clone(),
    );
    let response = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        kip::execute_readonly_request(session, &req),
    )
    .await?;
    let result = kip::ok_result(&response)
        .cloned()
        .ok_or_else(|| kip::error_message(&response))?;
    if response
        .results
        .first()
        .is_some_and(|r| r.next_cursor.is_some())
    {
        return Err("required action read has incomplete page coverage".into());
    }
    if serde_json::to_vec(&result)?.len() > 262_144 {
        return Err("action read exceeds byte budget".into());
    }
    Ok(result)
}

pub(super) async fn element(
    session: &Session,
    id: &str,
    seq: Option<u64>,
    ms: u64,
) -> Result<Json, BoxError> {
    let parsed: anda_cognitive_nexus::ElementId = id.parse()?;
    if parsed.to_string() != id {
        return Err("noncanonical action reference".into());
    }
    let kind = match id.as_bytes().first() {
        Some(b'C') => "CONCEPT",
        Some(b'P') => "PROPOSITION",
        Some(b'A') => "ASSERTION",
        Some(b'E') => "EVIDENCE",
        Some(b'X') => "ACTIVITY",
        _ => return Err("invalid action reference".into()),
    };
    let suffix = seq.map(|s| format!(" AS OF SEQ {s}")).unwrap_or_default();
    let pattern = if kind == "PROPOSITION" {
        format!("{kind}(id: :id)")
    } else {
        format!("{kind} {{id: :id}}")
    };
    let rows = query(
        session,
        format!("FIND(?e) WHERE {{?e {pattern}}}{suffix} LIMIT 1"),
        json!({"id":id}),
        ms,
    )
    .await?;
    let row = rows
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .ok_or("action reference unavailable")?;
    if row["id"] != id || row["_system"]["version"].as_u64().is_none() {
        return Err("action reference is not fully readable".into());
    }
    Ok(row)
}

async fn belief(session: &Session, id: &str, seq: Option<u64>, ms: u64) -> Result<Json, BoxError> {
    if !id.starts_with("P-") {
        return Err("BELIEF requires a Proposition".into());
    }
    let suffix = seq.map(|s| format!(" AS OF SEQ {s}")).unwrap_or_default();
    let rows = query(
        session,
        format!("FIND(?b) WHERE {{?p PROPOSITION(id: :id) ?b BELIEF(?p)}}{suffix} LIMIT 1"),
        json!({"id":id}),
        ms,
    )
    .await?;
    rows.as_array()
        .and_then(|a| a.first())
        .cloned()
        .ok_or_else(|| "native belief coordinate unavailable".into())
}

impl Capture {
    pub async fn read(
        session: &Session,
        wake: &WakeRecord,
        request: ContextRequest,
        limits: &ActionLimits,
    ) -> Result<Self, BoxError> {
        if request.required_refs.len() > 32
            || request.premises.len() > 16
            || request.applied_revisions.len() > 8
            || request.task_family.is_empty()
            || request.task_family.len() > 256
            || !digest_valid(&request.environment_digest)
            || request.tool_versions.len() > 16
            || request
                .tool_versions
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > 128 || v.is_empty() || v.len() > 256)
            || request
                .deduplication_key
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256)
        {
            return Err("invalid bounded gate context".into());
        }
        let ms = limits.callbacks_ms;
        let anchor = belief(session, &request.anchor, None, ms).await?;
        let basis = anchor["basis"].clone();
        let seq = basis["snapshot_seq"]
            .as_u64()
            .ok_or("native basis sequence missing")?;
        let mut refs: BTreeSet<String> = request
            .required_refs
            .iter()
            .chain(&request.premises)
            .chain(&request.applied_revisions)
            .cloned()
            .collect();
        refs.extend([
            request.anchor.clone(),
            wake.fire.watch_ref.clone(),
            wake.fire_activity_ref.clone(),
        ]);
        // Commitments are always delivered as required constraints. A full
        // page fails closed; a host cannot use a model's selection to hide one.
        let commitments = query(
            session,
            format!(
                "FIND(?c) WHERE {{?c CONCEPT {{type:\"Commitment\"}}}} AS OF SEQ {seq} LIMIT 33"
            ),
            json!({}),
            ms,
        )
        .await?;
        let commitments = commitments.as_array().ok_or("invalid commitment page")?;
        let mut insufficient =
            (commitments.len() >= 33).then(|| "constraint_coverage_incomplete".into());
        for row in commitments.iter().take(32) {
            refs.insert(
                row["id"]
                    .as_str()
                    .ok_or("commitment reference missing")?
                    .into(),
            );
        }
        let mut items = BTreeMap::new();
        let mut pins = Vec::new();
        for id in &refs {
            let row = element(session, id, Some(seq), ms).await?;
            if row["_system"]["dependency_validity"].is_object()
                && row["_system"]["dependency_validity"]["action_eligible"] != true
            {
                insufficient.get_or_insert_with(|| "dependency_unverified".into());
            }
            if request.applied_revisions.contains(id)
                && (row["schema_ref"] != format!("{PROFILE}SkillRevision")
                    || row["attributes"]["task_family"] != request.task_family
                    || row["_system"]["dependency_validity"]["action_eligible"] != true)
            {
                insufficient.get_or_insert_with(|| "revision_unverified".into());
            }
            pins.push(json!({"id":id,"version":row["_system"]["version"]}));
            let projection = if id == &request.anchor {
                Some(anchor.clone())
            } else if id.starts_with("P-") {
                Some(belief(session, id, Some(seq), ms).await?)
            } else {
                None
            };
            if request.premises.contains(id)
                && projection
                    .as_ref()
                    .is_none_or(|p| p["status"] != "accepted")
            {
                insufficient.get_or_insert_with(|| "premise_unknown_or_not_accepted".into());
            }
            // Complete native projection includes opposition, conflict and
            // uncertainty. It is not replaced by a boolean truth assertion.
            items.insert(
                id.clone(),
                MemoryItem {
                    id: id.clone(),
                    channel: Channel::Kip,
                    priority: Priority::Required,
                    content: json!({"element":row,"belief":projection}),
                },
            );
        }
        let items: Vec<_> = items.into_values().collect();
        let packet = recall_budget::pack(
            &limits.recall,
            &items,
            &[],
            Coverage {
                queried: vec![Channel::Kip],
                ..Default::default()
            },
        )?;
        if packet.insufficient {
            insufficient.get_or_insert_with(|| "required_context_does_not_fit".into());
        }
        let packet: MemoryPacket = if packet.content == "null" {
            // A null delivery still retains insufficiency outside the packet.
            MemoryPacket {
                format: recall_budget::PACKET_FORMAT.into(),
                status: "budget_insufficient".into(),
                failed_reason: None,
                tokenizer: limits.recall.tokenizer.clone(),
                token_limit: limits.recall.max_tokens,
                items: vec![],
                coverage: Coverage {
                    omitted: vec![Channel::Kip],
                    ..Default::default()
                },
                semantic_complete: false,
                action_ready: false,
            }
        } else {
            serde_json::from_str(&packet.content)?
        };
        Ok(Self {
            receipt: None,
            request,
            basis,
            pins,
            retrieved: refs.into_iter().collect(),
            packet,
            insufficient,
        })
    }

    /// Fresh reads immediately before dispatch catch truth changes that do not
    /// change a Proposition's own version (e.g. a new opposing Assertion).
    pub async fn revalidate(
        &self,
        session: &Session,
        wake: &WakeRecord,
        limits: &ActionLimits,
        clarification: bool,
    ) -> Result<(), BoxError> {
        let fresh = Self::read(session, wake, self.request.clone(), limits).await?;
        if fresh
            .insufficient
            .as_ref()
            .is_some_and(|r| !clarification || r != "premise_unknown_or_not_accepted")
        {
            return Err("current action prerequisites are insufficient".into());
        }
        for key in [
            "schema_environment_version",
            "identity_version",
            "policy",
            "trust_version",
            "authorization_view",
            "context_refs",
            "purpose",
            "risk",
        ] {
            if fresh.basis[key] != self.basis[key] {
                return Err("action basis changed".into());
            }
        }
        if fresh.pins != self.pins {
            return Err("action context or required constraints changed".into());
        }
        Ok(())
    }
}

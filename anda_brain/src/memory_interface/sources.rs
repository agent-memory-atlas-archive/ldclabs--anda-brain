//! Staged sources: the host's `source_ref` handles (MI §3, host ergonomics).
//!
//! A business Agent stages the exact messages it observed and gets back an
//! opaque handle with the digest of what was captured; observe, revise and
//! feedback then cite the handle instead of re-typing the bytes. Admission
//! runs before anything is stored: a source a forget excluded is refused and
//! binds no key, so a secret is never stored merely to issue a handle. A
//! handle is scoped to the caller that staged it, and the staging key makes a
//! retry resolve to the same handle.
use super::*;
use anda_core::Message;
use object_store::PutMode;

/// The most messages one staged source holds.
pub const MAX_SOURCE_MESSAGES: usize = crate::kip::MAX_INGESTED_MESSAGES;
/// The most bytes one staged source's messages serialize to.
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Message,
    ToolTrace,
    Artifact,
}

/// Host-captured source order (Profile SourceOrder): a transport attestation
/// of stream, event and ordinal, and the receipts that must be processed
/// first. Never inferred from payload text or completion times (MI §5.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SourceOrder {
    pub stream_ref: String,
    pub event_ref: String,
    pub ordinal: u64,
    #[serde(default)]
    pub predecessor_receipts: Vec<String>,
}

/// `POST /v1/{space_id}/memory/sources`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StageSourceInput {
    /// The observed messages, in order; each may carry its own `timestamp`
    /// (Unix ms), which is when that message was observed.
    #[cfg_attr(feature = "mcp", schemars(with = "Vec<serde_json::Value>"))]
    pub messages: Vec<Message>,
    /// When the source was observed: an RFC 3339 instant, canonicalized to
    /// millisecond UTC. Defaults to the capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<SourceOrder>,
    /// The staging retry identity: the same key and bytes return the same
    /// handle; the same key with other bytes is `IdempotencyConflict`.
    pub idempotency_key: String,
}

/// What staging returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct StagedSourceRef {
    pub source_ref: String,
    pub source_digest: String,
    pub captured_at: String,
}

/// A staged source as retained.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedSource {
    pub source_ref: String,
    pub namespace: String,
    pub source_digest: String,
    pub kind: SourceKind,
    /// Cleared when a forget erases the source's bytes.
    pub messages: Vec<Message>,
    pub observed_at: String,
    pub captured_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<SourceOrder>,
    #[serde(default)]
    pub erased: bool,
}

/// A source an intent cites, resolved for this caller.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedSource {
    pub source_ref: String,
    pub digest: String,
    pub messages: Vec<Message>,
    pub observed_at: String,
    pub order: Option<SourceOrder>,
    /// Set when the handle is an already captured Evidence element.
    pub evidence: Option<String>,
}

/// What a message's role makes it, as an Evidence class (Formation §7).
pub(crate) fn evidence_class(role: &str) -> &'static str {
    match role {
        "user" => "user_statement",
        "assistant" => "agent_statement",
        "tool" => "tool_result",
        _ => "message",
    }
}

/// The digest of what was captured: the messages, the observation time and
/// the kind. The handle's identity is separate from it.
fn source_digest(
    messages: &[Message],
    observed_at: &str,
    kind: SourceKind,
) -> Result<String, KipError> {
    anda_cognitive_nexus::content_digest(&json!({
        "kind": kind,
        "messages": messages,
        "observed_at": observed_at,
    }))
}

impl Space {
    /// Stages a source for `namespace` (see the module docs).
    pub async fn stage_memory_source(
        self: &Arc<Self>,
        namespace: &str,
        input: StageSourceInput,
    ) -> Result<StagedSourceRef, KipError> {
        if input.idempotency_key.is_empty() || input.idempotency_key.len() > 1024 {
            return Err(KipError::invalid_request_envelope(
                "idempotency_key is 1..=1024 characters",
            ));
        }
        if input.messages.is_empty() || input.messages.len() > MAX_SOURCE_MESSAGES {
            return Err(KipError::invalid_request_envelope(format!(
                "a staged source holds 1..={MAX_SOURCE_MESSAGES} messages"
            )));
        }
        if serde_json::to_vec(&input.messages)
            .map_err(|e| KipError::internal_error(e.to_string()))?
            .len()
            > MAX_SOURCE_BYTES
        {
            return Err(KipError::result_limit_exceeded(format!(
                "a staged source is at most {MAX_SOURCE_BYTES} bytes"
            )));
        }
        if let Some(order) = &input.order {
            if order.stream_ref.is_empty() || order.event_ref.is_empty() {
                return Err(KipError::invalid_request_envelope(
                    "a source order names its stream and event",
                ));
            }
            if order.predecessor_receipts.len() > wire::MAX_AFTER {
                return Err(KipError::result_limit_exceeded(
                    "too many predecessor receipts",
                ));
            }
            for predecessor in &order.predecessor_receipts {
                self.intake_record(namespace, predecessor).await?;
            }
        }
        let now = anda_engine::unix_ms();
        let captured_at = crate::kip::timestamp(now);
        let observed_at = match &input.observed_at {
            Some(value) => {
                crate::kip::source_timestamp(value).map_err(KipError::invalid_request_envelope)?
            }
            None => captured_at.clone(),
        };
        let digest = source_digest(&input.messages, &observed_at, input.kind)?;
        let key_id = anda_cognitive_nexus::content_digest(&json!([
            namespace,
            self.id(),
            "source",
            input.idempotency_key
        ]))?[7..47]
            .to_string();
        let source_ref = format!("src-{key_id}");
        // Admission precedes capture: bytes a forget excluded are refused
        // before they are stored, and the key stays unbound.
        let identity = crate::product::SourceIdentity {
            key: format!("memory-source:{source_ref}"),
            parents: vec![format!("memory-source-digest:{digest}")],
        };
        if !self.product_source_allowed(&identity) {
            return Err(KipError::not_found_or_not_visible(
                "this source is excluded from memory by an earlier forget",
            ));
        }
        let journal = &self.memory_interface.journal;
        let path = format!("sources/{source_ref}");
        let _gate = self.memory_interface.gate.lock().await;
        if let Some(existing) = journal
            .read::<StagedSource>(&path)
            .await
            .map_err(kip_error)?
        {
            let existing = existing.value;
            if existing.namespace != namespace {
                return Err(KipError::not_found_or_not_visible("source not found"));
            }
            if existing.source_digest != digest || existing.order != input.order {
                return Err(KipError::new(
                    KipErrorCode::IdempotencyConflict,
                    "this staging key was used for different source bytes",
                ));
            }
            return Ok(StagedSourceRef {
                source_ref,
                source_digest: existing.source_digest,
                captured_at: existing.captured_at,
            });
        }
        let staged = StagedSource {
            source_ref: source_ref.clone(),
            namespace: namespace.to_string(),
            source_digest: digest.clone(),
            kind: input.kind,
            messages: input.messages,
            observed_at,
            captured_at: captured_at.clone(),
            order: input.order,
            erased: false,
        };
        journal
            .put(&path, &staged, PutMode::Create)
            .await
            .map_err(kip_error)?;
        Ok(StagedSourceRef {
            source_ref,
            source_digest: digest,
            captured_at,
        })
    }

    /// A staged source's retained record, for its owner.
    pub async fn staged_memory_source(
        &self,
        namespace: &str,
        source_ref: &str,
    ) -> Result<StagedSource, KipError> {
        let path = source_path(source_ref)?;
        let staged = self
            .memory_interface
            .journal
            .read::<StagedSource>(&path)
            .await
            .map_err(kip_error)?
            .ok_or_else(|| KipError::not_found_or_not_visible("source not found"))?
            .value;
        if staged.namespace != namespace {
            return Err(KipError::not_found_or_not_visible("source not found"));
        }
        Ok(staged)
    }

    /// Resolves a `source_ref`: a staged handle the caller owns, or an active
    /// Evidence element of this Space. A missing or erased one is
    /// existence-neutral (MI §3).
    pub(crate) async fn resolve_source(
        &self,
        namespace: &str,
        source_ref: &str,
    ) -> Result<ResolvedSource, KipError> {
        if source_ref.starts_with("src-") {
            let staged = self.staged_memory_source(namespace, source_ref).await?;
            if staged.erased {
                return Err(KipError::not_found_or_not_visible("source not found"));
            }
            return Ok(ResolvedSource {
                source_ref: staged.source_ref,
                digest: staged.source_digest,
                messages: staged.messages,
                observed_at: staged.observed_at,
                order: staged.order,
                evidence: None,
            });
        }
        let not_found = || KipError::not_found_or_not_visible("source not found");
        let id: ElementId = source_ref.parse().map_err(|_| not_found())?;
        let Ok(Element::Evidence(row)) = self.memory.nexus().store.get_element(id).await else {
            return Err(not_found());
        };
        if row.space != DEFAULT_SPACE || row.state != "active" || row.payload_mode != "inline" {
            return Err(not_found());
        }
        let message =
            serde_json::from_value::<Message>(row.payload_inline.clone()).unwrap_or_else(|_| {
                Message {
                    role: "user".into(),
                    content: vec![row.payload_inline.to_string().into()],
                    ..Default::default()
                }
            });
        Ok(ResolvedSource {
            source_ref: id.to_string(),
            digest: row.content_digest.clone(),
            messages: vec![message],
            observed_at: if row.observed_at.is_empty() {
                row.created_at.clone()
            } else {
                row.observed_at.clone()
            },
            order: None,
            evidence: Some(id.to_string()),
        })
    }

    /// Erases a staged source's bytes, keeping only its non-content identity
    /// so a replay still names it (MI §5). Returns whether bytes were held.
    pub(crate) async fn erase_staged_source(&self, source_ref: &str) -> Result<bool, KipError> {
        let path = source_path(source_ref)?;
        let journal = &self.memory_interface.journal;
        loop {
            let Some(stored) = journal
                .read::<StagedSource>(&path)
                .await
                .map_err(kip_error)?
            else {
                return Ok(false);
            };
            if stored.value.erased {
                return Ok(false);
            }
            let mut staged = stored.value;
            staged.messages.clear();
            staged.erased = true;
            if journal
                .put(&path, &staged, PutMode::Update(stored.version))
                .await
                .is_ok()
            {
                return Ok(true);
            }
        }
    }
}

fn source_path(source_ref: &str) -> Result<String, KipError> {
    let id = source_ref
        .strip_prefix("src-")
        .filter(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| KipError::not_found_or_not_visible("source not found"))?;
    Ok(format!("sources/src-{id}"))
}

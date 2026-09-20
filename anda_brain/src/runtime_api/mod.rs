//! Credentialed product access to durable attention and independent observations.
//! Installation is a trusted host capability; ordinary API bodies cannot grant it.
use anda_cognitive_nexus::{
    attention::{RuntimePin, RuntimeScope},
    governance::AuthContext,
};
use anda_core::{BoxError, Json};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub mod config;
mod inbox;
mod native;
mod service;
pub(crate) use native::full_read;
pub use service::MemoryRuntime;

pub const FORMAT: &str = "anda-brain:runtime-api-v1";
pub(crate) const PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";

#[derive(Clone, Default)]
pub struct MemoryRuntimeBindings {
    pub spaces: BTreeMap<String, SpaceRuntimeBindings>,
}

#[derive(Clone)]
pub struct SpaceRuntimeBindings {
    pub pin: RuntimePin,
    pub subjects: Vec<SubjectMapping>,
    pub observers: Vec<crate::consequence::ObserverContract>,
    pub actions: Option<crate::action::ActionBindings>,
    pub inbox: Option<config::InboxAdapter>,
    pub utility: Option<crate::consequence::UtilityConfig>,
    pub trust: Option<crate::consequence::trust::TrustConfig>,
    pub semantic: Option<crate::attention::semantic::SemanticBindings>,
    #[cfg(feature = "learning")]
    pub learning: Option<crate::learning::LearningBindings>,
    /// Explicit native principal/grant provisioning. A revoked grant is never
    /// restored merely because the service restarts with the same configuration.
    pub bootstrap: bool,
    /// Additional audience restriction, on top of current native visibility.
    pub audience: BTreeSet<String>,
    pub inbox_recipient: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeCredential {
    CwtSubject {
        subject: String,
    },
    /// Digest of an already registered Space token, never its display name.
    SpaceTokenDigest {
        digest: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectMapping {
    pub credential: RuntimeCredential,
    pub principal: String,
    #[serde(default)]
    pub observer: bool,
    /// Explicit auditor visibility; normal readers remain recipient restricted.
    #[serde(default)]
    pub audit_recipients: bool,
}

/// Constructed from a verified credential by the channel adapter, not serde.
pub struct RuntimeCaller {
    pub(crate) auth: AuthContext,
    pub(crate) observer: bool,
    pub(crate) audit_recipients: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct AttentionQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionPage {
    pub scope: RuntimeScope,
    pub items: Vec<AttentionItem>,
    pub next_cursor: Option<String>,
    /// Completion of this bounded visible snapshot walk, not action completion.
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionItem {
    /// URL-safe native wake identifier (the hash after wake/v1/).
    pub id: String,
    pub wake_ref: String,
    pub parent_id: Option<String>,
    pub watch_ref: String,
    pub fire_activity_ref: String,
    pub summary: String,
    pub state: String,
    pub reason: Option<String>,
    pub decision: Option<Json>,
    pub decision_ref: Option<String>,
    pub attempt_ref: Option<String>,
    pub dispatch_ref: Option<String>,
    pub clarification: Option<Json>,
    pub delivery: Option<Json>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttentionResponse {
    Clarification {
        event_key: String,
        answer: String,
    },
    /// Business-agent statements are retained as statements, never Outcomes.
    AgentStatement {
        event_key: String,
        statement: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseReceipt {
    pub receipt_id: String,
    pub status: String,
    pub evidence_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStatus {
    #[serde(default)]
    pub utility: crate::consequence::utility::UtilityStatus,
    #[serde(default)]
    pub trust: crate::consequence::trust::TrustStatus,
    #[serde(default)]
    pub semantic_attention: crate::attention::semantic::SemanticStatus,
    pub supported: bool,
    pub configured: bool,
    pub scope: Option<RuntimeScope>,
    pub attention_enabled: bool,
    pub actions_enabled: bool,
    pub observation_enabled: bool,
    pub observer_authenticated: bool,
    pub blocked_reasons: Vec<String>,
    /// Counts are limited to this caller's visible page. No global backlog leak.
    pub visible_items: usize,
    pub inventory_complete: bool,
    #[serde(default)]
    pub learning: Json,
}
impl RuntimeStatus {
    pub(crate) fn unconfigured() -> Self {
        Self {
            supported: true,
            utility: Default::default(),
            trust: Default::default(),
            semantic_attention: Default::default(),
            configured: false,
            scope: None,
            attention_enabled: false,
            actions_enabled: false,
            observation_enabled: false,
            observer_authenticated: false,
            blocked_reasons: vec!["runtime_bindings_not_installed".into()],
            visible_items: 0,
            inventory_complete: false,
            learning: serde_json::json!({"compiled":cfg!(feature="learning"),"registered":false,"bindings_ready":false,"automatic_allowed":false}),
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Invalid(String),
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict(String),
    Unavailable(String),
    Storage(BoxError),
}
impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) | Self::Conflict(s) | Self::Unavailable(s) => f.write_str(s),
            Self::Unauthorized => f.write_str("runtime authentication required"),
            Self::Forbidden => f.write_str("runtime operation is not authorized"),
            Self::NotFound => f.write_str("runtime item not found or not visible"),
            Self::Storage(_) => f.write_str("runtime storage operation unavailable"),
        }
    }
}
impl std::error::Error for RuntimeError {}
impl From<BoxError> for RuntimeError {
    fn from(e: BoxError) -> Self {
        Self::Storage(e)
    }
}
impl From<anda_kip::KipError> for RuntimeError {
    fn from(e: anda_kip::KipError) -> Self {
        use anda_kip::KipErrorCode::*;
        match e.code {
            NotAuthorized => Self::Forbidden,
            NotFoundOrNotVisible => Self::NotFound,
            VersionConflict => Self::Conflict(
                "native runtime basis/version changed; re-read before retrying".into(),
            ),
            _ if e.code.name().starts_with("Invalid") || e.code == ConstraintViolation => {
                Self::Invalid("invalid native runtime parameters".into())
            }
            _ => Self::Storage(e.into()),
        }
    }
}
pub type RuntimeResult<T> = Result<T, RuntimeError>;

impl MemoryRuntimeBindings {
    pub fn validate(&self) -> Result<(), BoxError> {
        if self.spaces.len() > 128 {
            return Err("runtime configuration exceeds 128 Spaces".into());
        }
        for (id, cfg) in &self.spaces {
            anda_db::schema::validate_field_name(id)?;
            if id.is_empty()
                || id.len() > 128
                || id.contains(['/', '\\'])
                || id.starts_with("__brain_runtime__")
            {
                return Err("invalid runtime Space id".into());
            }
            if cfg.subjects.len() > 128
                || cfg.observers.len() > 32
                || cfg.audience.len() > 128
                || !digest_valid(&cfg.pin.digest)
                || cfg.pin.id.is_empty()
            {
                return Err("invalid bounded runtime configuration".into());
            }
            let mut credentials = BTreeSet::new();
            for subject in &cfg.subjects {
                if !principal_valid(&subject.principal)
                    || !credentials.insert(serde_json::to_string(&subject.credential)?)
                {
                    return Err("invalid or duplicate runtime identity mapping".into());
                }
                match &subject.credential {
                    RuntimeCredential::CwtSubject { subject } => {
                        anda_core::Principal::from_text(subject)?;
                    }
                    RuntimeCredential::SpaceTokenDigest { digest }
                        if !digest_valid(digest) || subject.observer =>
                    {
                        return Err("Space tokens cannot act as independent observers".into());
                    }
                    _ => {}
                }
            }
            if cfg.audience.iter().any(|id| !principal_valid(id))
                || cfg
                    .inbox_recipient
                    .as_ref()
                    .is_some_and(|id| !principal_valid(id))
            {
                return Err("invalid runtime audience".into());
            }
            for observer in &cfg.observers {
                observer.validate()?;
            }
            if let Some(semantic) = &cfg.semantic {
                semantic.config.validate()?;
                if cfg
                    .subjects
                    .iter()
                    .any(|s| s.principal == semantic.config.principal)
                    || cfg
                        .observers
                        .iter()
                        .any(|s| s.principal_id == semantic.config.principal)
                    || cfg
                        .inbox
                        .as_ref()
                        .is_some_and(|a| a.controller_principal != semantic.config.principal)
                {
                    return Err("semantic controller must be separate from callers/observers and match the attention controller".into());
                }
            }
            if let Some(trust) = &cfg.trust {
                trust.validate()?;
                if cfg.subjects.iter().any(|s| {
                    s.principal == trust.proposer_principal
                        || Some(&s.principal) == trust.governor_principal.as_ref()
                }) || cfg.observers.iter().any(|s| {
                    s.principal_id == trust.proposer_principal
                        || Some(&s.principal_id) == trust.governor_principal.as_ref()
                }) {
                    return Err("trust proposer/governor must be separate from channel callers and observers".into());
                }
            }
            if let Some(utility) = &cfg.utility {
                utility.validate()?;
            }
            #[cfg(feature = "learning")]
            if let Some(learning) = &cfg.learning {
                learning.validate()?;
                if cfg.subjects.iter().any(|s| {
                    s.principal == learning.registration.executor_principal
                        || (s.principal == learning.registration.observer.principal_id
                            && !s.observer)
                }) {
                    return Err("learning executor must be separate from runtime callers and observer must be explicitly mapped".into());
                }
            }
            if let Some(actions) = &cfg.actions {
                actions.validate()?;
            }
            if let Some(inbox) = &cfg.inbox {
                inbox.validate()?;
                if !cfg
                    .subjects
                    .iter()
                    .any(|s| s.principal == inbox.recipient_principal)
                {
                    return Err("inbox recipient requires an explicit credential mapping".into());
                }
            }
            if cfg.actions.is_some() && cfg.inbox.is_some() {
                return Err("choose one action adapter per Space".into());
            }
        }
        Ok(())
    }
}

pub(crate) fn principal_valid(p: &str) -> bool {
    p.starts_with("kip:principal:")
        && p.len() > 14
        && p.len() <= 256
        && !p.chars().any(char::is_control)
        && p.trim() == p
        && p != anda_cognitive_nexus::governance::SYSTEM_PRINCIPAL
        && p != anda_cognitive_nexus::governance::ANONYMOUS_PRINCIPAL
}
pub(crate) fn digest_valid(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub(crate) fn key(scope: &RuntimeScope, kind: &str, id: &str) -> Result<String, BoxError> {
    Ok(format!(
        "runtime-api/{kind}/{}",
        &anda_cognitive_nexus::content_digest(&serde_json::json!({"scope":scope,"id":id}))?[7..]
    ))
}
pub fn credential_digest(token: &str) -> String {
    let hash = ic_cose_types::cose::sha256(token.as_bytes());
    format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
pub(crate) fn item_id(reference: &str) -> RuntimeResult<String> {
    let id = reference
        .strip_prefix("wake/v1/")
        .ok_or(RuntimeError::NotFound)?;
    if !digest_valid(&format!("sha256:{id}")) {
        return Err(RuntimeError::NotFound);
    }
    Ok(id.into())
}
pub(crate) fn wake_ref(id: &str) -> RuntimeResult<String> {
    if !digest_valid(&format!("sha256:{id}")) {
        return Err(RuntimeError::NotFound);
    }
    Ok(format!("wake/v1/{id}"))
}

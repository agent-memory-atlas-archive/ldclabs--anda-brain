//! Versioned startup selection of compiled adapters. No arbitrary URLs, commands
//! or credentials are accepted through a memory/observer request.
use super::*;
use crate::action::*;
use anda_cognitive_nexus::{CognitiveNexus, attention::WakeRecord, nexus::DEFAULT_SPACE};
use async_trait::async_trait;
use object_store::PutMode;
use serde_json::json;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub format: String,
    pub spaces: BTreeMap<String, SpaceConfig>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceConfig {
    #[serde(default)]
    pub bootstrap: bool,
    pub subjects: Vec<ConfiguredSubject>,
    pub audience: BTreeSet<String>,
    #[serde(default)]
    pub observers: Vec<crate::consequence::ObserverContract>,
    pub adapter: Option<InboxAdapter>,
    /// Optional compiled learning adapter. Lean builds reject installation.
    #[serde(default)]
    pub learning: Option<Json>,
    #[serde(default)]
    pub utility: Option<crate::consequence::UtilityConfig>,
    #[serde(default)]
    pub trust: Option<crate::consequence::trust::TrustConfig>,
    #[serde(default)]
    pub semantic: Option<crate::attention::semantic::OpenAiWatchConfig>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredSubject {
    pub credential: ConfigCredential,
    pub principal: String,
    #[serde(default)]
    pub observer: bool,
    #[serde(default)]
    pub audit_recipients: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigCredential {
    CwtSubject { subject: String },
    SpaceTokenEnv { variable: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InboxAdapter {
    pub id: String,
    pub controller_principal: String,
    pub recipient_principal: String,
    pub message: String,
    /// Omit for a simple notification. An answer only produces another inbox
    /// delivery; it cannot authorize an arbitrary external business operation.
    pub question: Option<String>,
    pub reply_timeout_ms: u64,
    pub context: Option<ContextRequest>,
    #[serde(default)]
    pub limits: ActionLimits,
}
impl InboxAdapter {
    pub fn validate(&self) -> Result<(), BoxError> {
        self.limits.validate()?;
        if self.id != "attention_inbox_v1" {
            return Err("unknown runtime adapter; compiled adapter: attention_inbox_v1".into());
        }
        if !principal_valid(&self.controller_principal)
            || !principal_valid(&self.recipient_principal)
            || self.controller_principal == self.recipient_principal
            || self.message.is_empty()
            || self.message.len() > 4096
            || self
                .question
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 4096)
            || !(1..=604_800_000).contains(&self.reply_timeout_ms)
        {
            return Err("invalid inbox controller/recipient/message/deadline".into());
        }
        Ok(())
    }
    pub(crate) fn bind(
        &self,
        nexus: Arc<CognitiveNexus>,
        directory: Arc<crate::attention::Directory>,
    ) -> Result<ActionBindings, BoxError> {
        self.validate()?;
        let digest = anda_cognitive_nexus::content_digest(&json!(self))?;
        let executor = Arc::new(InboxExecutor {
            directory,
            recipient: self.recipient_principal.clone(),
        });
        let binding = ActionBindings {
            policy_pin: RuntimePin {
                id: "brain:inbox-policy-v1".into(),
                digest: digest.clone(),
            },
            binding_pin: RuntimePin {
                id: "brain:attention-inbox-v1".into(),
                digest,
            },
            policy: Arc::new(InboxPolicy {
                nexus,
                config: self.clone(),
            }),
            identity: Arc::new(Controller(self.controller_principal.clone())),
            business: Some(executor.clone()),
            clarification: Some(ClarificationBinding {
                executor,
                recipient_principal: self.recipient_principal.clone(),
                reply_timeout_ms: self.reply_timeout_ms,
            }),
            lookup: None,
            limits: self.limits.clone(),
        };
        binding.validate()?;
        Ok(binding)
    }
}

impl RuntimeConfig {
    /// Static validation only: resolve configured secrets and compiled bindings
    /// without loading a Space, probing services or provisioning authority.
    pub fn validate(&self, secret: impl FnMut(&str) -> Option<String>) -> Result<(), BoxError> {
        self.clone().resolve(secret).map(|_| ())
    }

    pub(crate) fn resolve(
        self,
        mut secret: impl FnMut(&str) -> Option<String>,
    ) -> Result<MemoryRuntimeBindings, BoxError> {
        if self.format != FORMAT {
            return Err(
                "unsupported BRAIN_RUNTIME_CONFIG format; expected anda-brain:runtime-api-v1"
                    .into(),
            );
        }
        let mut result = MemoryRuntimeBindings::default();
        for (space, cfg) in self.spaces {
            let semantic = cfg
                .semantic
                .as_ref()
                .map(|s| s.resolve(&mut secret))
                .transpose()?;
            #[cfg(feature = "learning")]
            let learning = cfg
                .learning
                .as_ref()
                .map(|v| -> Result<_, BoxError> {
                    let config: crate::learning::workflow_http::WorkflowHttpConfig =
                        serde_json::from_value(v.clone())?;
                    config.resolve(&mut secret)
                })
                .transpose()?;
            #[cfg(not(feature = "learning"))]
            if cfg.learning.is_some() {
                return Err("learning adapter requires the learning Cargo feature".into());
            }
            let mut subjects = Vec::new();
            for subject in cfg.subjects {
                let credential = match subject.credential {
                    ConfigCredential::CwtSubject { subject } => {
                        RuntimeCredential::CwtSubject { subject }
                    }
                    ConfigCredential::SpaceTokenEnv { variable } => {
                        if variable.is_empty()
                            || variable.len() > 128
                            || !variable
                                .bytes()
                                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                        {
                            return Err("invalid runtime secret environment reference".into());
                        }
                        let value = secret(&variable)
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| format!("runtime secret {variable} is unavailable"))?;
                        RuntimeCredential::SpaceTokenDigest {
                            digest: credential_digest(&value),
                        }
                    }
                };
                subjects.push(SubjectMapping {
                    credential,
                    principal: subject.principal,
                    observer: subject.observer,
                    audit_recipients: subject.audit_recipients,
                });
            }
            if let Some(adapter) = &cfg.adapter
                && (subjects
                    .iter()
                    .any(|s| s.principal == adapter.controller_principal)
                    || cfg
                        .observers
                        .iter()
                        .any(|o| o.principal_id == adapter.controller_principal)
                    || !cfg.audience.contains(&adapter.recipient_principal))
            {
                return Err("inbox controller must be separate from callers/observers and recipient must belong to audience".into());
            }
            let mut manifest = json!({"subjects":subjects,"audience":cfg.audience,"observers":cfg.observers,"adapter":cfg.adapter,"bootstrap":cfg.bootstrap});
            // Preserve the existing inbox pin when no learning adapter was added.
            if let Some(learning) = &cfg.learning {
                manifest["learning"] = learning.clone();
            }
            if let Some(trust) = &cfg.trust {
                manifest["trust"] = json!(trust);
            }
            if let Some(utility) = &cfg.utility {
                manifest["utility"] = json!(utility);
            }
            if let Some(semantic) = &cfg.semantic {
                manifest["semantic"] = json!(semantic);
            }
            let digest = anda_cognitive_nexus::content_digest(&manifest)?;
            result.spaces.insert(
                space,
                SpaceRuntimeBindings {
                    pin: RuntimePin {
                        id: FORMAT.into(),
                        digest,
                    },
                    subjects,
                    observers: cfg.observers,
                    actions: None,
                    bootstrap: cfg.bootstrap,
                    audience: cfg.audience,
                    inbox_recipient: cfg.adapter.as_ref().map(|a| a.recipient_principal.clone()),
                    inbox: cfg.adapter,
                    utility: cfg.utility,
                    trust: cfg.trust,
                    semantic,
                    #[cfg(feature = "learning")]
                    learning,
                },
            );
        }
        result.validate()?;
        Ok(result)
    }
}

struct Controller(String);
#[async_trait]
impl ActionIdentity for Controller {
    async fn authenticate(&self, _: &RuntimeScope) -> Result<AuthContext, BoxError> {
        let mut auth = AuthContext::principal(&self.0);
        auth.auth_method = "brain-runtime-config:owned-controller".into();
        Ok(auth)
    }
}
struct InboxPolicy {
    nexus: Arc<CognitiveNexus>,
    config: InboxAdapter,
}
#[async_trait]
impl ActionPolicy for InboxPolicy {
    async fn context(&self, _: &WakeRecord) -> Result<ContextRequest, BoxError> {
        if let Some(context) = &self.config.context {
            return Ok(context.clone());
        }
        // An actual stored Proposition supplies a native coordinate, not truth.
        // An empty graph stays blocked until a real coordinate exists.
        let session = self.nexus.session(
            Controller(self.config.controller_principal.clone())
                .authenticate(&RuntimeScope {
                    space_id: DEFAULT_SPACE.into(),
                    space_instance: String::new(),
                })
                .await?,
        );
        let response = crate::kip::execute_readonly_request(
            &session,
            &crate::kip::request(
                "FIND(?p.id) WHERE {?p (?subject,?predicate,?object)} ORDER BY ?p.id LIMIT 1",
            ),
        )
        .await;
        let anchor = crate::kip::ok_result(&response)
            .and_then(|v| v[0].as_str())
            .ok_or("inbox gate requires an existing native Proposition coordinate")?
            .to_string();
        Ok(ContextRequest {
            recall_receipt: None,
            anchor,
            required_refs: vec![],
            premises: vec![],
            applied_revisions: vec![],
            task_family: "memory.attention.v1".into(),
            environment_digest: anda_cognitive_nexus::content_digest(
                &json!({"adapter":self.config.id,"recipient":self.config.recipient_principal}),
            )?,
            tool_versions: BTreeMap::from([("attention_inbox".into(), "1".into())]),
            deduplication_key: None,
        })
    }
    async fn suggest(&self, input: &GateInput) -> Result<Proposal, BoxError> {
        let used_refs = vec![
            input.wake.fire.watch_ref.clone(),
            input.wake.fire_activity_ref.clone(),
        ];
        if let Some(question) = &self.config.question
            && input.reply.is_none()
        {
            return Ok(Proposal::Ask {
                rationale: "Request the configured clarification through the persistent inbox"
                    .into(),
                used_refs,
                question: question.clone(),
            });
        }
        Ok(Proposal::Act {
            rationale: "Deliver this proven Watch firing to the configured persistent inbox".into(),
            used_refs,
            payload: json!({"message":self.config.message,"watch_ref":input.wake.fire.watch_ref}),
        })
    }
    async fn authorize(&self, request: &ActionRequest) -> Result<(), BoxError> {
        if request.kind == ActionKind::Business && request.payload["message"] != self.config.message
        {
            return Err("inbox policy request mismatch".into());
        }
        if request.kind == ActionKind::DeliverClarification
            && request.payload["recipient"] != self.config.recipient_principal
        {
            return Err("clarification recipient mismatch".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    pub scope: RuntimeScope,
    pub attempt_id: String,
    pub attempt_ref: String,
    pub gate_wake_ref: String,
    pub recipient: String,
    pub payload: Json,
    pub request_digest: String,
    pub committed_at_ms: u64,
}
pub(crate) fn delivery_key(scope: &RuntimeScope, attempt: &str) -> Result<String, BoxError> {
    key(scope, "deliveries", attempt)
}
struct InboxExecutor {
    directory: Arc<crate::attention::Directory>,
    recipient: String,
}
#[async_trait]
impl ActionExecutor for InboxExecutor {
    fn supports_idempotency(&self) -> bool {
        true
    }
    async fn authorize(&self, request: &ActionRequest) -> Result<(), BoxError> {
        if serde_json::to_vec(&request.payload)?.len() > 16_384 {
            return Err("inbox payload exceeds bound".into());
        }
        Ok(())
    }
    async fn dispatch(
        &self,
        request: &ActionRequest,
        permit: &DispatchPermit,
    ) -> Result<DeliveryStatus, BoxError> {
        self.authorize(request).await?;
        if permit.expires_at_ms <= anda_engine::unix_ms() {
            return Err("inbox dispatch lease expired".into());
        }
        let key = delivery_key(&request.scope, &request.attempt_id)?;
        let digest = anda_cognitive_nexus::content_digest(&json!(request))?;
        if let Some(old) = self.directory.read::<Delivery>(&key).await? {
            if old.value.scope != request.scope
                || old.value.request_digest != digest
                || old.value.attempt_ref != permit.attempt_ref
                || old.value.recipient != self.recipient
            {
                return Err("inbox delivery idempotency conflict".into());
            }
            return Ok(DeliveryStatus::Finished);
        }
        let delivery = Delivery {
            scope: request.scope.clone(),
            attempt_id: request.attempt_id.clone(),
            attempt_ref: permit.attempt_ref.clone(),
            gate_wake_ref: request.gate_wake_ref.clone(),
            recipient: self.recipient.clone(),
            payload: request.payload.clone(),
            request_digest: digest,
            committed_at_ms: anda_engine::unix_ms(),
        };
        self.directory.put(&key, &delivery, PutMode::Create).await?;
        Ok(DeliveryStatus::Finished)
    }
}

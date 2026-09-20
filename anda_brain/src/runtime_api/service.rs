use super::*;
use anda_cognitive_nexus::{
    CognitiveNexus,
    attention::AttentionConfig,
    governance::{
        Permission, ResourceContext,
        store::{GrantDraft, PrincipalDraft},
    },
    nexus::DEFAULT_SPACE,
};
use anda_db::database::AndaDB;
use object_store::PutMode;
use serde_json::json;
use tokio::sync::Mutex;

pub struct MemoryRuntime {
    trust: parking_lot::RwLock<Option<std::sync::Weak<crate::consequence::trust::TrustRuntime>>>,
    utility:
        parking_lot::RwLock<Option<std::sync::Weak<crate::consequence::utility::UtilityRuntime>>>,
    pub(super) nexus: Arc<CognitiveNexus>,
    pub(super) directory: Arc<crate::attention::Directory>,
    pub(super) attention: Arc<crate::attention::AttentionRuntime>,
    pub(super) scope: RuntimeScope,
    pub(super) bindings: Arc<SpaceRuntimeBindings>,
    pub(super) cursor_key: [u8; 32],
    pub(super) tasks: crate::runtime::DurableTasks,
    gate: Mutex<()>,
    consequences: Arc<crate::consequence::ConsequenceRuntime>,
    automatic: bool,
    #[cfg(feature = "learning")]
    learning: Option<std::sync::Weak<crate::learning::LearningRuntime>>,
}

#[derive(Deserialize, Serialize)]
struct Bootstrap {
    complete: bool,
    grant: Option<u64>,
}
#[derive(Deserialize, Serialize)]
struct Statement {
    scope: RuntimeScope,
    principal: String,
    digest: String,
    command: String,
    receipt: ResponseReceipt,
}

impl MemoryRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect(
        nexus: Arc<CognitiveNexus>,
        db: Arc<AndaDB>,
        directory: Arc<crate::attention::Directory>,
        attention: Arc<crate::attention::AttentionRuntime>,
        bindings: Arc<SpaceRuntimeBindings>,
        automatic: bool,
        #[cfg(feature = "learning")] learning: Option<
            std::sync::Weak<crate::learning::LearningRuntime>,
        >,
    ) -> Result<Arc<Self>, BoxError> {
        if attention.status().await?.is_none() {
            attention.register_work().await?;
        }
        let native = nexus
            .system_session()
            .read_control(DEFAULT_SPACE, "attention/config", None)
            .await?
            .ok_or("attention identity missing")?;
        let scope = serde_json::from_value::<AttentionConfig>(native.value)?.scope;
        if bindings.bootstrap {
            let mut principals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for subject in &bindings.subjects {
                let actions = principals.entry(subject.principal.clone()).or_default();
                actions.extend(["read", "create", "derive"].into_iter().map(String::from));
                if subject.observer {
                    actions.insert("record_outcome".into());
                    actions.insert("read_history".into());
                }
            }
            for observer in &bindings.observers {
                principals
                    .entry(observer.principal_id.clone())
                    .or_default()
                    .extend(
                        ["read", "read_history", "create", "derive", "record_outcome"]
                            .into_iter()
                            .map(String::from),
                    );
            }
            #[cfg(feature = "learning")]
            if let Some(learning) = &bindings.learning {
                principals
                    .entry(learning.registration.observer.principal_id.clone())
                    .or_default()
                    .extend(
                        ["read", "read_history", "create", "derive", "record_outcome"]
                            .into_iter()
                            .map(String::from),
                    );
            }
            if let Some(trust) = &bindings.trust {
                principals
                    .entry(trust.proposer_principal.clone())
                    .or_default()
                    .extend(
                        [
                            "read",
                            "read_history",
                            "read_governance_history",
                            "create",
                            "derive",
                        ]
                        .into_iter()
                        .map(String::from),
                    );
                principals
                    .entry(trust.observer.principal_id.clone())
                    .or_default()
                    .extend(
                        ["read", "read_history", "create", "derive", "record_outcome"]
                            .into_iter()
                            .map(String::from),
                    );
                // ManageTrust is deliberately NEVER provisioned by bootstrap.
            }
            if let Some(semantic) = &bindings.semantic {
                principals
                    .entry(semantic.config.principal.clone())
                    .or_default()
                    .extend(
                        [
                            "read",
                            "read_history",
                            "read_governance_history",
                            "create",
                            "update",
                            "derive",
                            "maintain",
                        ]
                        .into_iter()
                        .map(String::from),
                    );
            }
            if let Some(actions) = attention.actions() {
                let controller = actions.session(&scope).await?.auth().principal_id.clone();
                if bindings.subjects.iter().any(|s| s.principal == controller)
                    || bindings
                        .observers
                        .iter()
                        .any(|s| s.principal_id == controller)
                {
                    return Err(
                        "runtime controller cannot be a caller or independent observer".into(),
                    );
                }
                principals.entry(controller).or_default().extend(
                    [
                        "read",
                        "read_history",
                        "read_governance_history",
                        "project",
                        "create",
                        "update",
                        "derive",
                        "maintain",
                    ]
                    .into_iter()
                    .map(String::from),
                );
            }
            for (principal, actions) in principals {
                Self::provision(&nexus, &db, &principal, actions.into_iter().collect()).await?;
            }
        }
        let cursor_key =
            if let Some(key) = db.get_extension_as::<[u8; 32]>("runtime_api_cursor_key") {
                key
            } else {
                let key = rand::random::<[u8; 32]>();
                db.save_extension_from("runtime_api_cursor_key".into(), &key)
                    .await?;
                key
            };
        let consequences = crate::consequence::ConsequenceRuntime::new(
            nexus.clone(),
            directory.clone(),
            scope.clone(),
            bindings.observers.clone(),
            bindings.inbox.is_some(),
            #[cfg(feature = "learning")]
            learning.clone(),
        );
        Ok(Arc::new(Self {
            utility: Default::default(),
            trust: Default::default(),
            nexus,
            directory,
            attention,
            scope,
            bindings,
            cursor_key,
            tasks: Default::default(),
            gate: Mutex::new(()),
            consequences,
            automatic,
            #[cfg(feature = "learning")]
            learning,
        }))
    }
    pub(crate) fn bind_trust(
        &self,
        trust: std::sync::Weak<crate::consequence::trust::TrustRuntime>,
    ) {
        *self.trust.write() = Some(trust);
    }
    pub(crate) fn bind_utility(
        &self,
        utility: std::sync::Weak<crate::consequence::utility::UtilityRuntime>,
    ) {
        *self.utility.write() = Some(utility);
    }
    async fn provision(
        nexus: &CognitiveNexus,
        db: &AndaDB,
        principal: &str,
        actions: Vec<String>,
    ) -> Result<(), BoxError> {
        let marker = format!(
            "runtime_bootstrap/{}",
            anda_cognitive_nexus::content_digest(
                &json!({"principal":principal,"actions":actions})
            )?
        );
        let old = db.get_extension_as::<Bootstrap>(&marker);
        if old.as_ref().is_some_and(|v| v.complete) {
            return Ok(());
        }
        let grants = nexus
            .governance()
            .grants_for(DEFAULT_SPACE, principal, &[])
            .await?;
        if let Some(grant) = grants
            .iter()
            .find(|g| actions.iter().all(|a| g.actions.contains(a)))
        {
            db.save_extension_from(
                marker,
                &Bootstrap {
                    complete: true,
                    grant: Some(grant._id),
                },
            )
            .await?;
            return Ok(());
        }
        if old.is_some() {
            return Err(format!("runtime bootstrap for {principal} was interrupted; provision/review its native grants explicitly").into());
        }
        db.save_extension_from(
            marker.clone(),
            &Bootstrap {
                complete: false,
                grant: None,
            },
        )
        .await?;
        nexus
            .governance()
            .ensure_principal(PrincipalDraft {
                principal_id: principal.into(),
                principal_class: "service".into(),
                ..Default::default()
            })
            .await?;
        let grant = nexus
            .system_session()
            .create_grant(
                DEFAULT_SPACE,
                GrantDraft {
                    grantee_principal: principal.into(),
                    actions,
                    ..Default::default()
                },
            )
            .await?;
        db.save_extension_from(
            marker,
            &Bootstrap {
                complete: true,
                grant: Some(grant._id),
            },
        )
        .await?;
        Ok(())
    }
    pub fn nexus(&self) -> Arc<CognitiveNexus> {
        self.nexus.clone()
    }
    pub fn consequences(&self) -> Arc<crate::consequence::ConsequenceRuntime> {
        self.consequences.clone()
    }
    pub fn scope(&self) -> &RuntimeScope {
        &self.scope
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.tasks.is_busy() || self.consequences.is_busy()
    }
    pub(crate) async fn shutdown(&self) {
        self.tasks.shutdown().await;
        self.consequences.shutdown().await;
    }

    /// Direct host use requires a real authenticated native context. Credentials
    /// must never be reconstructed from an observation body or saved journal.
    pub fn authenticated_caller(&self, auth: AuthContext) -> RuntimeResult<RuntimeCaller> {
        if !principal_valid(&auth.principal_id)
            || auth.auth_method.is_empty()
            || auth.auth_strength == "none"
            || !auth.delegation_chain.is_empty()
        {
            return Err(RuntimeError::Unauthorized);
        }
        let mapping = self
            .bindings
            .subjects
            .iter()
            .find(|s| s.principal == auth.principal_id)
            .ok_or(RuntimeError::Forbidden)?;
        Ok(RuntimeCaller {
            auth,
            observer: mapping.observer,
            audit_recipients: mapping.audit_recipients,
        })
    }
    pub(crate) fn map_credential(
        &self,
        credential: &RuntimeCredential,
    ) -> RuntimeResult<RuntimeCaller> {
        let mapping = self
            .bindings
            .subjects
            .iter()
            .find(|m| &m.credential == credential)
            .ok_or(RuntimeError::Forbidden)?;
        let mut auth = AuthContext::principal(&mapping.principal);
        auth.auth_method = match credential {
            RuntimeCredential::CwtSubject { .. } => "brain:cwt-ed25519",
            RuntimeCredential::SpaceTokenDigest { .. } => "brain:verified-space-token",
        }
        .into();
        Ok(RuntimeCaller {
            auth,
            observer: mapping.observer
                && matches!(credential, RuntimeCredential::CwtSubject { .. }),
            audit_recipients: mapping.audit_recipients,
        })
    }
    pub async fn submit_outcome(
        &self,
        caller: RuntimeCaller,
        input: crate::consequence::OutcomeInput,
    ) -> RuntimeResult<crate::consequence::ObservationReceipt> {
        if !self.automatic {
            return Err(RuntimeError::Unavailable(
                "isolated hosts cannot accept live product outcomes".into(),
            ));
        }
        if !caller.observer {
            return Err(RuntimeError::Forbidden);
        }
        self.consequences.submit(caller.auth, input).await
    }
    pub async fn respond(
        self: &Arc<Self>,
        caller: RuntimeCaller,
        id: String,
        input: AttentionResponse,
    ) -> RuntimeResult<ResponseReceipt> {
        let this = self.clone();
        self.tasks
            .run(async move { Ok(this.respond_owned(caller, id, input).await) })
            .await
            .map_err(RuntimeError::Storage)?
    }
    async fn respond_owned(
        &self,
        caller: RuntimeCaller,
        id: String,
        input: AttentionResponse,
    ) -> RuntimeResult<ResponseReceipt> {
        let _g = self.gate.lock().await;
        if !self.automatic {
            return Err(RuntimeError::Unavailable(
                "isolated hosts cannot accept live responses".into(),
            ));
        }
        let reference = wake_ref(&id)?;
        let wake = self
            .nexus
            .system_session()
            .read_wake(DEFAULT_SPACE, &reference)
            .await?;
        let item = self
            .item(&caller, &wake)
            .await?
            .ok_or(RuntimeError::NotFound)?;
        match input {
            AttentionResponse::Clarification { event_key, answer } => {
                let question = item.clarification.as_ref().ok_or_else(|| {
                    RuntimeError::Conflict("this item has no committed clarification".into())
                })?;
                if question["recipient"] != caller.auth.principal_id {
                    return Err(RuntimeError::Forbidden);
                }
                if event_key.is_empty()
                    || event_key.len() > 256
                    || answer.trim().is_empty()
                    || answer.len() > 16_384
                {
                    return Err(RuntimeError::Invalid(
                        "invalid bounded clarification response".into(),
                    ));
                }
                let action = self.attention.actions().ok_or_else(|| {
                    RuntimeError::Unavailable("action runtime is not installed".into())
                })?;
                action
                    .respond(
                        reference.clone(),
                        caller.auth,
                        crate::action::ClarificationResponse {
                            event_key: event_key.clone(),
                            answer,
                        },
                    )
                    .await
                    .map_err(|e| {
                        let text = e.to_string();
                        if text.contains("idempotency conflict") {
                            RuntimeError::Conflict("clarification event key conflict".into())
                        } else if text.contains("deadline passed") {
                            RuntimeError::Conflict(
                                "clarification deadline passed; no consent inferred".into(),
                            )
                        } else if text.contains("invalid authenticated") {
                            RuntimeError::Forbidden
                        } else {
                            RuntimeError::Storage(e)
                        }
                    })?;
                Ok(ResponseReceipt {
                    receipt_id: anda_cognitive_nexus::content_digest(
                        &json!({"scope":self.scope,"wake":reference,"event":event_key}),
                    )?[7..]
                        .into(),
                    status: "answer_received_not_authorization".into(),
                    evidence_ref: None,
                })
            }
            AttentionResponse::AgentStatement {
                event_key,
                statement,
            } => {
                if event_key.is_empty()
                    || event_key.len() > 256
                    || statement.trim().is_empty()
                    || statement.len() > 8192
                {
                    return Err(RuntimeError::Invalid(
                        "invalid bounded agent statement".into(),
                    ));
                }
                let session = self.nexus.session(caller.auth.clone());
                let digest = anda_cognitive_nexus::content_digest(
                    &json!({"scope":self.scope,"wake":reference,"event":event_key,"statement":statement}),
                )?;
                let storage_key = key(
                    &self.scope,
                    "statements",
                    &format!(
                        "{}\u{1f}{reference}\u{1f}{event_key}",
                        caller.auth.principal_id
                    ),
                )?;
                let old = self.directory.read::<Statement>(&storage_key).await?;
                let mut row = if let Some(old) = old {
                    if old.value.scope != self.scope
                        || old.value.principal != caller.auth.principal_id
                        || old.value.digest != digest
                    {
                        return Err(RuntimeError::Conflict(
                            "agent statement event key conflict".into(),
                        ));
                    }
                    old.value
                } else {
                    let payload=crate::kip::string_literal(&json!({"kind":"agent_statement","text":statement,"wake_ref":reference,"event_key":event_key}).to_string());
                    let command = format!(
                        r#"MUTATE {{ CREATE EVIDENCE ?statement {{SET FIELDS {{evidence_class:"artifact",payload:{payload},observed_at:{}}}}} CREATE ACTIVITY ?report {{SET FIELDS {{activity_class:"agent_statement",status:"completed"}} SET STRUCTURAL {{("inputs",{}) ("outputs",?statement)}}}} }}"#,
                        crate::kip::string_literal(&anda_cognitive_nexus::time::now()),
                        crate::kip::string_literal(&wake.fire_activity_ref)
                    );
                    Statement {
                        scope: self.scope.clone(),
                        principal: caller.auth.principal_id.clone(),
                        digest: digest.clone(),
                        command,
                        receipt: ResponseReceipt {
                            receipt_id: anda_cognitive_nexus::content_digest(&json!(storage_key))?
                                [7..]
                                .into(),
                            status: "agent_statement".into(),
                            evidence_ref: None,
                        },
                    }
                };
                if let Some(reference) = &row.receipt.evidence_ref {
                    full_read(&session, reference).await?;
                    return Ok(row.receipt);
                }
                let old = self.directory.read::<Statement>(&storage_key).await?;
                self.directory
                    .put(
                        &storage_key,
                        &row,
                        old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
                    )
                    .await?;
                let mut request = crate::kip::request(row.command.clone());
                request.operations[0].idempotency_key =
                    Some(format!("brain-statement:{}", row.receipt.receipt_id));
                request.parameters = Some(crate::kip::param("statement_digest", digest));
                let response = anda_kip::execute_request(&session, &request).await;
                let value = crate::kip::ok_result(&response).ok_or_else(|| {
                    RuntimeError::Unavailable(
                        "statement commit unresolved; retry the same authenticated request".into(),
                    )
                })?;
                row.receipt.evidence_ref = Some(
                    value["handles"]["statement"]
                        .as_str()
                        .ok_or(RuntimeError::NotFound)?
                        .into(),
                );
                let old = self
                    .directory
                    .read::<Statement>(&storage_key)
                    .await?
                    .ok_or(RuntimeError::NotFound)?;
                self.directory
                    .put(&storage_key, &row, PutMode::Update(old.version))
                    .await?;
                Ok(row.receipt)
            }
        }
    }
    pub async fn status(
        &self,
        caller: &RuntimeCaller,
        signature_verifier_enabled: bool,
    ) -> RuntimeResult<RuntimeStatus> {
        let page = self
            .inbox(
                caller,
                AttentionQuery {
                    cursor: None,
                    limit: Some(20),
                },
            )
            .await?;
        let configured = self.attention.runtime_status().await?;
        let actions = self.attention.actions();
        let mut blocked_reasons = Vec::new();
        if actions.is_none() {
            blocked_reasons.push("action_bindings_not_installed".into());
        }
        if !signature_verifier_enabled {
            blocked_reasons.push("signed_observer_authentication_not_configured".into());
        }
        if self.bindings.observers.is_empty() {
            blocked_reasons.push("generic_observer_contract_not_installed".into());
        }
        let auth = self
            .nexus
            .session(caller.auth.clone())
            .effective_authority(DEFAULT_SPACE)
            .await?;
        let observer_authenticated = caller.observer
            && auth
                .authorize(
                    Permission::RecordOutcome,
                    &ResourceContext::default(),
                    &caller.auth,
                )
                .into_result()
                .is_ok();
        let http_observer =
            self.bindings.subjects.iter().any(|s| {
                s.observer && matches!(s.credential, RuntimeCredential::CwtSubject { .. })
            });
        if !http_observer {
            blocked_reasons.push("observer_identity_mapping_not_configured".into());
        }
        let utility = self
            .utility
            .read()
            .as_ref()
            .and_then(|u| u.upgrade())
            .map(|u| u.status())
            .unwrap_or_default();
        let mut semantic_attention = self.attention.semantic_status().await;
        if !caller.audit_recipients {
            semantic_attention.last_pass = None;
        }
        let trust = self.trust.read().as_ref().and_then(|t| t.upgrade());
        let trust = match trust {
            Some(t) => t.status().await,
            None => Default::default(),
        };
        Ok(RuntimeStatus {
            trust,
            semantic_attention,
            utility,
            supported: true,
            configured: true,
            scope: Some(self.scope.clone()),
            attention_enabled: configured["structured_attention"]["enabled"] == true,
            actions_enabled: configured["actions"]["enabled"] == true,
            observation_enabled: self.automatic
                && signature_verifier_enabled
                && http_observer
                && (!self.bindings.observers.is_empty() || self.consequences.has_learning()),
            observer_authenticated,
            blocked_reasons,
            visible_items: page.items.len(),
            inventory_complete: page.complete,
            learning: {
                #[cfg(feature = "learning")]
                {
                    if let Some(learning) = self.learning.as_ref().and_then(|l| l.upgrade()) {
                        serde_json::to_value(
                            learning
                                .runtime_status(self.automatic, caller.audit_recipients)
                                .await?,
                        )
                        .map_err(|e| RuntimeError::Storage(e.into()))?
                    } else {
                        json!({"compiled":true,"registered":false,"bindings_ready":false,"automatic_allowed":self.automatic})
                    }
                }
                #[cfg(not(feature = "learning"))]
                {
                    json!({"compiled":false,"registered":false,"bindings_ready":false,"automatic_allowed":false})
                }
            },
        })
    }
}

//! Cache eviction, owned shutdown and Space initialization.
use super::*;

impl AppState {
    /// Starts background maintenance tasks:
    /// - Flushes active space databases every 5 minutes.
    /// - Evicts spaces idle for over 9 minutes.
    pub async fn start_background_tasks(&self, cancel_token: CancellationToken) {
        let flush_interval = Duration::from_secs(5 * 60);
        let idle_timeout_ms: u64 = 9 * 60 * 1000;

        let mut attention_tick =
            tokio::time::interval(Duration::from_millis(self.attention_policy.tick_ms));
        attention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut flush_tick = tokio::time::interval(flush_interval);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        flush_tick.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    self.attention_directory.tasks.shutdown().await;
                    // Close all spaces concurrently so shutdown stays fast even
                    // with many loaded spaces.
                    let entries: Vec<(String, Arc<SpaceEntry>)> = {
                        let spaces = self.spaces.read().await;
                        spaces.iter().map(|(id, entry)| (id.clone(), entry.clone())).collect()
                    };
                    let mut tasks = tokio::task::JoinSet::new();
                    for (id, entry) in entries {
                        if let Some(space) = entry.cell.get().cloned() {
                            tasks.spawn(async move {
                                if let Err(err) = space.close().await {
                                    log::error!(target: "brain", space_id = id; "close on shutdown failed: {err:?}");
                                }
                            });
                        }
                    }
                    while tasks.join_next().await.is_some() {}
                    return;
                }
                _ = attention_tick.tick() => {
                    if let Err(err) = self.attention_tick().await {
                        log::error!(target: "brain", "attention scheduling failed: {err}");
                    }
                }
                _ = flush_tick.tick() => {
                    self.flush_and_evict_once(unix_ms(), idle_timeout_ms).await;
                }
            }
        }
    }

    pub(super) async fn flush_and_evict_once(&self, now: u64, idle_timeout_ms: u64) {
        // Collect entries snapshot under read lock
        let entries: Vec<(String, Arc<SpaceEntry>)> = {
            let spaces = self.spaces.read().await;
            spaces.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };

        for (id, entry) in &entries {
            if self
                .try_evict_idle_space(id, entry, now, idle_timeout_ms)
                .await
            {
                log::warn!(target: "brain", space_id = id; "space evicted due to inactivity");
                continue;
            }

            if entry.closing.load(Ordering::Acquire) {
                continue;
            }
            // Periodic flush for active spaces
            if let Some(space) = entry.cell.get() {
                if let Err(err) = space.flush().await {
                    log::error!(target: "brain", space_id = id; "periodic flush failed: {err:?}");
                }
                // ... and the clock-driven maintenance trigger. A Space that
                // is read but never written to stays resident and never hits
                // a counting threshold, so this is the only thing that
                // metabolizes it.
                space.kick_scheduled_maintenance();
            }
        }
    }

    /// Mark the entry under the map lock, then close without blocking other
    /// Spaces. A failed/cancelled close retains the owner for a later retry.
    pub(super) async fn try_evict_idle_space(
        &self,
        id: &str,
        entry: &Arc<SpaceEntry>,
        now_ms: u64,
        idle_timeout_ms: u64,
    ) -> bool {
        {
            let mut spaces = self.spaces.write().await;
            if !spaces
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
            {
                return false;
            }
            if !entry.closing.load(Ordering::Acquire) {
                if now_ms.saturating_sub(entry.last_access_ms()) <= idle_timeout_ms
                    || Arc::strong_count(entry) > 2
                {
                    return false;
                }
                let Some(space) = entry.cell.get() else {
                    return spaces.remove(id).is_some();
                };
                #[cfg(feature = "wiki")]
                if space.wiki_digest.is_processing() || space.wiki.is_busy() {
                    return false;
                }
                if space.pinned || space.is_busy() || Arc::strong_count(space) > 1 {
                    return false;
                }
                entry.closing.store(true, Ordering::Release);
            }
        }
        if let Some(space) = entry.cell.get()
            && let Err(err) = space.close().await
        {
            log::error!(target: "brain", space_id = id; "close before eviction failed; owner retained: {err:?}");
            return false;
        }
        let mut spaces = self.spaces.write().await;
        if spaces
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            spaces.remove(id);
            true
        } else {
            false
        }
    }
}

impl Space {
    /// Closes the space's database so AndaDB flushes collections and
    /// metadata. Callers that open throwaway spaces (eval runs, forks) must
    /// close them through this method instead of reaching into the DB handle.
    pub async fn close(&self) -> Result<(), BoxError> {
        let mut state = self.close_state.lock().await;
        if state.closed {
            return Ok(());
        }
        self.capture_interrupted_work();
        self.tasks.cancel();
        self.engine.cancel();
        self.model_cancel.cancel();
        self.product_control.tasks.shutdown().await;
        if let Some(runtime) = &self.memory_runtime {
            runtime.shutdown().await;
        }
        #[cfg(feature = "learning")]
        self.learning.shutdown().await;
        self.trust.shutdown().await;
        self.attention.shutdown().await;
        self.recall_receipts.shutdown().await;
        self.utility.shutdown().await;
        self.tasks.shutdown().await;
        self.native_tasks.shutdown().await;
        #[cfg(feature = "wiki")]
        self.wiki.shutdown().await;

        // A hard stop is needed for an unresponsive provider, but it may also
        // drop a KIP/document write after its durable PUT. Treat that as a
        // crash: recover under Nexus's exclusive lock before closing, never
        // flush a poisoned generation or blindly replay the model workflow.
        if !state.reconciled {
            self.memory.nexus().recover().await?;
            state.collections.clear();
            for name in self.db.metadata().collections {
                let collection = self
                    .db
                    .open_collection(name, async |collection| {
                        collection.set_tokenizer(jieba_tokenizer());
                        Ok(())
                    })
                    .await?;
                state.collections.push(collection);
            }
            for name in ["conversations", "maintenance", "recall"] {
                let collection = self
                    .db
                    .open_collection(name.into(), async |collection| {
                        collection.set_tokenizer(jieba_tokenizer());
                        Ok(())
                    })
                    .await?;
                let interrupted = self
                    .interrupted_conversations
                    .lock()
                    .get(name)
                    .cloned()
                    .unwrap_or_default();
                // Select the status column first, without fetching every
                // historical message array. The host needs all active ids;
                // query_ids would silently cap this cleanup at MAX_SEARCH_LIMIT.
                let mut ids: BTreeSet<_> = collection
                    .query_all_ids(anda_db::query::Filter::Field((
                        "status".into(),
                        anda_db::query::RangeQuery::Eq(Fv::Text(
                            ConversationStatus::Working.to_string(),
                        )),
                    )))
                    .await?
                    .into_iter()
                    .collect();
                ids.extend(interrupted.iter().copied());
                for id in ids {
                    let conversation: Conversation = collection.get_as(id).await?;
                    if matches!(
                        conversation.status,
                        ConversationStatus::Completed | ConversationStatus::Cancelled
                    ) {
                        continue;
                    }
                    if conversation.status == ConversationStatus::Working
                        || interrupted.contains(&id)
                    {
                        let previous = conversation
                            .failed_reason
                            .map(|reason| format!("; previous failure: {reason}"))
                            .unwrap_or_default();
                        collection.update(id, BTreeMap::from([
                            ("status".into(), Fv::Text(ConversationStatus::Cancelled.to_string())),
                            ("failed_reason".into(), Fv::Text(format!("outcome_unknown: Space closed during processing; reconcile committed effects before retry{previous}"))),
                            ("updated_at".into(), Fv::U64(unix_ms())),
                        ])).await?;
                    }
                }
            }
            state.reconciled = true;
        } else {
            // A previous close may have failed or been cancelled while one
            // collection checkpoint was in flight. Healthy closed generations
            // stay closed; only poisoned checkpoint generations are reopened.
            // Do not send Nexus queries through its already-closed slots.
            for index in 0..state.collections.len() {
                if state.collections[index].is_poisoned() {
                    let name = state.collections[index].name().to_string();
                    state.collections[index] = self
                        .db
                        .open_collection(name, async |collection| {
                            collection.set_tokenizer(jieba_tokenizer());
                            Ok(())
                        })
                        .await?;
                }
            }
        }
        self.db.close().await?;
        state.closed = true;
        Ok(())
    }

    pub(super) fn capture_interrupted_work(&self) {
        let mut interrupted = self.interrupted_conversations.lock();
        for (name, id) in [
            ("conversations", self.formation.processing_id()),
            ("maintenance", self.maintenance.processing_id()),
        ] {
            if id != 0 {
                interrupted.entry(name).or_default().insert(id);
            }
        }
    }

    pub(super) async fn create(
        object_store: Arc<dyn ObjectStore>,
        db_config: DBConfig,
        creator: Principal,
        owner: Principal,
        tier: u32,
        now_ms: u64,
    ) -> Result<SpaceInfo, BoxError> {
        let id = db_config.name.clone();
        let db = AndaDB::create(object_store.clone(), db_config).await?;
        let tier = SpaceTier {
            tier,
            updated_at: now_ms,
        };

        db.set_extension_from("creator".to_string(), creator.to_string());
        db.set_extension_from("owner".to_string(), owner.to_string());
        db.set_extension_from("tier".to_string(), &tier);

        let db = Arc::new(db);
        let nexus = CognitiveNexus::connect(db.clone()).await?;
        init_nexus_kip(&nexus).await?;

        let nexus = Arc::new(nexus);
        // Creates the conversation and resource collections; `connect` below
        // reopens them with the brain's leaner index layout.
        MemoryManagement::connect(db.clone(), nexus.clone()).await?;
        let info = SpaceInfo {
            id: id.clone(),
            name: None,
            description: None,
            owner: owner.to_string(),
            db_stats: db.stats(),
            concepts: nexus.store.concepts().len(),
            propositions: nexus.store.propositions().len(),
            // The space was just created, so its conversation collection is
            // necessarily empty.
            conversations: 0,
            public: false,
            tier,
            ..Default::default()
        };
        db.close().await?;
        Ok(info)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn connect(
        object_store: Arc<dyn ObjectStore>,
        db_config: DBConfig,
        management: Arc<dyn Management>,
        http_client: reqwest::Client,
        models: Arc<Models>,
        llm_semaphore: Arc<tokio::sync::Semaphore>,
        pinned: bool,
        autostart: bool,
        clock: Arc<crate::runtime::BusinessClock>,
        automatic: bool,
        prompts: crate::agents::prompts::AgentPrompts,
        attention_directory: Arc<crate::attention::Directory>,
        attention_policy: crate::attention::AttentionPolicy,
        action_bindings: Option<Arc<crate::action::ActionBindings>>,
        memory_runtime_bindings: Option<Arc<crate::runtime_api::SpaceRuntimeBindings>>,
    ) -> Result<Arc<Self>, BoxError> {
        let id = db_config.name.clone();
        let db = Arc::new(AndaDB::open(object_store.clone(), db_config).await?);
        let nexus = CognitiveNexus::connect(db.clone()).await?;
        init_nexus_kip(&nexus).await?;
        reset_v1_bookkeeping(&db).await?;
        let mut schema = Conversation::schema()?;
        schema.with_version(4);

        let conversations = db
            .open_or_create_collection(
                schema.clone(),
                CollectionConfig {
                    name: "conversations".to_string(),
                    description: "conversations collection".to_string(),
                },
                async |collection| init_conversation_collection(collection).await,
            )
            .await?;

        let recall_conversations = db
            .open_or_create_collection(
                schema.clone(),
                CollectionConfig {
                    name: "recall".to_string(),
                    description: "Recall conversations collection".to_string(),
                },
                async |collection| init_conversation_collection(collection).await,
            )
            .await?;

        let maintenance_conversations = db
            .open_or_create_collection(
                schema.clone(),
                CollectionConfig {
                    name: "maintenance".to_string(),
                    description: "Maintenance conversations collection".to_string(),
                },
                async |collection| init_conversation_collection(collection).await,
            )
            .await?;

        db.open_or_create_collection(
            Resource::schema()?,
            CollectionConfig {
                name: "resources".to_string(),
                description: "Resources collection".to_string(),
            },
            async |collection| init_resource_collection(collection).await,
        )
        .await?;

        // The engine wrappers below open the same collections by name, and
        // `open_or_create_collection` hands back the already-open handle, so
        // they adopt the leaner index layout applied above instead of
        // recreating the indexes `init_*_collection` just dropped.
        // The KIP tool definition comes from `anda_kip` itself (via the
        // engine's default), so the schema the model is shown and the envelope
        // the engine executes stay in step across protocol revisions.
        // The Memory Interface this Brain serves is declared to the Nexus, so
        // `DESCRIBE CAPABILITIES`/`PRIMER` and every `requires` block report
        // the binding truthfully (Spec §67.4, MI §2).
        nexus.set_host_capabilities(crate::memory_interface::host_capabilities(&id))?;
        let memory = Arc::new(MemoryManagement::connect(db.clone(), Arc::new(nexus)).await?);
        let memory_interface = crate::memory_interface::MemoryInterface::new(&db);
        let recall_receipts =
            crate::recall_receipt::RecallReceipts::connect(&id, &db, attention_directory.clone())
                .await?;
        let action_bindings = match &memory_runtime_bindings {
            Some(cfg) if cfg.inbox.is_some() => Some(Arc::new(
                cfg.inbox
                    .as_ref()
                    .unwrap()
                    .bind(memory.nexus(), attention_directory.clone())?,
            )),
            Some(cfg) if cfg.actions.is_some() => cfg.actions.clone().map(Arc::new),
            _ => action_bindings,
        };
        let attention = crate::attention::AttentionRuntime::new(
            id.clone(),
            db.clone(),
            memory.nexus(),
            attention_directory.clone(),
            attention_policy,
            automatic,
            action_bindings,
            memory_runtime_bindings
                .as_ref()
                .and_then(|c| c.semantic.clone()),
        );
        let recall_store = Conversations::connect(db.clone(), "recall".to_string()).await?;
        if let Some(actions) = attention.actions() {
            actions.bind_receipts(recall_receipts.clone());
        }
        let maintenance_store =
            Conversations::connect(db.clone(), "maintenance".to_string()).await?;
        #[cfg(feature = "wiki")]
        let wiki = Arc::new(WikiService::connect(id.clone(), db.clone()).await?);

        // create a new models instance for each space to allow per-space customization in the future (e.g., different model providers or credentials)
        let model_cancel = CancellationToken::new();
        let models = Arc::new(crate::model_budget::registry(
            &models,
            &llm_semaphore,
            &model_cancel,
        ));
        #[cfg(feature = "wiki")]
        let wiki_digest = Arc::new(WikiDigest::new(
            wiki.clone(),
            memory.clone(),
            models.clone(),
        ));
        #[cfg(feature = "wiki")]
        wiki.set_audit_reads(db.get_extension_as("wiki_audit_reads").unwrap_or(false));
        // Agent wiki tools see only unlabeled content when the space is
        // public: recall there is world-reachable, so its evidence pool must
        // match the anonymous reader's view (PRD §8.2). Evaluated per call —
        // toggling `public` applies immediately.
        #[cfg(feature = "wiki")]
        let wiki_tool_scope: crate::wiki::WikiToolScope = {
            let db = db.clone();
            Arc::new(move || {
                if db.get_extension_as("public").unwrap_or(false) {
                    Some(Vec::new())
                } else {
                    None
                }
            })
        };
        #[cfg(feature = "learning")]
        let learning = crate::learning::LearningRuntime::connect(
            object_store.clone(),
            format!("{id}/learning"),
            memory.nexus(),
            clock.clone(),
        )
        .await?;
        #[cfg(feature = "learning")]
        learning.bind_attention(Arc::downgrade(&attention));
        #[cfg(feature = "learning")]
        let learning_bindings = memory_runtime_bindings
            .as_ref()
            .and_then(|r| r.learning.clone());
        let trust_config = memory_runtime_bindings
            .as_ref()
            .and_then(|r| r.trust.clone());
        let utility_config = memory_runtime_bindings
            .as_ref()
            .and_then(|r| r.utility.clone());
        let memory_runtime = if let Some(bindings) = memory_runtime_bindings {
            Some(
                crate::runtime_api::MemoryRuntime::connect(
                    memory.nexus(),
                    db.clone(),
                    attention_directory,
                    attention.clone(),
                    bindings,
                    automatic,
                    #[cfg(feature = "learning")]
                    Some(Arc::downgrade(&learning)),
                )
                .await?,
            )
        } else {
            None
        };
        #[cfg(feature = "learning")]
        if let Some(bindings) = learning_bindings {
            learning
                .install_bindings(
                    anda_cognitive_nexus::governance::AuthContext::system(),
                    bindings,
                )
                .await?;
        }
        let trust = crate::consequence::trust::TrustRuntime::new(
            memory.nexus(),
            object_store.clone(),
            recall_receipts.clone(),
            trust_config,
            automatic,
        );
        if let Some(runtime) = &memory_runtime {
            runtime.bind_trust(Arc::downgrade(&trust));
        }
        let utility = crate::consequence::utility::UtilityRuntime::new(
            memory.nexus(),
            object_store.clone(),
            recall_receipts.clone(),
            utility_config,
            automatic,
            #[cfg(feature = "learning")]
            Some(Arc::downgrade(&learning)),
        );
        if let Some(runtime) = &memory_runtime {
            runtime.bind_utility(Arc::downgrade(&utility));
            utility.bind_consequences(Arc::downgrade(&runtime.consequences()));
        }
        let product_control = crate::product::control::Control::connect(db.clone()).await?;
        let memory_r = TimedMemoryReadonly::new(memory.clone())
            .with_product_control(product_control.clone())
            .with_clock(clock.clone())
            .with_attention(attention.clone());
        let tasks = crate::runtime::RuntimeTasks::default();
        let memory_tool = MemoryTool::new(memory.clone());
        let note_tool = crate::product::control::ControlledNotes::new(product_control.clone());
        // Formation may draft vocabulary; Maintenance reviews drafts and
        // Recall is read-only, so neither gets the tool.
        let declare_tool = crate::vocabulary::DeclareSymbolsTool::new(memory.clone())
            .with_product_control(product_control.clone());

        let hooks = Arc::new(Hooks::new(db.clone()));
        let processing_gate = Arc::new(crate::agents::ProcessingGate::default());
        let formation = Arc::new(
            FormationAgent::new(memory.clone(), conversations.clone(), hooks.clone(), 100000)
                .with_processing_gate(processing_gate.clone())
                .with_product_control(product_control.clone())
                .with_prompt(prompts.prompt(crate::agents::prompts::PromptTarget::Formation))
                .with_clock(clock.clone())
                .with_tasks(tasks.clone()),
        );
        let recall = Arc::new(
            RecallAgent::new(
                memory.clone(),
                recall_store,
                recall_conversations,
                hooks.clone(),
                65535,
                {
                    let db = db.clone();
                    Arc::new(move || memory_policy_of(&db))
                },
            )
            .with_product_control(product_control.clone())
            .with_prompt(prompts.prompt(crate::agents::prompts::PromptTarget::Recall))
            .with_receipts(recall_receipts.clone())
            .with_utility(Arc::downgrade(&utility))
            .with_clock(clock.clone()),
        );
        let maintenance = Arc::new(
            MaintenanceAgent::new(
                memory.clone(),
                maintenance_store,
                maintenance_conversations,
                hooks.clone(),
            )
            .with_processing_gate(processing_gate)
            .with_product_control(product_control.clone())
            .with_prompt(prompts.prompt(crate::agents::prompts::PromptTarget::Maintenance))
            .with_clock(clock.clone())
            .with_tasks(tasks.clone()),
        );
        // Build agent engine with all configured components
        #[allow(unused_mut)]
        let mut engine = Engine::builder()
            // Notes and other durable agent state belong to this Space too.
            // The engine default is a separate ephemeral store, which neither
            // survives reopen nor participates in an experiment snapshot.
            .with_store(anda_engine::store::Store::new(Arc::new(
                object_store::prefix::PrefixStore::new(
                    object_store.clone(),
                    format!("{id}/engine"),
                ),
            )))
            .with_management(management)
            .with_models(models.clone())
            // `execute_kip`, but Formation only reaches the cognition-only
            // subset through it; maintenance keeps the whole of KML. The raw
            // `memory` handle stays available to host code, which is
            // deterministic and not what the gate is for.
            .register_tool(Arc::new(
                GuardedMemory::new(memory.clone())
                    .with_clock(clock.clone())
                    .with_product_control(product_control.clone()),
            ))?
            .register_tool(Arc::new(memory_r))?
            .register_tool(Arc::new(memory_tool))?
            .register_tool(Arc::new(note_tool))?
            .register_tool(Arc::new(declare_tool))?
            .register_tool(Arc::new(crate::kip_reference::KipReferenceTool))?
            .register_tool(Arc::new(
                crate::cognitive::MemoryRuntimeTool::new(memory.clone(), attention.clone())
                    .with_product_control(product_control.clone()),
            ))?;
        #[allow(unused_mut)]
        let mut exported_tools = vec![MemoryTool::NAME.to_string()];
        #[cfg(feature = "learning")]
        {
            engine = engine.register_tool(Arc::new(
                crate::learning::recall::ProcedureStatusTool(learning.clone()),
            ))?;
        }
        #[cfg(feature = "wiki")]
        {
            engine = engine
                .register_tool(Arc::new(WikiSearchTool::new(
                    wiki.clone(),
                    wiki_tool_scope.clone(),
                )))?
                .register_tool(Arc::new(WikiReadTool::new(
                    wiki.clone(),
                    wiki_tool_scope.clone(),
                )))?
                .register_tool(Arc::new(WikiCommitTool::new(wiki.clone())))?;
            exported_tools.extend([
                WikiSearchTool::NAME.to_string(),
                WikiReadTool::NAME.to_string(),
                WikiCommitTool::NAME.to_string(),
            ]);
        }
        let engine = engine
            .register_agent(formation.clone(), None)?
            .register_agent(recall.clone(), None)?
            .register_agent(maintenance.clone(), None)?
            .export_tools(exported_tools)
            .export_agents(vec![
                RecallAgent::NAME.to_string(),
                FormationAgent::NAME.to_string(),
                MaintenanceAgent::NAME.to_string(),
            ]);

        // Initialize and start the server
        let engine = engine.build(RecallAgent::NAME.to_string()).await?;
        let ledger = Arc::new(UsageLedger::connect(&db).await?);
        let miss_cache = Arc::new(MissCache::connect(&db).await?);
        let this = Arc::new(Self {
            id,
            db: db.clone(),
            http_client,
            models,
            llm_semaphore,
            model_cancel,
            formation,
            recall,
            maintenance,
            ledger,
            recall_receipts,
            memory_interface,
            utility,
            trust,
            miss_cache,
            settlement_lock: tokio::sync::Mutex::new(()),
            self_test_lock: tokio::sync::Mutex::new(()),
            shadow_lock: tokio::sync::Mutex::new(()),
            token_lock: tokio::sync::Mutex::new(()),
            judge_model: std::sync::RwLock::new(None),
            memory,
            conversations,
            #[cfg(feature = "wiki")]
            wiki,
            #[cfg(feature = "wiki")]
            wiki_digest,
            engine,
            pinned,
            automatic,
            clock,
            tasks,
            native_tasks: Default::default(),
            attention,
            memory_runtime,
            product_control,
            recovery_started: AtomicBool::new(false),
            close_state: tokio::sync::Mutex::new(CloseState::default()),
            interrupted_conversations: parking_lot::Mutex::new(BTreeMap::new()),
            #[cfg(feature = "learning")]
            learning,
        });
        this.recover_product_change().await?;
        hooks.bind_space(Arc::downgrade(&this));
        this.trust.bind_space(Arc::downgrade(&this));
        let weak = Arc::downgrade(&this);
        this.tasks.set_cancel_hook(Arc::new(move || {
            if let Some(space) = weak.upgrade() {
                space.capture_interrupted_work();
            }
        }));

        if let Some(cfg) = db.get_extension_as::<ModelConfig>("byok") {
            let cfg: EngineModelConfig = cfg.into();
            if let Ok(model) = cfg.model(this.http_client.clone()) {
                this.models.set_model(crate::model_budget::limit(
                    model,
                    &this.llm_semaphore,
                    &this.model_cancel,
                ));
            } else {
                log::error!(target: "brain", space_id = this.id; "failed to initialize BYOK model from config: {:?}", cfg);
            }
        }

        if autostart {
            this.start_background_recovery();
        } else {
            // No-autostart open (shadow forks): the agents still need their
            // history cursors, but the inherited formation backlog and wiki
            // digest must stay untouched.
            if let Err(err) = this.formation.init().await {
                log::warn!(target: "brain", space_id = this.id; "formation history init failed: {err:?}");
            }
            if let Err(err) = this.maintenance.init().await {
                log::warn!(target: "brain", space_id = this.id; "maintenance history init failed: {err:?}");
            }
            if let Err(err) = this.recall.init().await {
                log::warn!(target: "brain", space_id = this.id; "recall history init failed: {err:?}");
            }
        }

        Ok(this)
    }
}

impl Space {
    /// Cold attention loads initialize history without starting model work. A
    /// later ordinary load must still resume the existing formation/wiki queues.
    pub(super) fn start_background_recovery(self: &Arc<Self>) {
        if !self.automatic
            || self.engine.is_cancelled()
            || self.recovery_started.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let this_clone = self.clone();
        self.tasks.spawn(async move {
            if let Err(err) = this_clone.formation.init().await {
                log::warn!(target: "brain", space_id = this_clone.id; "formation history init failed: {err:?}");
            }
            if let Err(err) = this_clone.maintenance.init().await {
                log::warn!(target: "brain", space_id = this_clone.id; "maintenance history init failed: {err:?}");
            }
            if let Err(err) = this_clone.recall.init().await {
                log::warn!(target: "brain", space_id = this_clone.id; "recall history init failed: {err:?}");
            }
            // Startup repair: reclaim wiki commit-crash leftovers before the
            // space serves queries built on them.
            #[cfg(feature = "wiki")]
            {
                match this_clone.wiki.orphan_sweep(unix_ms()).await {
                    Ok(report) if !report.is_empty() => {
                        log::warn!(target: "brain", space_id = this_clone.id, report:serde = report; "wiki orphan sweep repaired state");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        log::warn!(target: "brain", space_id = this_clone.id; "wiki orphan sweep failed: {err:?}");
                    }
                }
                // Resume any wiki digest backlog left from before the restart.
                this_clone.kick_wiki_digest();
                this_clone.kick_wiki_housekeeping();
            }
            // Resume formation if it was interrupted before. A missing marker
            // means nothing was processed yet, so resume from the beginning.
            let conversation = this_clone.formation.get_processed().unwrap_or_default();
            let _ = this_clone
                .restart_formation(SELF_USER_ID, conversation + 1)
                .await;
        });
    }
}

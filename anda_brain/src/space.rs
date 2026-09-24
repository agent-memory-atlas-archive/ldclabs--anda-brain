use anda_cognitive_nexus::CognitiveNexus;
use anda_core::{
    AgentInput, AgentOutput, BoxError, ContentPart, Message, Principal, Resource, Usage,
};
use anda_db::{
    collection::{Collection, CollectionConfig},
    database::{AndaDB, DBConfig},
    error::DBError,
    index::BTree,
    query::Fv,
    schema::DocumentId,
};
use anda_db_tfs::jieba_tokenizer;
use anda_engine::{
    engine::Engine,
    management::Management,
    memory::{Conversation, ConversationStatus, Conversations, MemoryManagement, MemoryTool},
    model::{Model, ModelConfig as EngineModelConfig, Models, reqwest},
    rfc3339_datetime_now, unix_ms,
};
use anda_kip::{KipError, KipErrorCode, Request, Response, execute_request};
use ic_auth_types::ByteBufB64;
use ic_cose_types::cose::{
    SIGN1_TAG, cwt::cwt_from, ed25519::VerifyingKey, sign1::cose_sign1_from, skip_prefix,
};
#[cfg(feature = "learning")]
use object_store::ObjectStoreExt;
use object_store::{ObjectStore, memory::InMemory};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OnceCell, RwLock},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "wiki")]
use crate::wiki::{
    WikiCommitTool, WikiDigest, WikiDigestReport, WikiReadTool, WikiSearchTool, WikiService,
};
use crate::{
    agents::{
        BrainHook, FormationAgent, GuardedMemory, MaintenanceAgent, READONLY_KIP_TIMEOUT,
        RecallAgent, SELF_USER_ID, TimedMemoryReadonly,
    },
    assess, kip,
    ledger::{MissCache, UsageLedger},
    payload::StringOr,
    settlement,
    types::{
        AddSpaceTokenInput, CWToken, FormationInput, FormationStatus, MaintenanceInput,
        MaintenanceScope, MemoryForgetEntity, MemoryForgetInput, MemoryForgetReport,
        MemoryGraphCounters, MemoryMetrics, MemoryPolicy, MemorySettlementReport, MemoryStatus,
        ModelConfig, ProbeOutput, RecallInput, RecallOutput, SchemaAudit, SelfTestReport,
        ShadowEvalInput, ShadowReport, ShadowSample, SourceReliability, SpaceInfo, SpaceTier,
        SpaceToken, TokenScope, UpdateSpaceInput,
    },
};

/// Default cap on concurrent LLM-billed requests, matching the CLI's
/// `LLM_MAX_CONCURRENCY` flag default. States built without
/// [`AppState::with_llm_concurrency`] (tests, library embedders) get this
/// budget.
const DEFAULT_LLM_MAX_CONCURRENCY: usize = 64;

struct SpaceEntry {
    cell: OnceCell<Arc<Space>>,
    discovered: OnceCell<()>,
    closing: AtomicBool,
    last_access_ms: AtomicU64,
}

impl SpaceEntry {
    fn new() -> Self {
        Self {
            cell: OnceCell::new(),
            discovered: OnceCell::new(),
            closing: AtomicBool::new(false),
            last_access_ms: AtomicU64::new(unix_ms()),
        }
    }

    fn touch(&self) {
        self.last_access_ms.store(unix_ms(), Ordering::Relaxed);
    }

    fn last_access_ms(&self) -> u64 {
        self.last_access_ms.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub struct AppState {
    spaces: Arc<RwLock<BTreeMap<String, Arc<SpaceEntry>>>>,
    object_store: Arc<dyn ObjectStore>,
    db_config: Arc<DBConfig>,
    http_client: reqwest::Client,
    models: Arc<Models>,
    ed25519_pubkeys: Arc<Vec<VerifyingKey>>,
    management: Arc<dyn Management>,
    /// Independent judge model (plan M9), installed on every space this
    /// state loads — service mode included, so shadow-eval verdicts stop
    /// falling back to the evaluated space's own model.
    judge_model: Arc<Option<ModelConfig>>,
    prompts: crate::agents::prompts::AgentPrompts,
    clock: Arc<crate::runtime::BusinessClock>,
    automatic: bool,
    attention_directory: Arc<crate::attention::Directory>,
    attention_policy: crate::attention::AttentionPolicy,
    action_bindings: Option<Arc<crate::action::ActionBindings>>,
    memory_runtime_bindings: Arc<crate::runtime_api::MemoryRuntimeBindings>,
    /// Actual model calls, including background work and compaction.
    llm_semaphore: Arc<tokio::sync::Semaphore>,
    llm_request_semaphore: Arc<tokio::sync::Semaphore>,

    pub app_name: String,
    pub app_version: String,
    pub sharding: u32,
}

mod attention;
mod attention_recall;
#[cfg(feature = "experiments")]
pub mod experiments;
mod hooks;
mod lifecycle;
mod metabolism;
mod processing;
mod tokens;
use hooks::Hooks;
mod runtime_api;
mod self_test;
mod shadow;
pub use processing::{ProcessingKind, ProcessingReport, ProcessingState, ProcessingWait};
#[cfg(test)]
mod close_tests;
#[cfg(test)]
pub(crate) mod tests;

use shadow::copy_space_objects;

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        object_store: Arc<dyn ObjectStore>,
        db_config: Arc<DBConfig>,
        management: Arc<dyn Management>,
        http_client: reqwest::Client,
        models: Arc<Models>,
        ed25519_pubkeys: Arc<Vec<VerifyingKey>>,
        app_name: String,
        app_version: String,
        sharding: u32,
    ) -> Self {
        Self {
            spaces: Arc::new(RwLock::new(BTreeMap::new())),
            attention_directory: crate::attention::Directory::new(object_store.clone(), sharding),
            attention_policy: Default::default(),
            action_bindings: None,
            memory_runtime_bindings: Arc::new(Default::default()),
            object_store,
            db_config,
            management,
            http_client,
            models,
            ed25519_pubkeys,
            judge_model: Arc::new(None),
            prompts: Default::default(),
            clock: Arc::new(crate::runtime::BusinessClock::default()),
            automatic: true,
            llm_semaphore: Arc::new(tokio::sync::Semaphore::new(DEFAULT_LLM_MAX_CONCURRENCY)),
            llm_request_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_LLM_MAX_CONCURRENCY,
            )),
            app_name,
            app_version,
            sharding,
        }
    }

    /// Installs immutable deployment prompts before this host is shared or
    /// opens any Space. Rejects late changes instead of splitting the cache's
    /// prompt identity from the agents it already owns.
    pub fn with_agent_prompts(
        mut self,
        prompts: crate::agents::prompts::AgentPrompts,
    ) -> Result<Self, BoxError> {
        if Arc::strong_count(&self.spaces) != 1
            || !self
                .spaces
                .try_read()
                .map_err(|_| "host configuration is in use")?
                .is_empty()
        {
            return Err("configure prompts before cloning the host or opening a Space".into());
        }
        self.prompts = prompts;
        Ok(self)
    }

    /// Configures the independent judge model this state installs on every
    /// space it loads (consuming builder; call before the state is cloned).
    pub fn with_judge_model(mut self, config: Option<ModelConfig>) -> Self {
        self.judge_model = Arc::new(config);
        self
    }

    /// Sets independent caps on admitted requests and actual model calls (the service's
    /// `LLM_MAX_CONCURRENCY` flag; consuming builder, call before the state
    /// is cloned). `max(1)` keeps a misconfigured `0` from shedding every
    /// request.
    pub fn with_llm_concurrency(mut self, max: usize) -> Self {
        self.llm_semaphore = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self.llm_request_semaphore = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self
    }

    /// Shared budget held by completion adapters until the model call finishes.
    pub fn llm_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.llm_semaphore
    }

    /// Separate admission limit: a request may wait for a model-call permit.
    pub fn llm_request_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.llm_request_semaphore
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn cwt_auth_enabled(&self) -> bool {
        !self.ed25519_pubkeys.is_empty()
    }

    /// The object store backing this state's spaces.
    fn object_store(&self) -> Arc<dyn ObjectStore> {
        self.object_store.clone()
    }

    /// Forks a space into its own in-memory store, optionally installing a
    /// candidate memory policy. Forks are fully isolated: nothing they do
    /// can reach the source space's graph, ledger, or metrics. This is the
    /// only supported way to open a throwaway copy of a space (shadow eval,
    /// experiment forks): it owns the whole protocol — copy the
    /// objects, fork the state, and load without background autostart.
    pub async fn fork_space(
        &self,
        space_id: &str,
        policy: Option<MemoryPolicy>,
    ) -> Result<Arc<Space>, BoxError> {
        #[cfg(feature = "learning")]
        match self
            .object_store
            .head(&object_store::path::Path::from(format!(
                "{space_id}/learning/registration"
            )))
            .await
        {
            Ok(_) => {
                return Err(
                    "a configured learning Space cannot be copied with its dispatch journal".into(),
                );
            }
            Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        copy_space_objects(&self.object_store(), &store, space_id).await?;
        let state = self.fork_with_store(store);
        // autostart=false: the fork inherits the live space's formation
        // cursor and wiki-digest backlog, and must NOT resume them — that
        // would burn real LLM tokens twice and mutate both forks mid-replay,
        // making the A/B comparison non-reproducible.
        let fork = state.load_space_with(space_id, false, false).await?;
        // Check the recovered copy, not a racy source preflight. A retained
        // native instance/fence must never become independently executable.
        if fork
            .memory
            .nexus()
            .system_session()
            .read_control(
                anda_cognitive_nexus::nexus::DEFAULT_SPACE,
                "attention/config",
                None,
            )
            .await?
            .is_some()
        {
            fork.close().await?;
            return Err("a Space with native attention state cannot be forked with its wake identity and leases".into());
        }
        if let Some(policy) = policy {
            fork.db
                .set_extension_from(MemoryPolicy::EXTENSION_KEY.to_string(), policy);
        }
        Ok(fork)
    }

    // 平台管理员权限
    pub fn check_admin(
        &self,
        token: &str,
        audience: &str,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<CWToken, BoxError> {
        if self.ed25519_pubkeys.is_empty() {
            return Ok(CWToken {
                user: Principal::management_canister(),
                audience: audience.to_string(),
                scope,
            });
        }

        let token = self.check_auth(token, audience, scope, now_ms)?;
        if !self.management.is_manager(&token.user) {
            return Err("admin access required".into());
        }

        Ok(token)
    }

    // 用户权限
    pub(crate) fn check_auth_if(
        &self,
        token: &str,
        audience: &str,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<Option<CWToken>, BoxError> {
        if self.ed25519_pubkeys.is_empty() {
            return Ok(Some(CWToken {
                user: SELF_USER_ID,
                audience: audience.to_string(),
                scope,
            }));
        }

        if token.len() < 60 {
            return Ok(None);
        }

        let token = self.check_auth(token, audience, scope, now_ms)?;
        Ok(Some(token))
    }

    pub fn check_auth(
        &self,
        token: &str,
        audience: &str,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<CWToken, BoxError> {
        if self.ed25519_pubkeys.is_empty() {
            return Ok(CWToken {
                user: SELF_USER_ID,
                audience: audience.to_string(),
                scope,
            });
        }

        let data = ByteBufB64::from_str(token)?;
        let data = skip_prefix(&SIGN1_TAG, &data);
        let cs1 = cose_sign1_from(data, &[], &[], &self.ed25519_pubkeys)?;
        let claims = cwt_from(&cs1.payload.unwrap_or_default(), (now_ms / 1000) as i64)?;
        let token = CWToken::from_claims(claims)?;
        if token.audience != audience && token.audience != "*" {
            return Err("invalid audience".into());
        }

        if !token.scope.allows(scope) {
            return Err("insufficient scope".into());
        }
        Ok(token)
    }

    pub async fn admin_create_space(
        &self,
        creator: Principal,
        owner: Principal,
        id: String,
        tier: u32,
        now_ms: u64,
    ) -> Result<SpaceInfo, BoxError> {
        {
            let spaces = self.spaces.read().await;
            if spaces
                .get(&id)
                .is_some_and(|entry| entry.cell.initialized())
            {
                return Err(format!("space {id} already exists").into());
            }
        }

        let mut db_config = (*self.db_config).clone();
        db_config.name = id;
        Space::create(
            self.object_store.clone(),
            db_config,
            creator,
            owner,
            tier,
            now_ms,
        )
        .await
    }

    pub async fn load_space(&self, space_id: &str, pinned: bool) -> Result<Arc<Space>, BoxError> {
        self.load_space_with(space_id, pinned, true).await
    }

    /// `load_space` with control over background autostart. `autostart:
    /// false` opens the space without resuming its formation backlog or wiki
    /// digest — required for shadow forks, which are throwaway copies whose
    /// backlog must not burn LLM tokens or mutate the fork mid-replay.
    ///
    /// `pinned` takes effect on first initialization. A foreground autostart
    /// may subsequently resume queues on a Space first opened by attention;
    /// isolated hosts keep automatic work disabled.
    pub(crate) async fn load_space_with(
        &self,
        space_id: &str,
        pinned: bool,
        autostart: bool,
    ) -> Result<Arc<Space>, BoxError> {
        self.load_space_mode(space_id, pinned, autostart, true)
            .await
    }

    async fn load_space_mode(
        &self,
        space_id: &str,
        pinned: bool,
        autostart: bool,
        touch: bool,
    ) -> Result<Arc<Space>, BoxError> {
        let entry = {
            let spaces = self.spaces.read().await;
            spaces.get(space_id).cloned()
        };

        let entry = match entry {
            Some(entry) => entry,
            None => {
                let mut spaces = self.spaces.write().await;
                spaces
                    .entry(space_id.to_string())
                    .or_insert_with(|| {
                        let entry = Arc::new(SpaceEntry::new());
                        if !touch {
                            entry.last_access_ms.store(0, Ordering::Relaxed);
                        }
                        entry
                    })
                    .clone()
            }
        };

        if entry.closing.load(Ordering::Acquire) {
            return Err("space is closing; retry after eviction completes".into());
        }

        let space = entry
            .cell
            .get_or_try_init(|| async {
                let mut db_config = (*self.db_config).clone();
                db_config.name = space_id.to_string();
                let space = Space::connect(
                    self.object_store.clone(),
                    db_config,
                    self.management.clone(),
                    self.http_client.clone(),
                    self.models.clone(),
                    self.llm_semaphore.clone(),
                    pinned,
                    autostart,
                    self.clock.clone(),
                    self.automatic,
                    self.prompts.clone(),
                    self.attention_directory.clone(),
                    self.attention_policy.clone(),
                    self.action_bindings.clone(),
                    self.memory_runtime_bindings
                        .spaces
                        .get(space_id)
                        .cloned()
                        .map(Arc::new),
                )
                .await?;
                if let Some(judge) = self.judge_model.as_ref()
                    && let Err(err) = space.set_judge_model(judge.clone())
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "installing the independent judge model failed: {err:?}"
                    );
                }
                Ok::<_, BoxError>(space)
            })
            .await
            .cloned()?;

        if touch {
            entry.touch();
        }
        entry
            .discovered
            .get_or_try_init(|| space.attention.discover_existing())
            .await?;
        // A Space idle for nine minutes is evicted, so the background pass
        // above never sees the quietest ones at all — the next time anybody
        // opens this Space is the only moment its overdue cycle can be
        // noticed. `autostart: false` forks are excluded: a shadow copy must
        // not burn model calls or mutate itself mid-replay.
        if autostart {
            space.start_background_recovery();
            space.kick_scheduled_maintenance();
        }
        Ok(space)
    }
}

/// Errors from the settlement attempt that immediately precedes a maintenance
/// cycle. A top-level failure wins over the stored report because that report
/// belongs to an older attempt and must not describe the current cycle.
fn settlement_error_messages(
    stored: Option<&MemorySettlementReport>,
    current_error: Option<&str>,
) -> Vec<String> {
    if let Some(error) = current_error {
        return vec![format!("settlement: {error}")];
    }
    stored
        .map(|report| {
            [
                ("corrections", &report.correction_scan_error),
                ("watches", &report.watches.error),
                ("skills", &report.skills.error),
                ("retention", &report.retention.error),
            ]
            .into_iter()
            .filter_map(|(pass, error)| error.as_ref().map(|error| format!("{pass}: {error}")))
            .collect()
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct CloseState {
    reconciled: bool,
    closed: bool,
    collections: Vec<Arc<Collection>>,
}

pub struct Space {
    id: String,
    pub(crate) engine: Engine,
    http_client: reqwest::Client,
    models: Arc<Models>,
    llm_semaphore: Arc<tokio::sync::Semaphore>,
    model_cancel: CancellationToken,
    maintenance: Arc<MaintenanceAgent>,
    pinned: bool,
    automatic: bool,
    clock: Arc<crate::runtime::BusinessClock>,
    tasks: crate::runtime::RuntimeTasks,
    native_tasks: crate::runtime::DurableTasks,
    attention: Arc<crate::attention::AttentionRuntime>,
    memory_runtime: Option<Arc<crate::runtime_api::MemoryRuntime>>,
    pub(crate) product_control: Arc<crate::product::control::Control>,
    recovery_started: AtomicBool,
    close_state: tokio::sync::Mutex<CloseState>,
    interrupted_conversations: parking_lot::Mutex<BTreeMap<&'static str, BTreeSet<u64>>>,
    #[cfg(feature = "learning")]
    learning: Arc<crate::learning::LearningRuntime>,
    /// Memory usage ledger (plan M1): off-graph recall/correction counters.
    ledger: Arc<UsageLedger>,
    recall_receipts: Arc<crate::recall_receipt::RecallReceipts>,
    utility: Arc<crate::consequence::utility::UtilityRuntime>,
    trust: Arc<crate::consequence::trust::TrustRuntime>,
    /// Negative-knowledge cache (plan M5): probe queries the graph had
    /// nothing for; cleared whenever formation completes.
    pub(crate) miss_cache: Arc<MissCache>,
    /// Serializes memory-metabolism settlements (plan M2); the settlement
    /// itself is idempotent, the lock just avoids wasted duplicate passes.
    settlement_lock: tokio::sync::Mutex<()>,
    /// At most one dream self-test (plan M7) runs at a time; overlapping
    /// kicks are skipped, not queued.
    self_test_lock: tokio::sync::Mutex<()>,
    /// At most one shadow evaluation (plan M11) per space: each run holds
    /// two full in-memory copies, so stacking runs is an OOM vector.
    shadow_lock: tokio::sync::Mutex<()>,
    /// Serializes token minting so the name-uniqueness and count-cap checks
    /// in `add_space_token` cannot race two concurrent mints.
    token_lock: tokio::sync::Mutex<()>,
    /// Independent judge model for eval runs (plan M9); unset means judge
    /// completions share the space's default model (documented caveat).
    judge_model: std::sync::RwLock<Option<Arc<Model>>>,
    pub(crate) formation: Arc<FormationAgent>,
    pub(crate) recall: Arc<RecallAgent>,
    pub(crate) db: Arc<AndaDB>,
    pub(crate) memory: Arc<MemoryManagement>,
    /// The collection backing `memory`'s conversations. `MemoryManagement`
    /// wraps document access only, so document counts and the cursor paging
    /// in `list_conversations` go through this handle.
    pub(crate) conversations: Arc<Collection>,
    #[cfg(feature = "wiki")]
    pub(crate) wiki: Arc<WikiService>,
    #[cfg(feature = "wiki")]
    pub(crate) wiki_digest: Arc<WikiDigest>,
}

impl Space {
    pub fn is_processing(&self) -> bool {
        let memory_busy = self.formation.is_processing() || self.maintenance.is_processing();
        #[cfg(feature = "learning")]
        {
            memory_busy || self.learning.is_busy()
        }
        #[cfg(not(feature = "learning"))]
        {
            memory_busy
        }
    }

    /// Includes independent attention operations/leases for eviction decisions,
    /// without changing Formation/Maintenance's own processing gate.
    pub fn is_busy(&self) -> bool {
        self.is_processing()
            || self.native_tasks.is_busy()
            || self.product_control.tasks.is_busy()
            || self.attention.is_busy()
            || self.recall_receipts.is_busy()
            || self.utility.is_busy()
            || self.trust.is_busy()
            || self.memory_runtime.as_ref().is_some_and(|r| r.is_busy())
    }

    /// Trusted host control only. No model tool, HTTP or MCP route exposes
    /// configuration, executor dispatch or independently authenticated outcomes.
    #[cfg(feature = "learning")]
    pub fn learning(&self) -> Arc<crate::learning::LearningRuntime> {
        self.learning.clone()
    }

    pub async fn update(&self, input: UpdateSpaceInput, now_ms: u64) -> Result<(), BoxError> {
        // Validate up front: a bad policy must reject the request before any
        // other field of this update mutates in-memory extension state.
        if let Some(policy) = &input.memory_policy {
            policy.validate()?;
        }

        let mut changed = false;
        // The ACL-defaults write does real I/O and can fail; run it before
        // any in-memory extension mutation so its failure leaves the update
        // untouched.
        #[cfg(feature = "wiki")]
        if let Some(defaults) = input.wiki_acl_defaults {
            changed = true;
            self.wiki.set_acl_defaults(defaults).await?;
        }

        if let Some(name) = input.name {
            changed = true;
            self.db.set_extension_from("name".to_string(), name);
        }
        if let Some(description) = input.description {
            changed = true;
            self.db
                .set_extension_from("description".to_string(), description);
        }
        if let Some(public) = input.public {
            changed = true;
            self.db.set_extension_from("public".to_string(), public);
        }
        #[cfg(feature = "wiki")]
        if let Some(wiki_digest) = input.wiki_digest {
            changed = true;
            self.db
                .set_extension_from("wiki_digest".to_string(), wiki_digest);
        }
        #[cfg(feature = "wiki")]
        if let Some(audit_reads) = input.wiki_audit_reads {
            changed = true;
            self.db
                .set_extension_from("wiki_audit_reads".to_string(), audit_reads);
            self.wiki.set_audit_reads(audit_reads);
        }
        if let Some(policy) = input.memory_policy {
            changed = true;
            self.db
                .set_extension_from(MemoryPolicy::EXTENSION_KEY.to_string(), policy);
        }

        // Accepted non-atomicity: if this flush fails, the in-memory writes
        // above may already be visible and will be persisted by the periodic
        // flush anyway; `update` is idempotent, so a retry converges.
        if changed {
            self.db.flush_metadata(now_ms).await?;
        }
        Ok(())
    }

    /// The Space's persisted policy, or the compiled defaults when absent.
    fn memory_policy(&self) -> MemoryPolicy {
        memory_policy_of(&self.db)
    }

    pub fn get_byok(&self) -> Option<ModelConfig> {
        self.db.get_extension_as("byok")
    }

    pub async fn update_byok(&self, model_config: ModelConfig) -> Result<(), BoxError> {
        let engine_config: EngineModelConfig = model_config.clone().into();
        let model = engine_config.model(self.http_client.clone())?;
        self.db
            .save_extension_from("byok".to_string(), &model_config.to_ref())
            .await?;
        self.models.set_model(crate::model_budget::limit(
            model,
            &self.llm_semaphore,
            &self.model_cancel,
        ));
        Ok(())
    }

    pub fn is_public(&self) -> bool {
        self.db.get_extension_as("public").unwrap_or(false)
    }

    pub fn get_info(&self) -> SpaceInfo {
        let mut info = SpaceInfo {
            id: self.id.clone(),
            db_stats: self.db.stats(),
            concepts: self.memory.nexus().store.concepts().len(),
            propositions: self.memory.nexus().store.propositions().len(),
            conversations: self.conversations.len(),
            formation_processed_id: self.formation.get_processed().unwrap_or_default(),
            maintenance_processed_id: self.maintenance.get_processed().unwrap_or_default(),
            maintenance_at: self.maintenance.get_processed_at(),
            #[cfg(feature = "wiki")]
            wiki_docs: self.wiki.docs_count(),
            #[cfg(feature = "wiki")]
            wiki_chunks: self.wiki.chunks_count(),
            #[cfg(feature = "wiki")]
            wiki_versions: self.wiki.versions_count(),
            #[cfg(feature = "wiki")]
            wiki_queries: self.wiki.queries_count(),
            #[cfg(feature = "wiki")]
            wiki_digested: self.wiki_digest.cursor(),
            #[cfg(feature = "wiki")]
            wiki_stale_docs: self.wiki.stale_report_cached().stale_docs,
            ..Default::default()
        };

        self.db.extensions_with(|kv| {
            info.name = kv
                .get("name")
                .and_then(|v| String::try_from(v.clone()).ok());
            info.description = kv
                .get("description")
                .and_then(|v| String::try_from(v.clone()).ok());
            info.owner = kv
                .get("owner")
                .and_then(|v| String::try_from(v.clone()).ok())
                .unwrap_or_default();
            info.public = kv
                .get("public")
                .and_then(|v| bool::try_from(v.clone()).ok())
                .unwrap_or(false);
            info.tier = kv
                .get("tier")
                .and_then(|v| v.clone().deserialized::<SpaceTier>().ok())
                .unwrap_or_default();
            info.formation_usage = kv
                .get("formation_usage")
                .and_then(|v| v.clone().deserialized::<Usage>().ok())
                .unwrap_or_default();
            info.recall_usage = kv
                .get("recall_usage")
                .and_then(|v| v.clone().deserialized::<Usage>().ok())
                .unwrap_or_default();
            info.maintenance_usage = kv
                .get("maintenance_usage")
                .and_then(|v| v.clone().deserialized::<Usage>().ok())
                .unwrap_or_default();
        });
        info
    }

    pub fn formation_status(&self) -> FormationStatus {
        FormationStatus {
            id: self.id.clone(),
            concepts: self.memory.nexus().store.concepts().len(),
            propositions: self.memory.nexus().store.propositions().len(),
            conversations: self.conversations.len(),
            formation_processing: self.formation.is_processing(),
            maintenance_processing: self.maintenance.is_processing(),
            formation_processed_id: self.formation.get_processed().unwrap_or_default(),
            maintenance_processed_id: self.maintenance.get_processed().unwrap_or_default(),
            maintenance_at: self.maintenance.get_processed_at(),
        }
    }

    pub async fn ingest(
        &self,
        user: Principal,
        input: StringOr<FormationInput>,
    ) -> Result<AgentOutput, BoxError> {
        self.ingest_with_source(user, input, None).await
    }

    /// Trusted host source identity; derive it from authenticated conversation
    /// state, never from a model's arguments or recalled instructions.
    pub async fn ingest_product(
        &self,
        user: Principal,
        input: FormationInput,
        source: crate::product::SourceIdentity,
    ) -> Result<AgentOutput, BoxError> {
        source.validate()?;
        self.ingest_with_source(user, StringOr::Value(input), Some(source))
            .await
    }

    async fn ingest_with_source(
        &self,
        user: Principal,
        mut input: StringOr<FormationInput>,
        source: Option<crate::product::SourceIdentity>,
    ) -> Result<AgentOutput, BoxError> {
        // The observation time becomes every formed claim's start key, so an
        // unreadable one is refused here instead of silently becoming the
        // receipt time (Spec §6.5, §13.2).
        if let StringOr::Value(input) = &mut input
            && let Some(timestamp) = &input.timestamp
        {
            input.timestamp = Some(crate::kip::source_timestamp(timestamp)?);
        }
        if !self.product_control.available() {
            return Err(crate::product::SourceAdmissionError::Busy.into());
        }
        if source
            .as_ref()
            .is_some_and(|source| !self.product_control.source_allowed(source))
        {
            return Err(crate::product::SourceAdmissionError::Suppressed.into());
        }
        if self.engine.is_cancelled() {
            return Err("space is closed".into());
        }
        // Reject empty input up front (both HTTP and MCP funnel through
        // here): it would otherwise persist a garbage conversation and burn
        // a full LLM encoding cycle on nothing. Checking part counts is not
        // enough — a non-empty content array of blank text parts must also
        // count as empty; non-text parts (files, inline data, tool calls)
        // always count as content.
        let empty = match &input {
            StringOr::String(text) => text.trim().is_empty(),
            StringOr::Value(input) => input.messages.iter().all(|message| {
                message.content.iter().all(|part| match part {
                    ContentPart::Text { text } | ContentPart::Reasoning { text } => {
                        text.trim().is_empty()
                    }
                    _ => false,
                })
            }),
        };
        if empty {
            return Err("formation input must not be empty".into());
        }
        let nodes = self
            .memory
            .nexus()
            .store
            .concepts()
            .len()
            .max(self.conversations.len()) as u64;
        let tier = self.get_tier();
        if tier.allow_nodes() < nodes {
            return Err(format!(
                "node limit exceeded: {} nodes vs tier limit {}",
                nodes,
                tier.allow_nodes()
            )
            .into());
        }

        self.engine
            .agent_run(
                user,
                AgentInput {
                    name: FormationAgent::NAME.to_string(),
                    prompt: input.to_string(),
                    meta: source.map(|source| anda_core::RequestMeta {
                        extra: serde_json::Map::from_iter([(
                            crate::product::control::SOURCE_KEY.into(),
                            serde_json::json!(source),
                        )]),
                        ..Default::default()
                    }),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await
    }

    async fn run_recall(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<(AgentOutput, Option<crate::recall_budget::RecallBudget>), BoxError> {
        if self.engine.is_cancelled() {
            return Err("space is closed".into());
        }
        // Same guard as `probe_memory`: an empty query must not start a
        // billed multi-turn LLM run (covers `query` and `query_structured`
        // on both the HTTP and MCP channels).
        let empty = match &input {
            StringOr::String(text) => text.trim().is_empty(),
            StringOr::Value(input) => input.query.trim().is_empty(),
        };
        if empty {
            return Err("recall query must not be empty".into());
        }
        let output = self
            .engine
            .agent_run(
                user,
                AgentInput {
                    name: RecallAgent::NAME.to_string(),
                    prompt: input.to_string(),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await?;
        let budgeted = serde_json::from_str::<crate::recall_budget::MemoryPacket>(&output.content)
            .is_ok_and(|packet| packet.format == crate::recall_budget::PACKET_FORMAT)
            || (output.content == "null"
                && output
                    .failed_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("recall_")));
        let budget = if budgeted {
            match output.conversation {
                Some(conversation) => self.recall.conversation_budget(conversation).await?,
                None => None,
            }
        } else {
            None
        };
        Ok((output, budget))
    }

    pub async fn query(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<AgentOutput, BoxError> {
        let (mut output, budget) = self.run_recall(user, input).await?;
        if budget.is_some() {
            return Ok(output);
        }
        // The self-report footer (plan M4) is machine metadata; plain-text
        // callers must never see it. `query_structured` surfaces it instead.
        let (answer, meta) = assess::split_recall_meta(&output.content);
        output.content = answer;
        // The final assistant message inside `chat_history` carries the raw
        // model output — strip the footer there too, or clients reading the
        // history see the markup that `content` hides. Same for the failure
        // diagnostic, which embeds the rendered conversation.
        strip_recall_meta_from_history(&mut output.chat_history);
        if let Some(reason) = output.failed_reason.take() {
            output.failed_reason = Some(assess::split_recall_meta(&reason).0);
        }
        // Plain recalls feed the calibration counters (plan M12) exactly
        // like structured ones — else most production traffic is a blind
        // spot — but only successful runs: a failed recall's self-report is
        // not a calibration sample.
        if output.failed_reason.is_none()
            && let Some(uncertainty) = meta.and_then(|meta| meta.uncertainty)
        {
            self.bump_metrics(|metrics| {
                metrics.uncertainty_reports += 1;
                metrics.uncertainty_sum += uncertainty;
            });
        }
        Ok(output)
    }

    /// Recall with machine-readable provenance (plan M4): the answer plus
    /// trace-derived memory citations and the model's self-reported
    /// `found`/`uncertainty`, so a business agent can decide whether to
    /// assert, hedge, or ask.
    pub async fn query_structured(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<RecallOutput, BoxError> {
        let (output, budget) = self.run_recall(user, input).await?;
        let recall_receipt = match output.conversation {
            Some(id) => self.recall_receipts.for_conversation(id).await?,
            None => None,
        };
        if let Some(budget) = budget {
            let packet =
                serde_json::from_str::<crate::recall_budget::MemoryPacket>(&output.content).ok();
            return Ok(RecallOutput {
                recall_receipt,
                found: packet.as_ref().is_some_and(|p| {
                    p.items
                        .iter()
                        .any(|i| i.channel != crate::recall_budget::Channel::Primer)
                }),
                memory_budget: Some(crate::types::RecallBudgetReceipt {
                    tokenizer: budget.tokenizer,
                    token_limit: budget.max_tokens,
                    tokens: crate::recall_budget::count(&output.content)?,
                    context_token_limit: budget.context_tokens,
                }),
                answer: output.content,
                usage: output.usage,
                conversation: output.conversation,
                failed_reason: output.failed_reason,
                ..Default::default()
            });
        }
        let (answer, meta) = assess::split_recall_meta(&output.content);
        let memories = match output.conversation {
            Some(id) => match self.recall.conversations.get_conversation(id).await {
                Ok(conversation) => {
                    let messages: Vec<Message> = conversation
                        .messages
                        .into_iter()
                        .filter_map(|message| serde_json::from_value::<Message>(message).ok())
                        .collect();
                    assess::extract_memory_citations(&assess::RecallTrace::from_messages(&messages))
                }
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        };
        let meta = meta.unwrap_or_default();
        if output.failed_reason.is_none()
            && let Some(uncertainty) = meta.uncertainty
        {
            // Calibration raw material (plan M12): predicted uncertainty is
            // later audited against actual correction rates. Failed recalls
            // are excluded — their self-report is not a calibration sample.
            self.bump_metrics(|metrics| {
                metrics.uncertainty_reports += 1;
                metrics.uncertainty_sum += uncertainty;
            });
        }
        Ok(RecallOutput {
            recall_receipt,
            answer,
            // The trace is the ground truth when the model does not report.
            found: meta.found.unwrap_or(!memories.is_empty()),
            uncertainty: meta.uncertainty,
            memories,
            conversation: output.conversation,
            usage: output.usage,
            failed_reason: output
                .failed_reason
                .map(|reason| assess::split_recall_meta(&reason).0),
            memory_budget: None,
        })
    }

    /// The user queries of the most recent completed recall conversations,
    /// newest first — the shadow evaluation's replay corpus (plan M11).
    async fn recent_recall_queries(&self, limit: usize) -> Result<Vec<String>, BoxError> {
        let (conversations, _) = self
            .recall
            .conversations
            .list_conversations_by_user(&SELF_USER_ID, None, Some(limit.saturating_mul(2)))
            .await?;
        let mut queries = Vec::new();
        for conversation in conversations {
            if conversation.status != ConversationStatus::Completed {
                continue;
            }
            let Some(first) = conversation.messages.first() else {
                continue;
            };
            let text: String = first
                .get("content")
                .and_then(serde_json::Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }
            // The stored prompt is either a serialized `RecallInput` or the
            // raw query string.
            let query = serde_json::from_str::<RecallInput>(&text)
                .map(|input| input.query)
                .unwrap_or(text);
            queries.push(query);
            if queries.len() >= limit {
                break;
            }
        }
        Ok(queries)
    }

    /// Records which graph entities a completed recall surfaced
    /// (plan M1). Called from the conversation-end hook, off the hot path.
    async fn record_recall_usage(&self, messages: &[serde_json::Value]) -> Result<u64, BoxError> {
        let messages: Vec<Message> = messages
            .iter()
            .filter_map(|message| serde_json::from_value::<Message>(message.clone()).ok())
            .collect();
        let entities = assess::RecallTrace::from_messages(&messages).entity_ids();
        let touched = if entities.is_empty() {
            0
        } else {
            self.ledger.record_recall(&entities, unix_ms()).await?
        };
        self.bump_metrics(|metrics| {
            metrics.recalls_completed += 1;
            metrics.entities_recalled += touched;
        });
        Ok(touched)
    }

    pub async fn maintenance(
        &self,
        user: Principal,
        input: MaintenanceInput,
    ) -> Result<AgentOutput, BoxError> {
        if self.engine.is_cancelled() {
            return Err("space is closed".into());
        }
        // Caller-supplied parameters feed the KIP-writing maintenance prompt;
        // enforce the same bounds as `MemoryPolicy::validate` before anything
        // runs (both HTTP and MCP channels funnel through here).
        if let Some(parameters) = &input.parameters {
            parameters.validate()?;
        }
        // Hold the maintenance slot across the whole entry path: the
        // settlement below performs seconds of KIP writes, and without the
        // claim a formation cycle could start inside that window and write
        // the graph concurrently with the upcoming maintenance cycle. The
        // claim is transferred to `MaintenanceAgent::run_claimed`.
        let claim = self
            .maintenance
            .try_claim_processing()
            .ok_or("Formation or Maintenance is already in progress.")?;
        self.maintenance_claimed(user, input, claim).await
    }

    async fn maintenance_claimed(
        &self,
        user: Principal,
        mut input: MaintenanceInput,
        claim: crate::agents::MaintenanceClaim,
    ) -> Result<AgentOutput, BoxError> {
        input.formation_id = self.formation.get_processed().unwrap_or_default();
        // Callers that pass explicit parameters keep them; everyone else runs
        // under the space's memory policy. Default policy values equal the
        // defaults documented in BrainMaintenance.md, so an unset policy is
        // not a behavior change (plan module M-P).
        let mut effective_policy = self.memory_policy();
        if let Some(parameters) = &input.parameters {
            if let Some(value) = parameters.stale_event_threshold_days {
                effective_policy.stale_event_threshold_days = value;
            }
            if let Some(value) = parameters.unconsolidated_max_backlog {
                effective_policy.unconsolidated_max_backlog = value;
            }
            if let Some(value) = parameters.orphan_max_count {
                effective_policy.orphan_max_count = value;
            }
        }
        input.parameters = Some(effective_policy.maintenance_parameters());
        // Deterministic metabolism settles before the LLM cycle starts, so
        // the agent assesses an already-settled graph. Settlement failures
        // degrade the cycle, never abort it.
        let settlement_error = match self
            .settle_memory_metabolism(input.scope, self.clock.now_ms())
            .await
        {
            Ok(report) => {
                log::info!(
                    target: "brain",
                    space_id = self.id,
                    report:serde = report;
                    "memory metabolism settled"
                );
                None
            }
            Err(err) => {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "memory metabolism settlement failed: {err:?}"
                );
                Some(err.to_string())
            }
        };
        // What the settlement just measured, handed to the cycle's assessment
        // phase. Overwritten rather than merged: like `formation_id`, this is
        // the runtime's account of its own graph, and a request body must not
        // be able to tell the Brain what its vocabulary looks like.
        input.assessment = Some(
            self.maintenance_assessment(settlement_error.as_deref())
                .await,
        );
        let ctx = self.engine.ctx_with(
            user,
            MaintenanceAgent::NAME,
            MaintenanceAgent::NAME,
            Default::default(),
        )?;
        self.maintenance.run_claimed(ctx, input, claim).await
    }

    /// Installs an independent judge model for eval runs (plan M9): judge
    /// completions stop sharing the evaluated system's model and blind spots.
    pub fn set_judge_model(&self, config: ModelConfig) -> Result<(), BoxError> {
        if config.disabled {
            return Err("judge model is disabled".into());
        }
        let engine_config: EngineModelConfig = config.into();
        let model = engine_config.model(self.http_client.clone())?;
        *self.judge_model.write().expect("judge model lock poisoned") = Some(Arc::new(
            crate::model_budget::limit(model, &self.llm_semaphore, &self.model_cancel),
        ));
        Ok(())
    }

    /// The installed judge model, when one exists.
    pub(crate) fn judge_model(&self) -> Option<Arc<Model>> {
        self.judge_model
            .read()
            .expect("judge model lock poisoned")
            .clone()
    }

    /// Test-only judge model injection without a provider config.
    #[cfg(test)]
    pub(crate) fn set_judge_model_for_test(&self, model: Model) {
        *self.judge_model.write().expect("judge model lock poisoned") = Some(Arc::new(
            crate::model_budget::limit(model, &self.llm_semaphore, &self.model_cancel),
        ));
    }

    /// Metamemory probe (plan M5): a cheap, LLM-free existence check.
    /// Answers "does the brain know anything about this?" so callers can
    /// decide whether a full recall is worth its latency and tokens. Empty
    /// results are remembered in the negative-knowledge cache until new
    /// memory forms.
    pub async fn probe_memory(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<ProbeOutput, BoxError> {
        let query = query.trim();
        if query.is_empty() {
            return Err("probe query must not be empty".into());
        }
        let now_ms = unix_ms();
        if self.miss_cache.is_fresh_miss(query, now_ms).await? {
            self.bump_metrics(|metrics| metrics.negative_cache_hits += 1);
            return Ok(ProbeOutput {
                found: false,
                negative_cached: true,
                search_exhaustive: Some(true),
                hits: Vec::new(),
            });
        }

        let limit = limit.unwrap_or(8).clamp(1, 50);
        // MODE omitted: the engine picks hybrid when it has semantic
        // capability, keyword otherwise.
        let response = self
            .execute_kip_readonly(kip::request_with(
                format!("SEARCH CONCEPT :query LIMIT {limit}"),
                kip::param("query", query),
            ))
            .await?;
        let observation = assess::search_observation(&response)?;
        let exhaustive = observation.exhaustive == Some(true);
        let mut hits = assess::citations_from_json(assess::single_read_result(&response)?);
        // The engine's keyword fallback has no relevance threshold, so any
        // token can match the brain's own bookkeeping. A SleepTask is work the
        // maintenance cycle owes itself and a SelfModel is the brain's picture
        // of itself; neither is a memory the caller asked about, and counting
        // one as `found` would tell them to pay for a recall with nothing to
        // say. (`$ConceptType` / `$PropositionType` / `Domain` are gone: KIP
        // 2.0 keeps schema in Packages, so the meta-graph cannot be searched
        // into a result at all.)
        hits.retain(|hit| {
            !matches!(hit.r#type.as_deref(), Some("SleepTask") | Some("SelfModel"))
                && !hit.name.as_deref().unwrap_or_default().starts_with('$')
        });
        // A miss is cached only when the engine says the search saw everything
        // it could have. A non-exhaustive page means the window filled with
        // candidates this Space may not see, not that the Space holds nothing —
        // and remembering "nothing here" for the cache's whole window would
        // turn one truncated search into a stretch of confident wrong answers.
        if hits.is_empty() && exhaustive {
            self.miss_cache.record_miss(query, now_ms).await?;
        }
        self.bump_metrics(|metrics| {
            if hits.is_empty() {
                metrics.probe_misses += 1;
            } else {
                metrics.probe_hits += 1;
            }
        });
        Ok(ProbeOutput {
            found: !hits.is_empty(),
            negative_cached: false,
            search_exhaustive: observation.exhaustive,
            hits,
        })
    }

    /// Pins (or unpins) a memory (plan M6). Returns the number of updated
    /// elements (0 when the id does not exist).
    ///
    /// A pinned memory is exempt from disuse metabolism. KIP 1.x recorded that
    /// as `metadata.pinned`; 2.0 has no generic metadata bag, and "keep this
    /// out of the forgetting ladder" is a storage-lifecycle statement — so it
    /// is a retention class, which every element kind carries and which the
    /// decay sweep already reads.
    pub async fn pin_memory(&self, entity: &str, pinned: bool) -> Result<u64, BoxError> {
        let entity = entity.trim();
        if !assess::is_entity_id(entity) {
            return Err(format!("`{entity}` is not an element id (C-*, P-* or A-*)").into());
        }
        let class = if pinned {
            settlement::PINNED_RETENTION_CLASS
        } else {
            settlement::STANDARD_RETENTION_CLASS
        };
        let response = self
            .run_kip_settlement(kip::request_with(
                "SET RETENTION :id { retention_class: :class }",
                serde_json::Map::from_iter([
                    ("id".to_string(), serde_json::Value::from(entity)),
                    ("class".to_string(), serde_json::Value::from(class)),
                ]),
            ))
            .await?;
        if !kip::succeeded(&response) {
            return Err(format!("pin failed: {}", kip::error_message(&response)).into());
        }
        Ok(kip::changed(&response, "retention"))
    }

    /// Privacy-grade deletion (plan M6): purges content and graph links,
    /// cascades through attached propositions, and removes usage-ledger rows.
    /// KIP retains an erased identity stub. Archive does not satisfy forget.
    /// Run with `dry_run` first; per-entity errors, such as a protected system
    /// element, do not abort the batch.
    pub async fn forget_memory(
        &self,
        input: MemoryForgetInput,
    ) -> Result<MemoryForgetReport, BoxError> {
        // A running maintenance cycle already holds graph content in its LLM
        // context and re-UPSERTs concepts while consolidating — it could
        // silently re-materialize what we are about to physically delete.
        // (A formation backlog can still re-form a fact from *queued
        // conversations*; that is new information arriving, and conversation
        // scrubbing is tracked separately in the plan's deferred list.)
        if !input.dry_run && self.maintenance.is_processing() {
            return Err(
                "a maintenance cycle is running and could re-materialize deleted memories; \
                 retry forget when maintenance is idle"
                    .into(),
            );
        }
        // Each entity costs an existence check plus a cascade enumeration;
        // an unbounded batch would hold the graph busy for minutes.
        if input.entities.len() > 100 {
            return Err("too many entities in one forget request (max 100)".into());
        }
        let _product_guard = if input.dry_run {
            None
        } else {
            let guard = self.product_control.gate.lock().await;
            if !self.product_control.available() {
                return Err("memory_change_pending".into());
            }
            Some(guard)
        };
        let mut report = MemoryForgetReport {
            dry_run: input.dry_run,
            ..Default::default()
        };
        let mut seen = BTreeSet::new();
        for entity in &input.entities {
            let entity = entity.trim().to_string();
            if !seen.insert(entity.clone()) {
                continue;
            }
            let mut entry = MemoryForgetEntity {
                entity: entity.clone(),
                ..Default::default()
            };
            let exists_command = match entity.parse::<anda_cognitive_nexus::ElementId>() {
                Ok(id) => match id.kind {
                    anda_kip::ElementKind::Concept => {
                        "FIND(?e) WHERE { ?e CONCEPT {id: :id} } LIMIT 1"
                    }
                    anda_kip::ElementKind::Proposition => {
                        "FIND(?e) WHERE { ?e PROPOSITION(id: :id) } LIMIT 1"
                    }
                    anda_kip::ElementKind::Assertion => {
                        "FIND(?e) WHERE { ?e ASSERTION {id: :id} } LIMIT 1"
                    }
                    anda_kip::ElementKind::Evidence => {
                        "FIND(?e) WHERE { ?e EVIDENCE {id: :id} } LIMIT 1"
                    }
                    anda_kip::ElementKind::Activity => {
                        "FIND(?e) WHERE { ?e ACTIVITY {id: :id} } LIMIT 1"
                    }
                },
                Err(error) => {
                    entry.error = Some(error.to_string());
                    report.entities.push(entry);
                    continue;
                }
            };

            let response = self
                .execute_kip_readonly(kip::request_with(
                    exists_command,
                    kip::param("id", entity.as_str()),
                ))
                .await?;
            match kip::ok_result(&response) {
                Some(result) => {
                    // Provenance records are not Recall citations, but are
                    // still explicitly erasable graph elements.
                    entry.existed = result.as_array().is_some_and(|rows| {
                        rows.iter()
                            .any(|row| row["id"] == entity && row["_system"]["state"] != "erased")
                    });
                }
                None => {
                    // An errored/timed-out existence check means *unknown*,
                    // never "absent": this is a privacy-grade deletion, and a
                    // clean `existed: false` here would tell the caller the
                    // data is gone while it may still be in the graph.
                    entry.error = Some(format!(
                        "existence check failed, retry: {}",
                        kip::error_message(&response)
                    ));
                    report.entities.push(entry);
                    continue;
                }
            }

            if input.dry_run || !entry.existed {
                report.entities.push(entry);
                continue;
            }

            // Purging a concept cascades to its propositions; their ledger
            // rows must be removed too. Enumerate ids before PURGE removes
            // their graph links.
            let mut cascade: Vec<String> = vec![entity.clone()];
            if assess::is_concept_entity_id(&entity) {
                cascade.extend(self.concept_proposition_ids(&entity).await);
            }

            // `PURGE` is the only removal that satisfies a forget request:
            // archive keeps the content recallable and tombstone keeps it
            // stored. `authorized_cascade` is what makes purging a Concept
            // reach the Propositions that quote it — the 1.x `DETACH` — and
            // the engine still leaves an identity stub so dangling references
            // resolve to "erased" rather than to nothing.
            match self
                .run_kip_settlement(kip::request_with(
                    "PURGE :id REFERENCE POLICY \"authorized_cascade\" CONFIRM \"PURGE\"",
                    kip::param("id", entity.as_str()),
                ))
                .await
            {
                Ok(response) if kip::succeeded(&response) => {
                    report.deleted_concepts += purged_of_kind(&response, "concept");
                    report.deleted_propositions += purged_of_kind(&response, "proposition");
                    report.deleted_assertions += purged_of_kind(&response, "assertion");
                    report.deleted_evidence += purged_of_kind(&response, "evidence");
                    report.deleted_activities += purged_of_kind(&response, "activity");
                    let erased = kip::ok_result(&response)
                        .and_then(|result| result.get("changes"))
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|change| change["op"] == "purge")
                        .filter_map(|change| change["id"].as_str().map(str::to_string))
                        .collect();
                    if let Err(err) = self.clear_product_preview_content(&erased, None).await {
                        entry.error =
                            Some(format!("graph purged, saved preview cleanup failed: {err}"));
                    }
                    for gone in &cascade {
                        let _ = self.ledger.forget_entity(gone).await;
                    }
                }
                Ok(response) => {
                    entry.error = Some(kip::error_message(&response));
                }
                Err(err) => {
                    entry.error = Some(err.to_string());
                }
            }
            report.entities.push(entry);
        }
        let removed = report.deleted_concepts
            + report.deleted_propositions
            + report.deleted_assertions
            + report.deleted_evidence
            + report.deleted_activities;
        if removed > 0 {
            self.bump_metrics(|metrics| metrics.forgotten_entities += removed);
            // Plan M6 cascade: cached probe-miss rows carry raw query text
            // that may reference the forgotten content; dropping the whole
            // (bounded) cache is the conservative fulfillment.
            if let Err(err) = self.miss_cache.clear().await {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "negative-knowledge cache clear after forget failed: {err:?}"
                );
            }
        }
        Ok(report)
    }

    /// Ids of every proposition attached to a concept (either slot); used by
    /// forget to cascade ledger rows for purged propositions. Best-effort:
    /// an enumeration failure only leaves ledger rows behind, never blocks
    /// the deletion itself.
    async fn concept_proposition_ids(&self, concept_id: &str) -> Vec<String> {
        let mut ids = BTreeSet::new();
        for base_command in [
            "FIND(?link) WHERE { ?c CONCEPT {id: :id} ?link (?c, ?p, ?o) } LIMIT 1000",
            "FIND(?link) WHERE { ?c CONCEPT {id: :id} ?link (?s, ?p, ?c) } LIMIT 1000",
        ] {
            // Paginate with CURSOR: a concept with more than one page of
            // propositions must still cascade all of its ledger rows.
            let mut cursor: Option<String> = None;
            loop {
                let mut parameters = kip::param("id", concept_id);
                let command = match &cursor {
                    Some(token) => {
                        parameters.insert("cursor".to_string(), token.clone().into());
                        format!("{base_command} CURSOR :cursor")
                    }
                    None => base_command.to_string(),
                };
                let response = self
                    .execute_kip_readonly(kip::request_with(command, parameters))
                    .await;
                match response {
                    Ok(response) if kip::succeeded(&response) => {
                        if let Some(result) = kip::ok_result(&response) {
                            assess::collect_entity_objects(result, &mut |id, _| {
                                if assess::is_proposition_entity_id(id) {
                                    ids.insert(id.to_string());
                                }
                            });
                        }
                        // A single-operation response reports its cursor at the
                        // operation level; the request level carries it only
                        // when the whole envelope paged.
                        let next = response
                            .results
                            .first()
                            .and_then(|result| result.next_cursor.clone())
                            .or_else(|| response.next_cursor.clone());
                        match next {
                            Some(next) => cursor = Some(next),
                            None => break,
                        }
                    }
                    other => {
                        log::warn!(
                            target: "brain",
                            space_id = self.id;
                            "enumerating propositions of {concept_id} for forget cascade failed: {other:?}"
                        );
                        break;
                    }
                }
            }
        }
        ids.into_iter().collect()
    }

    /// Whether this Space is overdue a maintenance cycle on the clock.
    ///
    /// Maintenance was reachable only by counting formation conversations
    /// (daydream every 21, quick every 42, full every 168) or by an explicit
    /// request. A Space that stops ingesting therefore stopped metabolizing
    /// altogether: no Commitment review, no retention expiry, no self-test —
    /// and the reference policy's triggers are "scheduled, threshold, or
    /// change-driven", not threshold alone. Waiting is supposed to be active.
    ///
    /// A Space that has never formed anything is never overdue: there is
    /// nothing to metabolize, and firing a cycle at every freshly created
    /// Space would spend a model call to discover that.
    fn maintenance_overdue(&self, now_ms: u64) -> bool {
        let last = self.maintenance.get_processed_at().start_at;
        if last == 0 {
            // Never maintained. Due only once something has been formed —
            // `get_processed` is the formation watermark, not a count of
            // requests, so this is "memory exists" rather than "traffic
            // happened".
            return self.formation.get_processed().unwrap_or(0) > 0;
        }
        now_ms.saturating_sub(last) >= MAINTENANCE_MAX_INTERVAL_MS
    }

    /// Runs a scheduled maintenance cycle when the clock says one is due.
    ///
    /// `Full` rather than a cheaper scope on purpose: the passes a quiet Space
    /// is missing — retention expiry, the schema census — are the full-only
    /// ones, so a time-driven cycle that ran `quick` would fire on schedule
    /// and still not do the work the schedule exists for.
    ///
    /// Everything that could go wrong here is already guarded: `maintenance`
    /// claims the single-flight slot and refuses if formation or another cycle
    /// holds it, so a busy Space simply waits for the next tick.
    fn kick_scheduled_maintenance(self: &Arc<Self>) {
        if !self.automatic
            || self.engine.is_cancelled()
            || self.is_processing()
            || !self.maintenance_overdue(self.clock.now_ms())
        {
            return;
        }
        let space = self.clone();
        self.tasks.spawn(async move {
            let input = MaintenanceInput {
                trigger: "scheduled".to_string(),
                scope: MaintenanceScope::Full,
                timestamp: Some(rfc3339_datetime_now()),
                parameters: None,
                formation_id: 0,
                assessment: None,
            };
            match space.maintenance(SELF_USER_ID, input).await {
                Ok(output) => log::info!(
                    target: "brain",
                    space_id = space.id,
                    conversation = output.conversation;
                    "scheduled maintenance started on the clock"
                ),
                // "already in progress" is the ordinary answer on a busy
                // Space, not a fault: the slot is held and the next tick
                // will find the cycle already done.
                Err(err) => log::debug!(
                    target: "brain",
                    space_id = space.id;
                    "scheduled maintenance did not start: {err}"
                ),
            }
        });
    }

    /// WikiDigest is off by default (PRD §13): extraction writes to the
    /// graph, so spaces opt in explicitly via `update_space { wiki_digest }`.
    #[cfg(feature = "wiki")]
    fn wiki_digest_enabled(&self) -> bool {
        self.db.get_extension_as("wiki_digest").unwrap_or(false)
    }

    /// Runs the wiki digest synchronously: distills pending wiki versions
    /// into the Cognitive Nexus with citation provenance, supersedes stale
    /// facts, and re-verifies a citation sample.
    #[cfg(feature = "wiki")]
    pub async fn run_wiki_digest(&self, user: Principal) -> Result<WikiDigestReport, BoxError> {
        if !self.wiki_digest_enabled() {
            return Err(
                "wiki digest is disabled for this space; enable it via update_space { \"wiki_digest\": true }"
                    .into(),
            );
        }
        // Digest is not a registered engine agent; it borrows the formation
        // agent's context (same write-path trust) with its own label.
        let ctx = self.engine.ctx_with(
            user,
            FormationAgent::NAME,
            "wiki_digest",
            Default::default(),
        )?;
        let rt = self.wiki_digest.run_pending(ctx, unix_ms()).await?;
        let _ = self
            .db
            .set_extension_from_with("wiki_digest_usage".to_string(), |v| {
                let mut usage: Usage = v.unwrap_or_default();
                usage.accumulate(&rt.usage);
                Some(usage)
            });
        // The digest writes graph memory outside the formation hook, so it
        // must invalidate the negative-knowledge cache itself (plan M5): a
        // probe miss cached before this digest could now be answerable.
        if rt.digested > 0 || rt.superseded > 0 {
            if let Err(err) = self.miss_cache.clear().await {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "negative-knowledge cache clear after wiki digest failed: {err:?}"
                );
            }
            // The digest is what mints the `$self` Person, so this is the
            // first moment a fresh Space has one to designate. Doing it only
            // at open would leave the primer contradicting the prompts for the
            // whole run that created it.
            designate_self_concept(self.memory.nexus().as_ref()).await;
        }
        Ok(rt)
    }

    /// Non-LLM wiki housekeeping (PRD §7.4): orphan sweep (Quick tier — not
    /// only at startup, so long-running spaces reclaim commit-crash leftovers
    /// too), audit-log retention pruning, the stale-document report, and the
    /// citation sample check (independent of the digest switch). Cheap enough
    /// to run alongside every digest kick.
    #[cfg(feature = "wiki")]
    fn kick_wiki_housekeeping(self: &Arc<Self>) {
        let space = self.clone();
        self.tasks.spawn(async move {
            let now_ms = unix_ms();
            match space.wiki.orphan_sweep(now_ms).await {
                Ok(report) if !report.is_empty() => {
                    log::warn!(target: "brain", space_id = space.id, report:serde = report; "wiki orphan sweep repaired state");
                }
                Ok(_) => {}
                Err(err) => {
                    log::warn!(target: "brain", space_id = space.id; "wiki orphan sweep failed: {err:?}");
                }
            }
            if let Err(err) = space
                .wiki
                .prune_events(crate::wiki::DEFAULT_EVENT_RETENTION, now_ms)
                .await
            {
                log::warn!(target: "brain", space_id = space.id; "wiki event prune failed: {err:?}");
            }
            if let Err(err) = space
                .wiki
                .stale_report(now_ms, crate::wiki::DEFAULT_STALE_AFTER_MS)
                .await
            {
                log::warn!(target: "brain", space_id = space.id; "wiki stale report failed: {err:?}");
            }
            match space.wiki_digest.verify_recent(now_ms).await {
                Ok((_, invalid)) if invalid > 0 => {
                    log::error!(target: "brain", space_id = space.id, invalid = invalid; "wiki citation sample found invalid citations (storage corruption signal)");
                }
                Ok(_) => {}
                Err(err) => {
                    log::warn!(target: "brain", space_id = space.id; "wiki citation sample failed: {err:?}");
                }
            }
        });
    }

    /// Fire-and-forget digest kick used by startup and post-maintenance
    /// hooks; a no-op when disabled or already running.
    #[cfg(feature = "wiki")]
    fn kick_wiki_digest(self: &Arc<Self>) {
        if !self.wiki_digest_enabled() || self.wiki_digest.is_processing() {
            return;
        }
        let space = self.clone();
        self.tasks.spawn(async move {
            match space.run_wiki_digest(SELF_USER_ID).await {
                Ok(report) if report.failed > 0 => {
                    log::warn!(target: "brain", space_id = space.id, report:serde = report; "wiki digest has failed documents awaiting retry");
                }
                Ok(report) if report.digested > 0 || report.superseded > 0 => {
                    log::info!(target: "brain", space_id = space.id, report:serde = report; "wiki digest completed");
                }
                Ok(_) => {}
                Err(err) => {
                    log::warn!(target: "brain", space_id = space.id; "wiki digest failed: {err:?}");
                }
            }
        });
    }

    pub async fn restart_formation(
        &self,
        user: Principal,
        conversation: u64,
    ) -> Result<(), BoxError> {
        if self.engine.is_cancelled() {
            return Err("space is closed".into());
        }
        let ctx = self.engine.ctx_with(
            user,
            "formation_memory",
            "formation_memory",
            Default::default(),
        )?;
        self.formation.start_process(ctx, conversation).await
    }

    /// Executes a KIP request on the read-only path.
    ///
    /// The read-only gate is on what each operation *parses to*, never on a
    /// declared `language`, so a mutation cannot reach the engine through here
    /// however the envelope labels it.
    pub async fn execute_kip_readonly(&self, mut req: Request) -> Result<Response, BoxError> {
        if self.engine.is_cancelled() {
            return Err("space is closed".into());
        }
        self.clock.bind_read(&mut req)?;
        let nexus = self.memory.nexus();
        match timeout(
            READONLY_KIP_TIMEOUT,
            kip::execute_readonly_request(nexus.as_ref(), &req),
        )
        .await
        {
            Ok(res) => {
                self.attention.notice_read(&req, &res);
                Ok(res)
            }
            Err(_) => Ok(Response::failed(KipError::new(
                KipErrorCode::ExecutionTimeout,
                format!(
                    "read-only KIP execution timed out after {} seconds; memory is busy, retry later",
                    READONLY_KIP_TIMEOUT.as_secs()
                ),
            ))),
        }
    }

    pub async fn get_conversation(
        &self,
        collection: Option<String>,
        id: u64,
    ) -> Result<Conversation, BoxError> {
        let rt = match collection.as_deref() {
            Some("recall") => self.recall.conversations.get_conversation(id).await?,
            Some("maintenance") => self.maintenance.conversations.get_conversation(id).await?,
            // "formation" is the documented name of the default collection
            // (API.md, MCP tool schemas): the canonical spelling must stay
            // valid even though omitting it means the same thing.
            None | Some("formation") => self.memory.get_conversation(id).await?,
            // A typo (e.g. "Recall") must not silently read the formation
            // collection — the ids overlap, so it would return an unrelated
            // conversation instead of an error.
            Some(other) => {
                return Err(format!(
                    "unknown conversation collection {other:?} (expected \"formation\", \"recall\" or \"maintenance\")"
                )
                .into());
            }
        };

        Ok(rt)
    }

    pub async fn list_conversations(
        &self,
        collection: Option<String>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> Result<(Vec<Conversation>, Option<String>), BoxError> {
        use anda_db::query::{Filter, RangeQuery};

        let collection = match collection.as_deref() {
            Some("recall") => self.recall.conversations_collection.clone(),
            Some("maintenance") => self.maintenance.conversations_collection.clone(),
            None | Some("formation") => self.conversations.clone(),
            // Same strictness as `get_conversation`: unknown names error
            // instead of silently listing the formation collection.
            Some(other) => {
                return Err(format!(
                    "unknown conversation collection {other:?} (expected \"formation\", \"recall\" or \"maintenance\")"
                )
                .into());
            }
        };
        // AndaDB 0.11 treats 0 as an empty page, which would panic on
        // `rt.first().unwrap()` below; clamp instead.
        let limit = limit.unwrap_or(10).clamp(1, 100);
        let cursor = match BTree::from_cursor::<u64>(&cursor)? {
            Some(cursor) => cursor,
            None => collection.max_document_id() + 1,
        };

        let filter = Filter::Field(("_id".to_string(), RangeQuery::Lt(Fv::U64(cursor))));

        let ids = collection.query_last_ids(filter, Some(limit)).await?;
        use futures::{StreamExt, TryStreamExt};
        let rt: Vec<Conversation> = futures::stream::iter(ids)
            .map(|id| collection.get_as::<Conversation>(id))
            .buffered(8)
            .try_collect()
            .await?;
        let cursor = if rt.len() >= limit {
            BTree::to_cursor(&rt.first().unwrap()._id)
        } else {
            None
        };
        Ok((rt, cursor))
    }

    async fn flush(&self) -> Result<(), BoxError> {
        self.db.flush().await?;
        Ok(())
    }
}

impl Space {
    /// Executes a settlement-built write KIP request. Only deterministic,
    /// code-generated commands go through here — never model output.
    ///
    /// A timeout answers `outcome_unknown` rather than an error: the write may
    /// already have committed (Spec §80.3), and reporting a clean failure would
    /// invite a caller to redo work that is durable. Every settlement command
    /// writes absolute values, so the honest answer costs nothing but a re-run
    /// on the next cycle.
    async fn run_kip_settlement(&self, request: Request) -> Result<Response, BoxError> {
        let nexus = self.memory.nexus();
        match timeout(
            SETTLEMENT_KIP_TIMEOUT,
            self.native_tasks
                .run(async move { Ok(execute_request(nexus.as_ref(), &request).await) }),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => Ok(Response::outcome_unknown(KipError::new(
                KipErrorCode::OutcomeUnknown,
                format!(
                    "settlement KIP execution timed out after {} seconds; whether it committed is \
                     unknown until the transaction is looked up",
                    SETTLEMENT_KIP_TIMEOUT.as_secs()
                ),
            ))),
        }
    }
}

/// The settlement's adapter onto this Space's graph: `readonly` picks between
/// the two executors the Space already has, so a pass cannot reach the write
/// path by choosing the wrong one.
impl settlement::RunKip for Space {
    async fn attention_sweep(&self) -> Option<crate::types::WatchSettlement> {
        Some(match self.attention.tick().await {
            Ok(p) => crate::types::WatchSettlement {
                fired: p.fired as u64,
                disarmed: p.expired as u64,
                deferred: (p.blocked.saturating_sub(p.blocked_wakes)
                    + p.advanced.saturating_sub(p.fired + p.expired))
                    as u64,
                conflicted: p.conflicted as u64,
                error: p.error,
            },
            Err(e) => crate::types::WatchSettlement {
                error: Some(e.to_string()),
                ..Default::default()
            },
        })
    }
    async fn advance_watch(
        &self,
        id: &str,
        version: u64,
        generation: u64,
    ) -> Result<serde_json::Value, anda_kip::KipError> {
        self.attention.advance(id.into(), version, generation).await
    }

    fn space_id(&self) -> &str {
        &self.id
    }

    async fn run_kip(&self, request: Request, readonly: bool) -> Result<Response, BoxError> {
        if readonly {
            self.execute_kip_readonly(request).await
        } else {
            self.run_kip_settlement(request).await
        }
    }
}

async fn init_conversation_collection(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["user"]).await?;
    // Closing reconciles only active rows, without scanning every historical
    // message document. Build the index once on first upgraded open.
    collection.create_btree_index_nx(&["status"]).await?;
    collection.remove_btree_index(&["thread"]).await?;
    collection.remove_btree_index(&["period"]).await?;
    collection
        .remove_bm25_index(&["messages", "resources", "artifacts"])
        .await?;
    Ok(())
}

async fn init_resource_collection(collection: &mut Collection) -> Result<(), DBError> {
    collection.set_tokenizer(jieba_tokenizer());
    collection.create_btree_index_nx(&["tags"]).await?;
    collection.create_btree_index_nx(&["hash"]).await?;
    collection.create_btree_index_nx(&["mime_type"]).await?;
    collection
        .remove_bm25_index(&["name", "description", "metadata"])
        .await?;
    Ok(())
}

/// Brings a freshly opened Nexus up to the vocabulary the brain writes against.
///
/// A Space that has activated nothing resolves Core alone, and Core declares no
/// Concept types at all — `Person`, `Event`, `Experience`, `SleepTask` and the
/// `MnemonicState` Facet all come from the Cognitive Memory Profile, so without
/// it in force nothing the brain writes even parses.
///
/// KIP 1.x also seeded `$self` and `$system` Person nodes here. Neither is
/// created any more: a Person is cognition and a Principal is authority, and
/// the engine registers its own system Principal at `connect`. What the brain
/// knows about itself now lives in a `SelfModel` Concept that Maintenance
/// consolidates from evidence — descriptive, and unable to grant anything.
///
/// `install_and_activate` re-activates only when the lock actually changes, so
/// running this on every open does not walk the Schema Environment version
/// forward on each restart.
async fn init_nexus_kip(nexus: &CognitiveNexus) -> Result<(), BoxError> {
    // Load first: a Space that has been running already has a vocabulary of its
    // own in force, and activating the Profile alone would deactivate it — a
    // host owns its Space's lock, so dropping an artifact from that list is
    // exactly how a package is retired.
    let vocabulary = crate::vocabulary::MemoryVocabulary::load(nexus).await?;
    vocabulary.activate(nexus).await?;
    designate_self_concept(nexus).await;
    Ok(())
}

/// Graph ids and projection semantics changed during the v1 import. Retire
/// only old-id usage rows, reset derived metrics/misses once, and preserve
/// conversations, tokens, policies and any already-recorded v2 usage.
pub(crate) async fn reset_v1_bookkeeping(db: &Arc<AndaDB>) -> Result<(), BoxError> {
    const MARKER: &str = "brain_kip2_bookkeeping_reset";
    if !db
        .metadata()
        .collections
        .contains(anda_cognitive_nexus::migrate::LEGACY_STAGING)
        || db.get_extension(MARKER).is_some()
    {
        return Ok(());
    }
    let mut removed = 0;
    if db.metadata().collections.contains("memory_usage") {
        let usage = db
            .open_collection("memory_usage".into(), async |_| Ok(()))
            .await?;
        for id in usage.ids() {
            let row: serde_json::Value = usage.get_as(id).await?;
            if row["entity"]
                .as_str()
                .is_some_and(|id| id.starts_with("C:") || id.starts_with("P:"))
            {
                usage.remove(id).await?;
                removed += 1;
            }
        }
        usage.flush(unix_ms()).await?;
    }
    if db.metadata().collections.contains("recall_misses") {
        db.delete_collection("recall_misses").await?;
    }
    for key in [
        "memory_metrics",
        "source_reliability",
        "memory_graph_counters",
        "memory_settlement",
        "memory_self_test",
        "memory_self_test_cursor",
        "shadow_report",
        "schema_audit",
        "correction_cursor",
    ] {
        db.remove_extension(key).await?;
    }
    db.save_extension_from(
        MARKER.into(),
        &serde_json::json!({"version":1,"removed_usage_rows":removed,"at":unix_ms()}),
    )
    .await?;
    Ok(())
}

/// Points the Space's §5.6 self identity at the `$self` Person, when there is
/// one.
///
/// `DESCRIBE PRIMER` reports the authenticated Principal and the semantic
/// `$self` as the two different things §64.2 requires it to distinguish, and
/// the brain puts that primer in front of every agent — beside a Recall prompt
/// that opens "you operate on behalf of `$self`, the owner of this
/// MemorySpace". Leaving the designation unset would put a primer saying this
/// Space has no `$self` next to a policy saying it has one, in the same
/// context window.
///
/// So it is designated where it exists and left alone where it does not. A
/// Space this brain has never digested a wiki into has no `$self` Concept —
/// `init_nexus_kip` deliberately seeds no Person, because a Person is cognition
/// and a Principal is authority — and inventing one here to fill the slot would
/// be the host writing the Brain's own identity into the graph. A primer that
/// says "none designated" is the honest answer for such a Space.
///
/// Protected Space configuration, so it goes through the Governance operation
/// rather than KML: §5.6 forbids ordinary KML from creating or changing it,
/// which is what stops cognitive content from deciding who the Brain is.
///
/// Best-effort and idempotent: an already-designated Space is left untouched,
/// and a failure is logged rather than blocking the open. Nothing the brain
/// writes depends on the designation — it is orientation, not authority.
async fn designate_self_concept(nexus: &CognitiveNexus) {
    use anda_cognitive_nexus::nexus::DEFAULT_SPACE;

    // Read straight off the Space row. `DESCRIBE PRIMER` reports the same
    // field, but it builds its element counts by enumerating every Concept,
    // Proposition, Assertion, Evidence and Activity in the Space — a full scan
    // per open, and per eval fork, to answer one boolean.
    match nexus.store.get_space(DEFAULT_SPACE).await {
        Ok(space) if !space.self_concept.is_empty() => return,
        Ok(_) => {}
        Err(err) => {
            log::warn!(
                target: "brain",
                "reading the Space's $self designation failed: {err:?}"
            );
            return;
        }
    }

    let found = execute_request(
        nexus,
        &kip::request_with(
            r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: :key} } LIMIT 1"#,
            kip::param("key", SELF_ACTOR_KEY),
        ),
    )
    .await;
    let Some(id) = kip::ok_result(&found)
        .and_then(|result| result.as_array())
        .and_then(|rows| rows.first())
        .and_then(serde_json::Value::as_str)
        .and_then(|id| id.parse::<anda_cognitive_nexus::id::ElementId>().ok())
    else {
        return;
    };

    if let Err(err) = nexus
        .system_session()
        .designate_self(DEFAULT_SPACE, Some(id))
        .await
    {
        log::warn!(
            target: "brain",
            "designating the Space's $self identity failed; DESCRIBE PRIMER will \
             report none: {err:?}"
        );
    }
}

#[cfg(test)]
impl Space {
    pub(crate) fn ctx_for_test(
        &self,
        user: Principal,
        agent_name: &str,
    ) -> Result<anda_engine::context::AgentCtx, BoxError> {
        self.engine
            .ctx_with(user, agent_name, agent_name, Default::default())
    }

    pub(crate) fn maintenance_for_test(&self) -> Arc<MaintenanceAgent> {
        self.maintenance.clone()
    }
}

impl Space {
    /// Model completion for online self-test query generation and shadow
    /// diagnostic fallback. It does not grant learning standing and is not
    /// a separate HTTP/MCP operation.
    pub(crate) async fn diagnostic_complete(
        &self,
        req: anda_core::CompletionRequest,
    ) -> Result<AgentOutput, BoxError> {
        use anda_core::CompletionFeatures;

        let ctx = self.engine.ctx_with(
            SELF_USER_ID,
            RecallAgent::NAME,
            RecallAgent::NAME,
            Default::default(),
        )?;
        ctx.completion(req, Vec::new()).await
    }
}

/// Timeout for one settlement-built write KIP command.
const SETTLEMENT_KIP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a Space may go without a maintenance cycle before the background
/// pass runs one on the clock.
///
/// The counting triggers (21 / 42 / 168 formation conversations) pace a Space
/// that is being written to; this is the floor for one that is not. A day is
/// short enough that a due Commitment or a lapsed retention date is acted on
/// while it still matters, and long enough that a mostly-idle Space costs one
/// model call a day.
const MAINTENANCE_MAX_INTERVAL_MS: u64 = 24 * 3_600 * 1_000;

/// The `key` of the Concept this brain treats as its semantic self (§5.6).
///
/// A `key`, not a name: a key is immutable identity and a name is a mutable
/// label. Nothing seeds this Concept — `init_nexus_kip` deliberately creates no
/// Person — so a Space has one only where the wiki digest minted it as the
/// actor its extracted claims are attributed to.
pub(crate) const SELF_ACTOR_KEY: &str = "$self";

/// This Space's memory policy: the stored instance policy or compiled defaults.
///
/// A free function rather than only a [`Space`] method because the agents are
/// built before the `Space` that owns them, and a policy knob read through a
/// second copy of this fallback chain is a knob that eventually disagrees with
/// itself.
fn memory_policy_of(db: &AndaDB) -> MemoryPolicy {
    db.get_extension_as(MemoryPolicy::EXTENSION_KEY)
        .unwrap_or_default()
}

/// Strips the recall self-report footer from assistant text in a chat
/// history (plan M4): `content` is stripped by the callers, and the history
/// must not re-leak the markup to clients that read it.
fn strip_recall_meta_from_history(history: &mut [anda_core::Message]) {
    for message in history {
        if message.role != "assistant" {
            continue;
        }
        for part in &mut message.content {
            if let anda_core::ContentPart::Text { text } = part
                && text.contains(assess::RECALL_META_TAG_OPEN)
            {
                let (stripped, _) = assess::split_recall_meta(text);
                *text = stripped;
            }
        }
    }
}

/// How many elements of one Core kind a `PURGE` erased.
fn purged_of_kind(response: &anda_kip::Response, kind: &str) -> u64 {
    kip::ok_result(response)
        .and_then(|result| result.get("changes"))
        .and_then(serde_json::Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter(|change| {
                    change.get("op").and_then(serde_json::Value::as_str) == Some("purge")
                        && change.get("kind").and_then(serde_json::Value::as_str) == Some(kind)
                })
                .count() as u64
        })
        .unwrap_or(0)
}

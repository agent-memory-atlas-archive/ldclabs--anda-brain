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
    extension::note::NoteTool,
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
    last_access_ms: AtomicU64,
}

impl SpaceEntry {
    fn new() -> Self {
        Self {
            cell: OnceCell::new(),
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
    /// Bounds requests that can each drive a full multi-turn LLM round.
    /// One budget for every channel: the HTTP LLM routes and the MCP LLM
    /// tools (recall/maintenance) drain this same semaphore, so neither
    /// channel can turn request concurrency into unbounded model spend.
    llm_semaphore: Arc<tokio::sync::Semaphore>,

    pub app_name: String,
    pub app_version: String,
    pub sharding: u32,
}

mod attention;
#[cfg(feature = "experiments")]
pub mod experiments;
mod processing;
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

    /// Sets the cap on concurrent LLM-billed requests (the service's
    /// `LLM_MAX_CONCURRENCY` flag; consuming builder, call before the state
    /// is cloned). `max(1)` keeps a misconfigured `0` from shedding every
    /// request.
    pub fn with_llm_concurrency(mut self, max: usize) -> Self {
        self.llm_semaphore = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self
    }

    /// The shared LLM concurrency budget (see the field doc): the HTTP LLM
    /// routes and the MCP LLM tools must both draw permits from it.
    pub fn llm_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.llm_semaphore
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
        space.attention.discover_existing().await?;
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

    async fn flush_and_evict_once(&self, now: u64, idle_timeout_ms: u64) {
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

    /// Evicts an idle space entry, closing its database *before* removing it
    /// from the map. The close happens while holding the map write lock so a
    /// concurrent `load_space` cannot connect a second AndaDB instance to the
    /// same storage while the old one is still flushing. Idle spaces were
    /// already flushed by the periodic pass, so this close is cheap.
    async fn try_evict_idle_space(
        &self,
        id: &str,
        entry: &Arc<SpaceEntry>,
        now_ms: u64,
        idle_timeout_ms: u64,
    ) -> bool {
        let mut spaces = self.spaces.write().await;
        let Some(current_entry) = spaces.get(id) else {
            return false;
        };
        if !Arc::ptr_eq(current_entry, entry) {
            return false;
        }

        let is_idle = now_ms.saturating_sub(entry.last_access_ms()) > idle_timeout_ms;
        if !is_idle {
            return false;
        }

        match entry.cell.get() {
            Some(space) => {
                // An in-flight wiki digest holds the DB too (it is kicked
                // right after maintenance finishes, in the same window this
                // check races against).
                #[cfg(feature = "wiki")]
                if space.wiki_digest.is_processing() || space.wiki.is_busy() {
                    return false;
                }
                if space.pinned || space.is_busy() {
                    return false;
                }
                // Map + background snapshot are the only expected SpaceEntry refs here;
                // OnceCell is the only expected Space ref. Anything more means a request
                // has recently loaded or is still using this space, so eviction waits.
                if Arc::strong_count(entry) > 2 || Arc::strong_count(space) > 1 {
                    return false;
                }
                if let Err(err) = space.close().await {
                    log::error!(target: "brain", space_id = id; "close before eviction failed: {err:?}");
                }
            }
            None => {
                // Initialization never succeeded (e.g. probes for unknown space
                // IDs). Drop the unused placeholder so such probes cannot grow
                // the map unboundedly.
                if Arc::strong_count(entry) > 2 {
                    return false;
                }
            }
        }

        spaces.remove(id).is_some()
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
                ("decay", &report.decay_error),
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
    engine: Engine,
    http_client: reqwest::Client,
    models: Arc<Models>,
    maintenance: Arc<MaintenanceAgent>,
    pinned: bool,
    automatic: bool,
    clock: Arc<crate::runtime::BusinessClock>,
    tasks: crate::runtime::RuntimeTasks,
    attention: Arc<crate::attention::AttentionRuntime>,
    memory_runtime: Option<Arc<crate::runtime_api::MemoryRuntime>>,
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
    miss_cache: Arc<MissCache>,
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

    fn get_tier(&self) -> SpaceTier {
        self.db.get_extension_as("tier").unwrap_or_default()
    }

    pub async fn admin_update_tier(&self, tier: u32, now_ms: u64) -> Result<SpaceTier, BoxError> {
        let tier = SpaceTier {
            tier,
            updated_at: now_ms,
        };
        self.db
            .save_extension_from("tier".to_string(), &tier.to_ref())
            .await?;
        Ok(tier)
    }

    pub async fn add_space_token(
        &self,
        token: String,
        input: AddSpaceTokenInput,
        now_ms: u64,
    ) -> Result<SpaceToken, BoxError> {
        // Serialize mints: the count cap and name-uniqueness checks below
        // read shared extension state, and two concurrent mints must not
        // both pass them.
        let _guard = self.token_lock.lock().await;
        let count = self
            .db
            .extensions_with(|kv| kv.keys().filter(|k| k.starts_with("ST")).count());
        if count >= 100 {
            return Err("space token limit reached".into());
        }

        // The token name is the audit identity (`st:{name}`): it is required
        // and unique, or two tokens would be indistinguishable in the event
        // log (and un-revokable by name).
        let name = input.name.trim().to_string();
        if name.is_empty() {
            return Err("space token name is required".into());
        }
        if self
            .list_space_tokens()?
            .iter()
            .any(|st| st.name.trim() == name)
        {
            return Err(format!("space token name {name:?} already exists").into());
        }

        let labels = match input.labels {
            Some(labels) => {
                // Label-restricted tokens are read-only wiki viewers (PRD
                // §8.2): any write scope would let them commit to, archive,
                // relabel or export documents behind labels they cannot read.
                if input.scope != TokenScope::Read {
                    return Err("labeled tokens must have read scope".into());
                }
                let mut cleaned: Vec<String> = labels
                    .iter()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                cleaned.sort();
                cleaned.dedup();
                if cleaned.is_empty() && !labels.is_empty() {
                    return Err("labels must not be blank".into());
                }
                Some(cleaned)
            }
            None => None,
        };

        let sp = SpaceToken {
            token: token.clone(),
            scope: input.scope,
            name,
            expires_at: input.expires_at,
            labels,
            created_at: now_ms,
            updated_at: now_ms,
            ..Default::default()
        };

        self.db.save_extension_from(token, &sp.to_ref()).await?;
        Ok(sp)
    }

    pub fn verify_space_token(
        &self,
        token: String,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<SpaceToken, BoxError> {
        // Space tokens always carry the "ST" prefix. Rejecting other keys here
        // keeps non-token extensions (e.g. "byok", "tier") out of the
        // credential lookup below.
        if !token.starts_with("ST") {
            return Err("invalid space token".into());
        }
        let token = self
            .db
            .set_extension_from_with::<_, SpaceToken>(token, |v| {
                if let Some(mut st) = v
                    && st.expires_at.map(|exp| exp > now_ms).unwrap_or(true)
                    && st.scope.allows(scope)
                    // Labeled tokens are read-only wiki viewers; a legacy row
                    // carrying a write scope fails closed here (PRD §8.2).
                    && (st.labels.is_none() || scope == TokenScope::Read)
                {
                    st.usage = st.usage.saturating_add(1);
                    st.updated_at = now_ms;
                    return Some(st);
                }
                None
            });

        token.ok_or_else(|| "invalid space token".into())
    }

    pub async fn revoke_space_token(&self, token: &str) -> Result<bool, BoxError> {
        // Same guard as verify_space_token: the token is caller-supplied, so
        // restricting it to the "ST" prefix keeps non-token extensions
        // (e.g. "byok", "tier", "owner") safe from deletion through this API.
        if !token.starts_with("ST") {
            return Err("invalid space token".into());
        }
        let rt = self.db.remove_extension(token).await?;
        Ok(rt.is_some())
    }

    /// Revokes a token by its (unique) name. This is the recovery path for
    /// managers who did not save the token value at mint time —
    /// `list_space_tokens` deliberately never echoes full token values.
    pub async fn revoke_space_token_by_name(&self, name: &str) -> Result<bool, BoxError> {
        let name = name.trim();
        if name.is_empty() {
            return Err("invalid space token name".into());
        }
        let key = self.db.extensions_with(|kvs| {
            kvs.iter().find_map(|(k, v)| {
                (k.starts_with("ST")
                    && v.clone()
                        .deserialized::<SpaceToken>()
                        .is_ok_and(|st| st.name.trim() == name))
                .then(|| k.clone())
            })
        });
        match key {
            Some(key) => Ok(self.db.remove_extension(&key).await?.is_some()),
            None => Ok(false),
        }
    }

    pub fn list_space_tokens(&self) -> Result<Vec<SpaceToken>, BoxError> {
        let tokens: Vec<SpaceToken> = self.db.extensions_with(|kvs| {
            kvs.iter()
                .filter_map(|(k, v)| {
                    if k.starts_with("ST")
                        && let Ok(mut st) = v.clone().deserialized::<SpaceToken>()
                    {
                        // The map key *is* the bearer credential: expose only
                        // a display prefix, or any Write-scoped manager could
                        // harvest every other caller's token in plaintext.
                        st.token = if k.len() > 8 {
                            format!("{}…", k.chars().take(8).collect::<String>())
                        } else {
                            k.clone()
                        };
                        Some(st)
                    } else {
                        None
                    }
                })
                .collect()
        });

        Ok(tokens)
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
        self.models.set_model(model);
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
        mut input: MaintenanceInput,
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
        // claim is consumed (inherited) by `MaintenanceAgent::run`.
        let claim = self
            .maintenance
            .try_claim_processing()
            .ok_or("Maintenance cycle is already in progress.")?;
        input.formation_id = self.formation.get_processed().unwrap_or_default();
        // Callers that pass explicit parameters keep them; everyone else runs
        // under the space's memory policy. Default policy values equal the
        // defaults documented in BrainMaintenance.md, so an unset policy is
        // not a behavior change (plan module M-P).
        let mut effective_policy = self.memory_policy();
        if let Some(parameters) = &input.parameters {
            if let Some(value) = parameters.memory_strength_decay_factor {
                effective_policy.memory_strength_decay_factor = value;
            }
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
            .settle_memory_metabolism_using(
                input.scope,
                self.clock.now_ms(),
                DECAY_MIN_INTERVAL_MS,
                effective_policy,
            )
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
        let rt = self
            .engine
            .agent_run(
                user,
                AgentInput {
                    name: MaintenanceAgent::NAME.to_string(),
                    prompt: StringOr::Value(&input).to_string(),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await?;
        // `agent_run` returning Ok proves `MaintenanceAgent::run` inherited
        // and already released the claim, so its Drop is a no-op; forget it
        // to close the ABA window where a later claim's flag could be
        // clobbered. The Err path above keeps Drop as the release.
        std::mem::forget(claim);
        Ok(rt)
    }

    /// The last memory-metabolism settlement report, when one has run.
    fn memory_settlement(&self) -> Option<MemorySettlementReport> {
        self.db.get_extension_as("memory_settlement")
    }

    /// What the settlement measured, as the Maintenance prompt receives it.
    ///
    /// Both extensions predate any reader: `audit_schema` and correction
    /// discovery have been writing them since the memory-evolution plan
    /// landed, while nothing downstream ever opened them. `BrainMaintenance.md`
    /// §A.1 has meanwhile told the model that the schema census is in its
    /// input, which it was not.
    ///
    /// A `quick` or `daydream` cycle takes no census of its own, so it reads
    /// the last full cycle's — which is why `audited_at` travels with it.
    async fn maintenance_assessment(
        &self,
        current_settlement_error: Option<&str>,
    ) -> crate::types::MaintenanceAssessment {
        let audit: Option<SchemaAudit> = self.db.get_extension_as("schema_audit");
        let settlement = self.memory_settlement();
        let settlement_errors =
            settlement_error_messages(settlement.as_ref(), current_settlement_error);
        crate::types::MaintenanceAssessment {
            settlement_errors,
            audited_at: audit.as_ref().map(|audit| audit.audited_at),
            predicates: audit.map(|audit| audit.predicates).unwrap_or_default(),
            source_reliability: self
                .db
                .get_extension_as("source_reliability")
                .unwrap_or_default(),
            space_seq: self.current_space_seq().await,
            armed_watches: settlement::watches_in_status(self, "armed").await,
            fired_watches: settlement::watches_in_status(self, "fired").await,
            consumed_seq: None,
            revised_roots: settlement
                .map(|report| report.revised_roots)
                .unwrap_or_default(),
        }
    }

    /// The Space's sequence coordinate right now.
    ///
    /// Read off the Space row rather than derived from a query: it is the
    /// `basis_seq` a refreshed `WorkingState` has to be stamped with, and a
    /// digest that guessed its own basis would be a derived view claiming a
    /// consistency it does not have.
    async fn current_space_seq(&self) -> Option<u64> {
        use anda_cognitive_nexus::nexus::DEFAULT_SPACE;

        match self.memory.nexus().store.current_seq(DEFAULT_SPACE).await {
            Ok(seq) => Some(seq),
            Err(err) => {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "reading the Space sequence for the maintenance assessment failed: {err:?}"
                );
                None
            }
        }
    }

    /// Bumps the incrementally-updated observability counters (plan M12).
    /// Writers pay one in-memory extension update; readers never pay a
    /// heavy query.
    fn bump_metrics(&self, update: impl FnOnce(&mut MemoryMetrics)) {
        let now_ms = unix_ms();
        let _ = self
            .db
            .set_extension_from_with("memory_metrics".to_string(), |value| {
                let mut metrics: MemoryMetrics = value.unwrap_or_default();
                update(&mut metrics);
                metrics.updated_at = now_ms;
                Some(metrics)
            });
    }

    /// Memory observability snapshot (plan M12): incrementally-maintained
    /// counters, derived rates, graph counts, and the latest module reports.
    pub async fn memory_status(&self) -> MemoryStatus {
        fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
            (denominator > 0).then(|| numerator as f64 / denominator as f64)
        }

        let metrics: MemoryMetrics = self
            .db
            .get_extension_as("memory_metrics")
            .unwrap_or_default();
        // Graph counters come from the settlement-time census (M12: readers
        // never pay heavy queries — the orphan count is a near-full scan,
        // and this endpoint is reachable anonymously on public spaces). A
        // space that has never settled reports the free in-memory counts;
        // `as_of: None` and the omitted scan-backed fields say "not yet
        // censused" without running any scan here.
        let graph = self
            .db
            .get_extension_as::<MemoryGraphCounters>("memory_graph_counters")
            .unwrap_or_else(|| MemoryGraphCounters {
                concepts: self.memory.nexus().store.concepts().len() as u64,
                propositions: self.memory.nexus().store.propositions().len() as u64,
                ..Default::default()
            });
        let maintenance_usage: Usage = self
            .db
            .get_extension_as("maintenance_usage")
            .unwrap_or_default();
        let maintenance_tokens = maintenance_usage
            .input_tokens
            .saturating_add(maintenance_usage.output_tokens);

        MemoryStatus {
            groundability: ratio(metrics.self_test_grounded, metrics.self_test_tested),
            probe_hit_rate: ratio(
                metrics.probe_hits,
                metrics.probe_hits + metrics.probe_misses,
            ),
            correction_rate: ratio(metrics.corrections, metrics.recalls_completed),
            avg_uncertainty: (metrics.uncertainty_reports > 0)
                .then(|| metrics.uncertainty_sum / metrics.uncertainty_reports as f64),
            maintenance_tokens_per_recall: ratio(maintenance_tokens, metrics.recalls_completed),
            metrics,
            graph,
            last_settlement: self.memory_settlement(),
            last_self_test: self.db.get_extension_as("memory_self_test"),
            last_shadow: self.db.get_extension_as("shadow_report"),
            last_schema_audit: self.db.get_extension_as("schema_audit"),
        }
    }

    /// Counts the graph-health numbers `memory_status` reports. Heavy (the
    /// orphan query is a near-full scan), so it runs at settlement time and
    /// the result is cached in the `memory_graph_counters` extension.
    async fn census_graph_counters(&self, now_ms: u64) -> MemoryGraphCounters {
        let formation = self.formation_status();
        MemoryGraphCounters {
            concepts: formation.concepts as u64,
            propositions: formation.propositions as u64,
            unconsolidated: assess::kip_count_sum(self, assess::UNCONSOLIDATED_COUNT_KQL).await,
            orphans: assess::orphan_count(self).await,
            predicate_types: self.registered_predicates().await.map(|p| p.len() as u64),
            as_of: Some(now_ms),
        }
    }

    /// Per-predicate link census (plan M8), run by full-scope settlements.
    /// The counts feed the schema-sprawl metric and give the Maintenance
    /// prompt's merge guidance real numbers to look at.
    async fn audit_schema(&self, now_ms: u64) -> Result<(), BoxError> {
        let names = self.registered_predicates().await.unwrap_or_default();

        // Serial, bounded to 50 predicates: each count is a scan and this
        // runs while the settlement lock is held, so it must not hammer the
        // graph with parallel scans.
        let mut predicates = BTreeMap::new();
        for name in names.into_iter().take(50) {
            let count = assess::kip_count(
                self,
                &format!(
                    "FIND(COUNT(?link)) WHERE {{ ?link (?s, {}, ?o) }}",
                    kip::string_literal(&name)
                ),
            )
            .await;
            // A failed count (typically the engine's full-scan cap on the
            // busiest predicates) must be *absent*, not zero: reporting the
            // most-used predicate as having zero links would point the
            // Phase-6 merge guidance at exactly the wrong target.
            match count {
                Some(count) => {
                    predicates.insert(name, count);
                }
                None => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "schema census count failed for predicate `{name}`; omitted from audit"
                    );
                }
            }
        }
        self.db.set_extension_from(
            "schema_audit".to_string(),
            SchemaAudit {
                audited_at: now_ms,
                predicates,
            },
        );
        Ok(())
    }

    /// The predicates this Space's Schema Environment declares.
    ///
    /// KIP 1.x read these off `$PropositionType` Concepts, which an ordinary
    /// write could mint; 2.0 resolves predicates from immutable Schema Packages
    /// and answers `LIST PREDICATES` from the active environment. `None` when
    /// the introspection itself failed — an empty vocabulary and an unreachable
    /// one are not the same answer.
    pub(crate) async fn registered_predicates(&self) -> Option<Vec<String>> {
        let response = self
            .execute_kip_readonly(kip::request("LIST PREDICATES LIMIT 500"))
            .await
            .ok()?;
        if !kip::succeeded(&response) {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "listing registered predicates failed: {}",
                kip::error_message(&response)
            );
            return None;
        }
        Some(
            kip::ok_result(&response)
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            entry
                                .get("local_name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    /// Ledger rows corrected after `since_ms` — the scenario-mining signal
    /// (plan M9).
    pub async fn corrected_entities(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<String>, BoxError> {
        Ok(self
            .ledger
            .corrected_since(since_ms, limit)
            .await?
            .into_iter()
            .map(|row| row.entity)
            .collect())
    }

    /// Installs an independent judge model for eval runs (plan M9): judge
    /// completions stop sharing the evaluated system's model and blind spots.
    pub fn set_judge_model(&self, config: ModelConfig) -> Result<(), BoxError> {
        if config.disabled {
            return Err("judge model is disabled".into());
        }
        let engine_config: EngineModelConfig = config.into();
        let model = engine_config.model(self.http_client.clone())?;
        *self.judge_model.write().expect("judge model lock poisoned") = Some(Arc::new(model));
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
        *self.judge_model.write().expect("judge model lock poisoned") = Some(Arc::new(model));
    }

    /// Deterministic memory metabolism (plan M2/M3), run before each
    /// maintenance cycle. The passes themselves live in [`crate::settlement`],
    /// behind its `RunKip` port; this is what a Space owes them and what it
    /// does with what they decide:
    ///
    /// 1. **Bulk disuse metabolism** (every scope, rate-limited to
    ///    `DECAY_MIN_INTERVAL_MS` by the sweep's own `last_metabolized_at`
    ///    filter): the Phase-7 decay the Maintenance prompt used to run by
    ///    hand. Pinned Concepts are exempt. It decays
    ///    `MnemonicState.memory_strength`, never Assertion confidence.
    /// 2. **Correction discovery** (every scope): newly superseded links are
    ///    recorded in the ledger and aggregated per asserting actor into the
    ///    `source_reliability` extension. The scan is the settlement's; the
    ///    ledger and the extension are this Space's, so it applies them here.
    /// 3. **Watch expiry** and the **Skill lifecycle** (every scope), then
    ///    **retention expiry and the schema census** (`full` scope).
    ///
    /// Nothing here reads the usage ledger back into the graph: see the body
    /// for why recall no longer reinforces what it touched.
    #[cfg(any(test, feature = "experiments"))]
    async fn settle_memory_metabolism(
        &self,
        scope: MaintenanceScope,
        now_ms: u64,
    ) -> Result<MemorySettlementReport, BoxError> {
        self.settle_memory_metabolism_with(scope, now_ms, DECAY_MIN_INTERVAL_MS)
            .await
    }

    /// [`Self::settle_memory_metabolism`] with control over the decay rate
    /// limit. Shadow forks pass `0`: they inherit the live space's
    /// `decay_applied_at` stamps, and under the weekly gate both forks would
    /// settle identically whenever the live space decayed within the window
    /// — every comparison of decay knobs would be a systematic tie.
    async fn settle_memory_metabolism_with(
        &self,
        scope: MaintenanceScope,
        now_ms: u64,
        decay_min_interval_ms: u64,
    ) -> Result<MemorySettlementReport, BoxError> {
        self.settle_memory_metabolism_using(
            scope,
            now_ms,
            decay_min_interval_ms,
            self.memory_policy(),
        )
        .await
    }

    async fn settle_memory_metabolism_using(
        &self,
        scope: MaintenanceScope,
        now_ms: u64,
        decay_min_interval_ms: u64,
        policy: MemoryPolicy,
    ) -> Result<MemorySettlementReport, BoxError> {
        let _guard = self.settlement_lock.lock().await;
        let mut report = MemorySettlementReport {
            settled_at: now_ms,
            ..Default::default()
        };

        // There is deliberately no reinforcement pass here.
        //
        // Until this was removed, every completed recall's touched Concepts
        // were drained out of the usage ledger and their
        // `MnemonicState.memory_strength` raised by `recall_reinforcement`.
        // That is the one thing the reference Recall policy forbids outright:
        // §1 ("Recall MUST NOT ... change memory_strength, increment recall
        // counters"), §32 ("Repeated Recall must not automatically increase
        // memory_strength/confidence/salience"), invariant 2 ("Read does not
        // reinforce memory"). Deferring the write to maintenance did not make
        // reading stop reinforcing; it only moved where the reinforcement was
        // written from.
        //
        // The ledger stays, as instrumentation: it still tells the dream
        // self-test which memories have never been exercised, still feeds
        // `entities_recalled` and the correction rate, and still supplies the
        // scenario miner. What it no longer does is close a loop back into
        // cognitive state. Reading is now observed and not rewarded — which is
        // also why a recalled Concept is no longer spared the sweep below.
        report.decay_ran = true;
        let decay = settlement::metabolize(self, &policy, now_ms, decay_min_interval_ms).await;
        report.decayed = decay.decayed;
        report.decay_error = decay.error;

        let after = self
            .db
            .get_extension_as::<settlement::CorrectionCursor>("correction_cursor")
            .unwrap_or_else(|| {
                self.db
                    .get_extension_as::<u64>("correction_cursor")
                    .unwrap_or(0)
                    .into()
            });
        let corrections = settlement::scan_corrections(self, after.clone()).await;
        report.correction_scan_error = corrections.error;
        report.correction_scan_incomplete = corrections.incomplete;
        report.correction_scan_through_seq = corrections.watermark;
        // The derivation review's input (§57.5): what each revised root fed,
        // walked here so the cycle is handed a list rather than a guess.
        report.revised_roots = settlement::revised_roots(self, &corrections.rows).await;
        for row in corrections.rows {
            if !self
                .ledger
                .record_correction(&row.assertion, now_ms)
                .await?
            {
                continue;
            }
            report.new_corrections += 1;
            let Some(actor) = row.actor else { continue };
            let _ = self
                .db
                .set_extension_from_with("source_reliability".to_string(), |value| {
                    let mut map: BTreeMap<String, SourceReliability> = value.unwrap_or_default();
                    let entry = map.entry(actor.clone()).or_default();
                    entry.corrections += 1;
                    entry.last_corrected_at = now_ms;
                    Some(map)
                });
        }
        if corrections.cursor != after {
            self.db
                .set_extension_from("correction_cursor".to_string(), &corrections.cursor);
        }

        // Nexus checks generation, element CAS and complete authorized coverage.
        report.watches = settlement::sweep_watches(self).await;
        if let Some(error) = &report.watches.error {
            log::error!(
                target: "brain",
                space_id = self.id;
                "watch expiry failed — silence Watches are NOT firing: {error}"
            );
        }

        report.skills = settlement::skill_settlement();
        #[cfg(feature = "learning")]
        if self.learning.is_configured() {
            match self.learning.runtime_status(self.automatic, false).await {
                Ok(status) => {
                    report.skills.unsupported_reason = Some("learning runs in the independent scheduler; no comparison verdict executed by this maintenance call".into());
                    report.skills.runtime = Some(serde_json::to_value(status)?);
                }
                Err(_) => {
                    report.skills.error = Some("learning scheduler status unavailable".into())
                }
            }
        }

        // Retention expiry, full scope only. Both halves are the host
        // deciding *when* forgetting happens; the engine only ever decided
        // what may be forgotten. They are explicit calls rather than a
        // background timer for the reason the engine declines to run one: a
        // thread that removed memory on its own schedule would act while no
        // request was in flight and no Principal was accountable for it.
        //
        // This is also what makes `SET RETENTION` mean something here. The
        // maintenance policy's retention review tells the model to set expiry
        // on what should stop being kept; until something swept on
        // `expires_at`, that write was recorded and never honoured.
        if scope == MaintenanceScope::Full {
            report.retention = self.sweep_retention().await;
            if let Some(error) = &report.retention.error {
                log::error!(
                    target: "brain",
                    space_id = self.id;
                    "retention expiry failed — lapsed records are NOT being archived: {error}"
                );
            }
        }

        // Full cycles also refresh the per-predicate schema census (plan M8).
        if scope == MaintenanceScope::Full
            && let Err(err) = self.audit_schema(now_ms).await
        {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "schema audit failed: {err:?}"
            );
        }

        self.bump_metrics(|metrics| {
            metrics.corrections += report.new_corrections;
            metrics.decayed += report.decayed;
        });
        // Refresh the cached graph counters `memory_status` serves (M12:
        // readers never pay heavy queries).
        let counters = self.census_graph_counters(now_ms).await;
        self.db
            .set_extension_from("memory_graph_counters".to_string(), counters);
        self.db
            .set_extension_from("memory_settlement_at".to_string(), now_ms);
        self.db
            .set_extension_from("memory_settlement".to_string(), report.clone());
        self.db.flush_metadata(now_ms).await.ok();
        Ok(report)
    }

    /// Acts on what this Space's own retention said should stop being kept.
    ///
    /// Two passes over two different clocks, in this order:
    ///
    /// 1. **Lapsed claims.** An Assertion whose `valid_time.until` has passed
    ///    is marked `expired` (§14.3) — a lifecycle state the Cognitive Memory
    ///    Profile names and that nothing produced until the engine gained this
    ///    call. A projection still admits it at a coordinate its window
    ///    covered, so `FOR TIME` in the past does not lose every claim that has
    ///    since lapsed.
    /// 2. **Lapsed records.** An element whose `retention.expires_at` has
    ///    passed is archived: out of ordinary recall, still readable, still
    ///    referenced. Tombstone would withdraw it from use and purge would
    ///    destroy it, and neither is what an expiry date asked for.
    ///
    /// Purge is deliberately not reachable from here. §19.3 makes erasure
    /// high-impact with its own reference policy, and running it over a set the
    /// caller never enumerated would be the largest irreversible action this
    /// service can take, reached by a scheduled maintenance cycle. A forget
    /// request enumerates its target and purges that.
    ///
    /// Errors are reported rather than propagated: a settlement that could not
    /// sweep is a degraded cycle, not a failed one, and the surrounding passes
    /// have already done work worth keeping.
    async fn sweep_retention(&self) -> crate::types::RetentionSettlement {
        use anda_cognitive_nexus::nexus::{DEFAULT_SPACE, RetentionAction};

        let mut report = crate::types::RetentionSettlement::default();
        let session = self.memory.nexus().system_session();
        #[cfg(feature = "experiments")]
        let session = if self.clock.is_manual() {
            match session.with_simulated_lifecycle_time(&kip::timestamp(self.clock.now_ms())) {
                Ok(session) => session,
                Err(error) => {
                    report.error = Some(error.to_string());
                    return report;
                }
            }
        } else {
            session
        };

        // The two passes are independent — different clocks, and different
        // permissions (`expire_lapsed_assertions` needs the Assertion write,
        // `sweep_expired` needs `manage_retention` at Space scope) — so one
        // failing must not silently cancel the other. Letting it would leave a
        // report of one error and four zeros, which reads as "nothing had
        // lapsed" rather than "the record sweep never ran".
        let mut errors: Vec<String> = Vec::new();

        match session
            .expire_lapsed_assertions(DEFAULT_SPACE, settlement::SETTLEMENT_BATCH_LIMIT)
            .await
        {
            Ok(expired) => report.expired_assertions = expired.len() as u64,
            Err(err) => errors.push(format!("expiring lapsed claims: {err}")),
        }

        match session
            .sweep_expired(
                DEFAULT_SPACE,
                RetentionAction::Archive,
                settlement::SETTLEMENT_BATCH_LIMIT,
            )
            .await
        {
            Ok(sweep) => {
                report.archived = sweep.swept.len() as u64;
                report.held = sweep.held as u64;
                report.refused = sweep.refused as u64;
                report.remaining = sweep.remaining as u64;
            }
            Err(err) => errors.push(format!("archiving lapsed records: {err}")),
        }

        if !errors.is_empty() {
            report.error = Some(errors.join("; "));
        }
        report
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
        tokio::spawn(async move {
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
        let mut rt = Vec::with_capacity(ids.len());
        for id in ids {
            rt.push(collection.get_as::<Conversation>(id).await?);
        }
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

    /// Closes the space's database so AndaDB flushes collections and
    /// metadata. Callers that open throwaway spaces (eval runs, forks) must
    /// close them through this method instead of reaching into the DB handle.
    pub async fn close(&self) -> Result<(), BoxError> {
        let mut state = self.close_state.lock().await;
        if state.closed {
            return Ok(());
        }
        if let Some(runtime) = &self.memory_runtime {
            runtime.shutdown().await;
        }
        #[cfg(feature = "learning")]
        self.learning.shutdown().await;
        self.trust.shutdown().await;
        self.attention.shutdown().await;
        self.recall_receipts.shutdown().await;
        self.utility.shutdown().await;
        self.capture_interrupted_work();
        self.tasks.cancel();
        self.engine.cancel();
        self.tasks.shutdown().await;
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

    fn capture_interrupted_work(&self) {
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

    async fn create(
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
    async fn connect(
        object_store: Arc<dyn ObjectStore>,
        db_config: DBConfig,
        management: Arc<dyn Management>,
        http_client: reqwest::Client,
        models: Arc<Models>,
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
        let memory = Arc::new(MemoryManagement::connect(db.clone(), Arc::new(nexus)).await?);
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
        let models = Arc::new(Models::from_clone(models.as_ref()));
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
        let memory_r = TimedMemoryReadonly::new(memory.clone())
            .with_clock(clock.clone())
            .with_attention(attention.clone());
        let tasks = crate::runtime::RuntimeTasks::default();
        let memory_tool = MemoryTool::new(memory.clone());
        let note_tool = NoteTool::new();
        // Formation and Maintenance may grow this Space's vocabulary; Recall
        // may not, and gets the tool nowhere.
        let declare_tool = crate::vocabulary::DeclareSymbolsTool::new(memory.clone());

        let hooks = Arc::new(Hooks::new(db.clone()));
        let formation = Arc::new(
            FormationAgent::new(memory.clone(), conversations.clone(), hooks.clone(), 100000)
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
                GuardedMemory::new(memory.clone()).with_clock(clock.clone()),
            ))?
            .register_tool(Arc::new(memory_r))?
            .register_tool(Arc::new(memory_tool))?
            .register_tool(Arc::new(note_tool))?
            .register_tool(Arc::new(declare_tool))?
            .register_tool(Arc::new(crate::kip_reference::KipReferenceTool))?
            .register_tool(Arc::new(crate::cognitive::MemoryRuntimeTool::new(
                memory.clone(),
                attention.clone(),
            )))?;
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
            formation,
            recall,
            maintenance,
            ledger,
            recall_receipts,
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
            attention,
            memory_runtime,
            recovery_started: AtomicBool::new(false),
            close_state: tokio::sync::Mutex::new(CloseState::default()),
            interrupted_conversations: parking_lot::Mutex::new(BTreeMap::new()),
            #[cfg(feature = "learning")]
            learning,
        });
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
                this.models.set_model(model);
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
    fn start_background_recovery(self: &Arc<Self>) {
        if !self.automatic
            || self.engine.is_cancelled()
            || self.recovery_started.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let this_clone = self.clone();
        tokio::spawn(async move {
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
            execute_request(nexus.as_ref(), &request),
        )
        .await
        {
            Ok(res) => Ok(res),
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

struct Hooks {
    db: Arc<AndaDB>,
    space: OnceLock<Weak<Space>>,
}

impl Hooks {
    fn new(db: Arc<AndaDB>) -> Self {
        Self {
            db,
            space: OnceLock::new(),
        }
    }

    fn bind_space(&self, space: Weak<Space>) {
        let _ = self.space.set(space);
    }

    fn space(&self) -> Option<Arc<Space>> {
        self.space.get().and_then(Weak::upgrade)
    }
}

// grcov-excl-start: async_trait rewrites this impl into generated futures; behavior is covered by hook and agent scheduling tests.
#[async_trait::async_trait]
impl BrainHook for Hooks {
    fn is_maintenance_processing(&self) -> bool {
        self.space()
            .map(|space| space.maintenance.is_processing())
            .unwrap_or(false)
    }

    async fn on_conversation_end(&self, agent_name: &str, conversation: &Conversation) {
        #[cfg(feature = "experiments")]
        if self.space().is_some_and(|space| !space.automatic) {
            use experiments::{CostStage, StageCost};
            let stage = match agent_name {
                "formation_memory" => Some(CostStage::Formation),
                "recall_memory" => Some(CostStage::Recall),
                "maintenance_memory" => Some(CostStage::Maintenance),
                _ => None,
            };
            if let Some(stage) = stage {
                // Each callback is one execution, including a failed attempt
                // before Formation's retry. Do not overwrite it with final usage.
                let known = conversation.usage.requests > 0
                    || conversation.usage.input_tokens > 0
                    || conversation.usage.output_tokens > 0;
                let mut truncated = false;
                self.db
                    .set_extension_from_with("experiment_costs".into(), |value| {
                        let mut rows: Vec<StageCost> = value.unwrap_or_default();
                        if rows.len() >= 10_000 {
                            truncated = true;
                            return Some(rows);
                        }
                        rows.push(StageCost {
                            stage,
                            conversation: Some(conversation._id),
                            failed: conversation.status != ConversationStatus::Completed,
                            requests: known.then_some(conversation.usage.requests),
                            input_tokens: known.then_some(conversation.usage.input_tokens),
                            output_tokens: known.then_some(conversation.usage.output_tokens),
                            elapsed_ms: None,
                            accounting_complete: false,
                        });
                        Some(rows)
                    });
                if truncated {
                    self.db
                        .set_extension_from("experiment_costs_truncated".into(), true);
                }
            }
        }
        match agent_name {
            "recall_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("recall_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // Usage-ledger writeback (plan M1): record which memories
                // this completed recall surfaced. Local collection writes —
                // cheap enough to run inline, which also guarantees a
                // maintenance cycle right after a recall sees its usage.
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                    && let Err(err) = space.record_recall_usage(&conversation.messages).await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "recall usage ledger writeback failed: {err:?}"
                    );
                }
            }
            "maintenance_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("maintenance_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // A completed model call does not attest a consumed change page.
                // Per-Watch progress is retained by Nexus in WatchState.
                // Dream self-test (plan M7): after the sleep cycle ends, probe
                // whether recent memories are actually findable; failures
                // become review SleepTasks for the next cycle.
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                {
                    // Maintenance re-encodes and merges graph memory, so a
                    // probe miss cached before the cycle could now be
                    // answerable (plan M5 invalidation).
                    if let Err(err) = space.miss_cache.clear().await {
                        log::warn!(
                            target: "brain",
                            space_id = space.id;
                            "negative-knowledge cache clear after maintenance failed: {err:?}"
                        );
                    }
                    if space.automatic && !space.engine.is_cancelled() {
                        space.kick_memory_self_test();
                    }
                }
            }
            "formation_memory" => {
                let _ = self
                    .db
                    .set_extension_from_with("formation_usage".to_string(), |v| {
                        let mut usage: Usage = v.unwrap_or_default();
                        usage.accumulate(&conversation.usage);
                        Some(usage)
                    });
                // New memory can answer any past miss: drop the whole
                // negative-knowledge cache (plan M5 invalidation).
                if conversation.status == ConversationStatus::Completed
                    && let Some(space) = self.space()
                    && let Err(err) = space.miss_cache.clear().await
                {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "negative-knowledge cache clear failed: {err:?}"
                    );
                }
            }
            _ => {}
        }
    }

    async fn try_start_formation(&self) {
        let space = match self.space() {
            Some(space) => space,
            None => return,
        };

        if space.engine.is_cancelled() {
            return;
        }
        // A missing marker means nothing was processed yet; resume from the
        // beginning so conversations queued during maintenance are not stuck.
        let id = space.formation.get_processed().unwrap_or_default();
        if let Err(err) = space.restart_formation(SELF_USER_ID, id + 1).await {
            let reason = err.to_string();
            // "No pending ..." simply means no backlog. Anything else is a
            // transient handoff race; no retry — eviction-reload autostart or
            // the next ingest self-heals the queued backlog.
            if !reason.contains("No pending formation conversation") {
                log::warn!(
                    target: "brain",
                    space_id = space.id;
                    "formation resume failed: {reason}"
                );
            }
        }
        // Post-sleep digest: fold freshly committed wiki knowledge into the
        // graph while formation is quiet (PRD §7.3, Daydream cadence).
        #[cfg(feature = "wiki")]
        {
            if space.automatic {
                space.kick_wiki_digest();
                space.kick_wiki_housekeeping();
            }
        }
    }

    async fn try_start_maintenance(&self, formation_id: DocumentId) -> Option<DocumentId> {
        let space = match self.space() {
            Some(space) => space,
            None => return None,
        };

        if !space.automatic || space.engine.is_cancelled() {
            return None;
        }
        let at = space.maintenance.get_processed_at();
        let scope = if formation_id >= at.full + 168 {
            MaintenanceScope::Full
        } else if formation_id >= at.quick.max(at.full) + 42 {
            MaintenanceScope::Quick
        } else if formation_id >= at.daydream.max(at.quick).max(at.full) + 21 {
            MaintenanceScope::Daydream
        } else {
            return None;
        };

        let input = MaintenanceInput {
            trigger: "scheduled".to_string(),
            scope,
            timestamp: Some(rfc3339_datetime_now()),
            parameters: None,
            formation_id,
            // Filled by `Space::maintenance` once the settlement has run.
            assessment: None,
        };
        match space.maintenance(SELF_USER_ID, input).await {
            Ok(rt) => rt.conversation,
            Err(err) => {
                log::error!(target: "brain", formation_id; "scheduled maintenance failed to start: {}", err);
                None
            }
        }
    }
}
// grcov-excl-stop

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

/// Bulk decay is a weekly-rate process (the factor is documented per week in
/// BrainMaintenance.md); links decayed more recently than this are skipped,
/// so daily maintenance cannot over-decay.
const DECAY_MIN_INTERVAL_MS: u64 = 7 * 24 * 3_600 * 1_000;

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

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
use object_store::{ObjectStore, memory::InMemory};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
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
    /// Bounds requests that can each drive a full multi-turn LLM round.
    /// One budget for every channel: the HTTP LLM routes and the MCP LLM
    /// tools (recall/maintenance) drain this same semaphore, so neither
    /// channel can turn request concurrency into unbounded model spend.
    llm_semaphore: Arc<tokio::sync::Semaphore>,

    pub app_name: String,
    pub app_version: String,
    pub sharding: u32,
}

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
            object_store,
            db_config,
            management,
            http_client,
            models,
            ed25519_pubkeys,
            judge_model: Arc::new(None),
            llm_semaphore: Arc::new(tokio::sync::Semaphore::new(DEFAULT_LLM_MAX_CONCURRENCY)),
            app_name,
            app_version,
            sharding,
        }
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
    /// shared-formation eval forks): it owns the whole protocol — copy the
    /// objects, fork the state, and load without background autostart.
    pub async fn fork_space(
        &self,
        space_id: &str,
        policy: Option<MemoryPolicy>,
    ) -> Result<Arc<Space>, BoxError> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        copy_space_objects(&self.object_store(), &store, space_id).await?;
        let state = self.fork_with_store(store);
        // autostart=false: the fork inherits the live space's formation
        // cursor and wiki-digest backlog, and must NOT resume them — that
        // would burn real LLM tokens twice and mutate both forks mid-replay,
        // making the A/B comparison non-reproducible.
        let fork = state.load_space_with(space_id, false, false).await?;
        if let Some(policy) = policy {
            fork.db
                .set_extension_from(MemoryPolicy::EXTENSION_KEY.to_string(), policy);
        }
        Ok(fork)
    }

    /// On-demand shadow evaluation (plan M11): forks the space twice —
    /// current policy vs candidate policy — settles both forks, replays
    /// recent real recall queries on each, and lets the judge blind-compare
    /// the answers (deterministically alternating A/B order to cancel
    /// position bias). The live space is only read: replays run on forks,
    /// so they can never pollute its conversations, usage ledger, or
    /// metrics (plan guardrail 4). Promotion stays human: read the report,
    /// then `update_space` with the candidate policy if it won.
    pub(crate) async fn run_shadow_eval(
        &self,
        space_id: &str,
        input: ShadowEvalInput,
    ) -> Result<ShadowReport, BoxError> {
        input.policy.validate()?;
        // Unpinned: shadow evaluation must not exempt a cold space from idle
        // eviction forever.
        let space = self.load_space(space_id, false).await?;
        // One shadow evaluation per space at a time: each run holds two full
        // in-memory copies of the space, so concurrent retries would stack
        // copies until the process OOMs (and race the `shadow_report` write).
        let Ok(_shadow_guard) = space.shadow_lock.try_lock() else {
            return Err("a shadow evaluation is already running for this space".into());
        };
        let now_ms = unix_ms();
        let sample = input
            .replay_sample
            .unwrap_or_else(|| space.memory_policy().shadow_replay_sample as usize)
            .clamp(1, 16);

        let queries = space.recent_recall_queries(sample).await?;
        if queries.is_empty() {
            return Err("no completed recall conversations to replay".into());
        }

        // Flush the live space (collections included, not just metadata) so
        // the forks see its latest persisted state, then fork twice:
        // baseline keeps the current policy, candidate gets the proposed
        // one. The fork is still not a point-in-time snapshot — writes that
        // land mid-fork may appear on one side — but both sides replay the
        // same queries, so drift shows up as a tie, not a false win.
        // Settling both makes the comparison fair — same metabolism pass,
        // different knobs.
        space.flush().await.ok();
        let (baseline, candidate) = tokio::try_join!(
            self.fork_space(space_id, None),
            self.fork_space(space_id, Some(input.policy.clone())),
        )?;
        // Interval 0 bypasses the weekly decay gate: the forks inherit the
        // live space's `decay_applied_at` stamps, and under the gate both
        // sides would settle identically whenever the live space decayed
        // recently — turning every decay-knob comparison into a tie. Forks
        // are throwaway copies, so over-decaying them has no consequence.
        let _ = tokio::join!(
            baseline.settle_memory_metabolism_with(MaintenanceScope::Full, now_ms, 0),
            candidate.settle_memory_metabolism_with(MaintenanceScope::Full, now_ms, 0),
        );

        let mut report = ShadowReport {
            compared_at: now_ms,
            candidate_policy: input.policy,
            ..Default::default()
        };
        for (index, query) in queries.iter().enumerate() {
            let recall_input = || {
                StringOr::Value(RecallInput {
                    query: query.clone(),
                    context: None,
                })
            };
            let (baseline_out, candidate_out) = tokio::join!(
                baseline.query(SELF_USER_ID, recall_input()),
                candidate.query(SELF_USER_ID, recall_input()),
            );
            let (baseline_answer, candidate_answer) = match (baseline_out, candidate_out) {
                (Ok(baseline_out), Ok(candidate_out)) => {
                    report.usage.accumulate(&baseline_out.usage);
                    report.usage.accumulate(&candidate_out.usage);
                    (baseline_out.content, candidate_out.content)
                }
                _ => {
                    report.judge_errors += 1;
                    report.samples.push(ShadowSample {
                        query: crate::assess::truncate_chars(query, 200),
                        winner: "error".to_string(),
                        reason: "replay failed on one side".to_string(),
                    });
                    continue;
                }
            };
            report.replayed += 1;

            // Deterministic order alternation cancels position bias without
            // sacrificing reproducibility.
            let swap = index % 2 == 1;
            let (answer_a, answer_b) = if swap {
                (&candidate_answer, &baseline_answer)
            } else {
                (&baseline_answer, &candidate_answer)
            };
            let prompt = format!(
                "# User query\n{query}\n\n# Answer A\n{answer_a}\n\n# Answer B\n{answer_b}"
            );
            let verdict = crate::assess::AssessContext::judge_complete(
                space.as_ref(),
                anda_core::CompletionRequest {
                    instructions: SHADOW_JUDGE_INSTRUCTIONS.to_string(),
                    prompt,
                    effort: Some(anda_core::ModelEffort::Low),
                    ..Default::default()
                },
            )
            .await
            .and_then(|output| {
                report.usage.accumulate(&output.usage);
                crate::assess::parse_json_payload::<ShadowVerdict>(&output.content)
            });

            let (winner, reason) = match verdict {
                Ok(verdict) => {
                    let winner = match (verdict.winner.trim().to_lowercase().as_str(), swap) {
                        ("a", false) | ("b", true) => {
                            report.baseline_wins += 1;
                            "baseline"
                        }
                        ("b", false) | ("a", true) => {
                            report.candidate_wins += 1;
                            "candidate"
                        }
                        _ => {
                            report.ties += 1;
                            "tie"
                        }
                    };
                    (winner.to_string(), verdict.reason)
                }
                Err(err) => {
                    report.judge_errors += 1;
                    ("error".to_string(), err.to_string())
                }
            };
            report.samples.push(ShadowSample {
                query: crate::assess::truncate_chars(query, 200),
                winner,
                reason,
            });
        }

        // Forks live in memory and vanish on drop; closing is best-effort.
        let _ = baseline.close().await;
        let _ = candidate.close().await;

        space
            .db
            .set_extension_from("shadow_report".to_string(), report.clone());
        space.db.flush_metadata(unix_ms()).await.ok();
        Ok(report)
    }

    /// A sibling `AppState` over a different object store, sharing model and
    /// management configuration but with an empty space cache. Used by the
    /// eval harness to open forked space copies in isolation.
    pub fn fork_with_store(&self, object_store: Arc<dyn ObjectStore>) -> AppState {
        AppState {
            spaces: Arc::new(RwLock::new(BTreeMap::new())),
            object_store,
            db_config: self.db_config.clone(),
            http_client: self.http_client.clone(),
            models: self.models.clone(),
            judge_model: self.judge_model.clone(),
            llm_semaphore: self.llm_semaphore.clone(),
            ed25519_pubkeys: self.ed25519_pubkeys.clone(),
            management: self.management.clone(),
            app_name: self.app_name.clone(),
            app_version: self.app_version.clone(),
            sharding: self.sharding,
        }
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
    /// Note: `pinned` and `autostart` take effect only on the load that
    /// actually initializes the space; a cache hit returns the space as it
    /// was first opened and ignores both parameters.
    async fn load_space_with(
        &self,
        space_id: &str,
        pinned: bool,
        autostart: bool,
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
                    .or_insert_with(|| Arc::new(SpaceEntry::new()))
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

        entry.touch();
        // A Space idle for nine minutes is evicted, so the background pass
        // above never sees the quietest ones at all — the next time anybody
        // opens this Space is the only moment its overdue cycle can be
        // noticed. `autostart: false` forks are excluded: a shadow copy must
        // not burn model calls or mutate itself mid-replay.
        if autostart {
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

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
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
                _ = tokio::time::sleep(flush_interval) => {}
            }

            self.flush_and_evict_once(unix_ms(), idle_timeout_ms).await;
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
                if space.wiki_digest.is_processing() {
                    return false;
                }
                if space.pinned || space.is_processing() {
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

pub struct Space {
    id: String,
    engine: Engine,
    http_client: reqwest::Client,
    models: Arc<Models>,
    maintenance: Arc<MaintenanceAgent>,
    pinned: bool,
    /// Memory usage ledger (plan M1): off-graph recall/correction counters.
    ledger: Arc<UsageLedger>,
    /// Negative-knowledge cache (plan M5): probe queries the graph had
    /// nothing for; cleared whenever formation completes.
    miss_cache: Arc<MissCache>,
    /// Serializes memory-metabolism settlements (plan M2); the settlement
    /// itself is idempotent, the lock just avoids wasted duplicate passes.
    settlement_lock: tokio::sync::Mutex<()>,
    /// Bumped whenever this Space's vocabulary package is published — by the
    /// `declare_memory_symbols` tool or the wiki digest — so Recall's cached
    /// primer is refetched on the next read rather than at the end of its TTL.
    schema_generation: Arc<std::sync::atomic::AtomicU64>,
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
        self.formation.is_processing() || self.maintenance.is_processing()
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

    /// The space's memory policy; absent means the process-wide eval
    /// override (optimizer runs, plan M10) or [`MemoryPolicy::default`],
    /// which reproduces the compiled-in behavior (plan module M-P).
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
    ) -> Result<AgentOutput, BoxError> {
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
        self.engine
            .agent_run(
                user,
                AgentInput {
                    name: RecallAgent::NAME.to_string(),
                    prompt: input.to_string(),
                    resources: vec![],
                    ..Default::default()
                },
            )
            .await
    }

    pub async fn query(
        &self,
        user: Principal,
        input: StringOr<RecallInput>,
    ) -> Result<AgentOutput, BoxError> {
        let mut output = self.run_recall(user, input).await?;
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
        let output = self.run_recall(user, input).await?;
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
        if input.parameters.is_none() {
            input.parameters = Some(self.memory_policy().maintenance_parameters());
        }
        // Deterministic metabolism settles before the LLM cycle starts, so
        // the agent assesses an already-settled graph. Settlement failures
        // degrade the cycle, never abort it.
        match self.settle_memory_metabolism(input.scope, unix_ms()).await {
            Ok(report) => {
                log::info!(
                    target: "brain",
                    space_id = self.id,
                    report:serde = report;
                    "memory metabolism settled"
                );
            }
            Err(err) => {
                log::warn!(
                    target: "brain",
                    space_id = self.id;
                    "memory metabolism settlement failed: {err:?}"
                );
            }
        }
        // What the settlement just measured, handed to the cycle's assessment
        // phase. Overwritten rather than merged: like `formation_id`, this is
        // the runtime's account of its own graph, and a request body must not
        // be able to tell the Brain what its vocabulary looks like.
        input.assessment = Some(self.maintenance_assessment().await);
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
    async fn maintenance_assessment(&self) -> crate::types::MaintenanceAssessment {
        let audit: Option<SchemaAudit> = self.db.get_extension_as("schema_audit");
        crate::types::MaintenanceAssessment {
            audited_at: audit.as_ref().map(|audit| audit.audited_at),
            predicates: audit.map(|audit| audit.predicates).unwrap_or_default(),
            source_reliability: self
                .db
                .get_extension_as("source_reliability")
                .unwrap_or_default(),
            space_seq: self.current_space_seq().await,
            armed_watches: settlement::watches_in_status(self, "armed").await,
            fired_watches: settlement::watches_in_status(self, "fired").await,
            consumed_seq: self.db.get_extension_as(DELTA_CONSUMED_SEQ_KEY),
            revised_roots: self
                .memory_settlement()
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
        let _guard = self.settlement_lock.lock().await;
        let policy = self.memory_policy();
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

        let after: u64 = self.db.get_extension_as("correction_cursor").unwrap_or(0);
        let corrections = settlement::scan_corrections(self, after).await;
        report.correction_scan_error = corrections.error;
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
        if corrections.watermark > after {
            self.db
                .set_extension_from("correction_cursor".to_string(), corrections.watermark);
        }

        // Watch expiry, every scope. A deadline passing does not care how
        // expensive the cycle it landed in was, and a silence Watch that
        // waited for a `full` cycle would be a promise the Brain kept only
        // when it was already busy.
        // The head is what a structured Watch is evaluated through; the
        // consumption record is what a prose silence Watch waits on (§5.11).
        let head_seq = self.current_space_seq().await;
        let consumed_seq: Option<u64> = self.db.get_extension_as(DELTA_CONSUMED_SEQ_KEY);
        report.watches = settlement::sweep_watches(self, now_ms, head_seq, consumed_seq).await;
        if let Some(error) = &report.watches.error {
            log::error!(
                target: "brain",
                space_id = self.id;
                "watch expiry failed — silence Watches are NOT firing: {error}"
            );
        }

        // Skill lifecycle verdicts, every scope. Profile §14 rule 1 puts these
        // in deterministic code rather than in a prompt: the Brain proposes,
        // compiles and narrates; it never promotes.
        report.skills = settlement::settle_skills(self, now_ms).await;
        if let Some(error) = &report.skills.error {
            log::error!(
                target: "brain",
                space_id = self.id;
                "skill lifecycle pass failed — verdicts are NOT running: {error}"
            );
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
        if !kip::succeeded(&response) {
            return Err(format!("probe search failed: {}", kip::error_message(&response)).into());
        }
        let result = kip::ok_result(&response);
        // §66.6: a page is cut from a bounded candidate window that spans the
        // database while the search is Space-scoped, so an empty page is not
        // always an empty index. The engine says which, and this is the one
        // place the answer can be acted on — see the cache write below.
        let exhaustive = result
            .and_then(|result| result.get("search_context"))
            .and_then(|context| context.get("exhaustive"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let mut hits = result.map(assess::citations_from_json).unwrap_or_default();
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

    /// Privacy-grade deletion (plan M6): physically removes entities from
    /// the graph (concepts detach and take their propositions with them) and
    /// their usage-ledger rows. Archive does not satisfy forget. Run with
    /// `dry_run` first; per-entity errors (e.g. KIP_3004 protecting system
    /// nodes) do not abort the batch.
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
            let exists_command = if assess::is_proposition_entity_id(&entity) {
                "FIND(?link) WHERE { ?link (id: :id) } LIMIT 1"
            } else if assess::is_concept_entity_id(&entity) {
                "FIND(?c) WHERE { ?c {id: :id} } LIMIT 1"
            } else if assess::is_assertion_entity_id(&entity) {
                "FIND(?a) WHERE { ?a ASSERTION {id: :id} } LIMIT 1"
            } else {
                entry.error = Some("not an element id (C-*, P-* or A-*)".to_string());
                report.entities.push(entry);
                continue;
            };

            let response = self
                .execute_kip_readonly(kip::request_with(
                    exists_command,
                    kip::param("id", entity.as_str()),
                ))
                .await?;
            match kip::ok_result(&response) {
                Some(result) => {
                    entry.existed = assess::citations_from_json(result)
                        .iter()
                        .any(|hit| hit.entity == entity);
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

            // Deleting a concept DETACH-deletes all its propositions; their
            // ledger rows must cascade too (entity ids embed predicate names
            // like `P:7:has_allergy` — usage traces of a forgotten memory).
            // Enumerate them before the DELETE destroys the links.
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
        let removed = report.deleted_concepts + report.deleted_propositions;
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
    /// forget to cascade ledger rows for DETACH-deleted links. Best-effort:
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
        if self.is_processing() || !self.maintenance_overdue(unix_ms()) {
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

    /// Fires the dream self-test in the background (plan M7); called after a
    /// maintenance cycle completes. Skipped when disabled by policy or when
    /// a pass is already running.
    fn kick_memory_self_test(self: &Arc<Self>) {
        if self.memory_policy().self_test_queries_per_cycle == 0 {
            return;
        }
        let space = self.clone();
        tokio::spawn(async move {
            match space.run_memory_self_test(unix_ms()).await {
                Ok(Some(report)) => {
                    log::info!(
                        target: "brain",
                        space_id = space.id,
                        report:serde = report;
                        "memory self-test completed"
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        space_id = space.id;
                        "memory self-test failed: {err:?}"
                    );
                }
            }
        });
    }

    /// The dream self-test (plan M7): sample memories the brain has not probed
    /// yet, generate one natural query each (single LLM call), and check
    /// whether search actually surfaces them. Ungroundable memories become
    /// SleepTasks the next full maintenance re-encodes. Self-test retrievals
    /// count only into `self_test_count` — never into usage reinforcement.
    ///
    /// KIP 1.x stamped `metadata.self_tested_at` on each sampled link so the
    /// next pass would skip it. A Proposition is immutable and carries no
    /// metadata, so coverage is paced by a Space-sequence cursor instead: each
    /// pass reads the window after the last one and parks the cursor at its
    /// end, wrapping around once the horizon has passed. The bookkeeping that
    /// remains — which memories were tested, and how often — lives in the usage
    /// ledger, where it never touches cognition at all.
    ///
    /// Returns `None` when disabled, already running, or nothing qualifies.
    async fn run_memory_self_test(&self, now_ms: u64) -> Result<Option<SelfTestReport>, BoxError> {
        let Ok(_guard) = self.self_test_lock.try_lock() else {
            return Ok(None);
        };
        let policy = self.memory_policy();
        let budget = policy.self_test_queries_per_cycle as usize;
        if budget == 0 {
            return Ok(None);
        }

        // 1) Sample the next window of Propositions. Projecting the subject and
        // object variables returns the whole Concept — name and type included —
        // so a groundable query can be written without a second lookup per
        // endpoint, which is what the 1.x pass spent most of its round trips on.
        let cursor: SelfTestCursor = self
            .db
            .get_extension_as("memory_self_test_cursor")
            .unwrap_or_default();
        let window = budget * 4;
        let response = self
            .execute_kip_readonly(kip::request_with(
                format!(
                    r#"FIND(?p.id, ?p._system.space_seq, ?s, ?o)
WHERE {{
  ?p (?s, ?predicate, ?o)
  FILTER(?p._system.space_seq > :after)
}}
ORDER BY ?p._system.space_seq
LIMIT {window}"#
                ),
                kip::param("after", cursor.after),
            ))
            .await?;
        if !kip::succeeded(&response) {
            log::error!(
                target: "brain",
                space_id = self.id;
                "self-test sampling scan failed — dream self-test is NOT running \
                 (graph past the full-scan engine cap?): {}",
                kip::error_message(&response)
            );
            return Ok(None);
        }
        let sampled =
            self_test_candidates(kip::ok_result(&response).unwrap_or(&serde_json::Value::Null));

        // A short window means the cursor has reached the end of the graph.
        // Park it back at zero so coverage cycles — but only once the retest
        // horizon has passed, or a small graph would re-test the same handful
        // of memories on every cycle and burn its whole token budget doing it.
        let reached_end = sampled.len() < window;
        let next_after = sampled.last().map(|c| c.seq).unwrap_or(cursor.after);
        let wrap = reached_end && now_ms.saturating_sub(cursor.cycled_at) >= SELF_TEST_RETEST_MS;
        self.db.set_extension_from(
            "memory_self_test_cursor".to_string(),
            SelfTestCursor {
                after: if wrap { 0 } else { next_after },
                cycled_at: if wrap { now_ms } else { cursor.cycled_at },
            },
        );

        // Prefer memories with no usage evidence at all: recalled ones are
        // proven groundable, already-tested ones had their chance.
        let mut candidates = Vec::new();
        for candidate in sampled {
            let usage = self.ledger.get(&candidate.id).await?;
            if usage
                .as_ref()
                .is_none_or(|row| row.recall_count == 0 && row.self_test_count == 0)
            {
                candidates.push(candidate);
            }
            if candidates.len() >= budget {
                break;
            }
        }
        candidates.retain(|candidate| !candidate.subject_name.is_empty());
        if candidates.is_empty() {
            return Ok(None);
        }

        // 2) One LLM call generates all probe queries. The token budget is
        // enforced *before* the call by shrinking the candidate batch to fit
        // (≈3 chars per token, conservative); the knob bounds real spend
        // instead of warning after the fact.
        let max_prompt_chars = (policy.self_test_token_budget as usize).saturating_mul(3);
        while candidates.len() > 1
            && serde_json::to_string(&candidates)
                .map(|prompt| prompt.len() > max_prompt_chars)
                .unwrap_or(false)
        {
            candidates.pop();
        }
        let output = assess::AssessContext::complete(
            self,
            anda_core::CompletionRequest {
                instructions: SELF_TEST_INSTRUCTIONS.to_string(),
                prompt: serde_json::to_string_pretty(&candidates).unwrap_or_default(),
                effort: Some(anda_core::ModelEffort::Low),
                ..Default::default()
            },
        )
        .await?;
        let queries: SelfTestQueries = assess::parse_json_payload(&output.content)?;
        let mut report = SelfTestReport {
            tested_at: now_ms,
            usage: output.usage,
            ..Default::default()
        };
        let budget_tokens = report
            .usage
            .input_tokens
            .saturating_add(report.usage.output_tokens);
        if budget_tokens > policy.self_test_token_budget {
            log::warn!(
                target: "brain",
                space_id = self.id;
                "memory self-test used {budget_tokens} tokens, over policy budget {}",
                policy.self_test_token_budget
            );
        }

        // 3) Deterministic grounding check: does search surface the memory's
        // subject or object concept for the generated query?
        let mut tested_entities = BTreeSet::new();
        for candidate in &candidates {
            let Some(query) = queries
                .queries
                .iter()
                .find(|query| query.id == candidate.id)
                .map(|query| query.query.trim())
                .filter(|query| !query.is_empty())
            else {
                continue;
            };
            let response = self
                .execute_kip_readonly(kip::request_with(
                    "SEARCH CONCEPT :query LIMIT 8",
                    kip::param("query", query),
                ))
                .await?;
            let Some(result) = kip::ok_result(&response) else {
                // An errored/timed-out search is *unknown*, not
                // "ungroundable": counting it would fabricate a false
                // negative, lower the groundability metric, and burn a
                // re-encode task on a healthy memory. Abort the whole pass; a
                // later one re-samples from the same cursor.
                return Err(format!(
                    "self-test grounding search errored for {}: {}",
                    candidate.id,
                    kip::error_message(&response)
                )
                .into());
            };
            let mut hit_ids = BTreeSet::new();
            assess::collect_entity_objects(result, &mut |id, _| {
                hit_ids.insert(id.to_string());
            });
            report.tested += 1;
            tested_entities.insert(candidate.id.clone());
            if hit_ids.contains(&candidate.subject) || hit_ids.contains(&candidate.object) {
                report.grounded += 1;
                continue;
            }

            // Ungroundable: enqueue a SleepTask for the next cycle, unless one
            // is already pending for this concept.
            if self.has_pending_review_task(&candidate.subject).await? {
                continue;
            }
            match self
                .run_kip_settlement(self_test_task_request(candidate, query, now_ms))
                .await
            {
                Ok(response) if kip::succeeded(&response) => report.reencode_tasks += 1,
                Ok(response) => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "self-test SleepTask creation failed: {}",
                        kip::error_message(&response)
                    );
                }
                Err(err) => {
                    log::warn!(
                        target: "brain",
                        space_id = self.id;
                        "self-test SleepTask creation failed: {err:?}"
                    );
                }
            }
        }

        self.ledger
            .record_self_test(&tested_entities, now_ms)
            .await?;
        self.bump_metrics(|metrics| {
            metrics.self_test_tested += report.tested;
            metrics.self_test_grounded += report.grounded;
            metrics.reencode_tasks += report.reencode_tasks;
        });
        self.db
            .set_extension_from("memory_self_test".to_string(), report.clone());
        self.db.flush_metadata(now_ms).await.ok();
        Ok(Some(report))
    }

    /// True when a pending SleepTask already covers this Concept.
    async fn has_pending_review_task(&self, target: &str) -> Result<bool, BoxError> {
        let response = self
            .execute_kip_readonly(kip::request_with(
                r#"FIND(?task) WHERE {
  ?task CONCEPT {type: "SleepTask"}
  FILTER(?task.attributes.status == "pending")
  ?about CONCEPT {id: :target}
  STRUCTURAL (?task, "about", ?about)
} LIMIT 1"#,
                kip::param("target", target),
            ))
            .await?;
        // An error here means *unknown*, and the caller's fallback is to create
        // one more task than it needed — which the `key` on the upsert then
        // collapses anyway.
        Ok(kip::ok_result(&response).is_some_and(|result| {
            let mut found = false;
            assess::collect_entity_objects(result, &mut |_, _| found = true);
            found
        }))
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
        if rt.digested > 0 {
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
            // The digest may have grown the vocabulary package on its way
            // through; Recall's cached primer is stale from here.
            self.schema_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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
        tokio::spawn(async move {
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
        tokio::spawn(async move {
            match space.run_wiki_digest(SELF_USER_ID).await {
                Ok(report) if report.digested > 0 => {
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
    pub async fn execute_kip_readonly(&self, req: Request) -> Result<Response, BoxError> {
        let nexus = self.memory.nexus();
        match timeout(
            READONLY_KIP_TIMEOUT,
            kip::execute_readonly_request(nexus.as_ref(), &req),
        )
        .await
        {
            Ok(res) => Ok(res),
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
        self.db.close().await?;
        Ok(())
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

    async fn connect(
        object_store: Arc<dyn ObjectStore>,
        db_config: DBConfig,
        management: Arc<dyn Management>,
        http_client: reqwest::Client,
        models: Arc<Models>,
        pinned: bool,
        autostart: bool,
    ) -> Result<Arc<Self>, BoxError> {
        let id = db_config.name.clone();
        let db = Arc::new(AndaDB::open(object_store.clone(), db_config).await?);
        let nexus = CognitiveNexus::connect(db.clone()).await?;
        init_nexus_kip(&nexus).await?;
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
        let recall_store = Conversations::connect(db.clone(), "recall".to_string()).await?;
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
        let memory_r = TimedMemoryReadonly::new(memory.clone());
        let memory_tool = MemoryTool::new(memory.clone());
        let note_tool = NoteTool::new();
        // Formation and Maintenance may grow this Space's vocabulary; Recall
        // may not, and gets the tool nowhere.
        let schema_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let declare_tool = crate::vocabulary::DeclareSymbolsTool::new(memory.clone())
            .with_schema_generation(schema_generation.clone());

        let hooks = Arc::new(Hooks::new(db.clone()));
        let formation = Arc::new(FormationAgent::new(
            memory.clone(),
            conversations.clone(),
            hooks.clone(),
            100000,
        ));
        let recall = Arc::new(RecallAgent::new(
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
        .with_schema_generation(schema_generation.clone()));
        let maintenance = Arc::new(MaintenanceAgent::new(
            memory.clone(),
            maintenance_store,
            maintenance_conversations,
            hooks.clone(),
        ));
        // Build agent engine with all configured components
        #[allow(unused_mut)]
        let mut engine = Engine::builder()
            .with_management(management)
            .with_models(models.clone())
            // `execute_kip`, but Formation only reaches the cognition-only
            // subset through it; maintenance keeps the whole of KML. The raw
            // `memory` handle stays available to host code, which is
            // deterministic and not what the gate is for.
            .register_tool(Arc::new(GuardedMemory::new(memory.clone())))?
            .register_tool(Arc::new(memory_r))?
            .register_tool(Arc::new(memory_tool))?
            .register_tool(Arc::new(note_tool))?
            .register_tool(Arc::new(declare_tool))?;
        #[allow(unused_mut)]
        let mut exported_tools = vec![MemoryTool::NAME.to_string()];
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
            miss_cache,
            settlement_lock: tokio::sync::Mutex::new(()),
            schema_generation,
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
        });
        hooks.bind_space(Arc::downgrade(&this));

        if let Some(cfg) = db.get_extension_as::<ModelConfig>("byok") {
            let cfg: EngineModelConfig = cfg.into();
            if let Ok(model) = cfg.model(this.http_client.clone()) {
                this.models.set_model(model);
            } else {
                log::error!(target: "brain", space_id = this.id; "failed to initialize BYOK model from config: {:?}", cfg);
            }
        }

        if autostart {
            let this_clone = this.clone();
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
                // The cycle read the Change Stream through the coordinate it
                // was handed (`assessment.space_seq`, BrainMaintenance.md
                // §A.2): that is where the next cycle starts, and what a prose
                // silence Watch's deadline is measured against (§5.11). Only a
                // completed cycle counts — a failed one may have read nothing.
                if conversation.status == ConversationStatus::Completed
                    && let Some(seq) = consumed_seq_of(conversation)
                {
                    let _ = self.db.set_extension_from_with(
                        DELTA_CONSUMED_SEQ_KEY.to_string(),
                        |recorded: Option<u64>| Some(recorded.unwrap_or(0).max(seq)),
                    );
                }
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
                    space.kick_memory_self_test();
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
            space.kick_wiki_digest();
            space.kick_wiki_housekeeping();
        }
    }

    async fn try_start_maintenance(&self, formation_id: DocumentId) -> Option<DocumentId> {
        let space = match self.space() {
            Some(space) => space,
            None => return None,
        };

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
    /// One-shot completion on this space's model, used only by the eval
    /// harness (judge, user simulator, prompt optimizer). Not exposed over
    /// HTTP/MCP.
    pub(crate) async fn eval_complete(
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

/// Space extension: the coordinate the last completed maintenance cycle read
/// the Change Stream through (`assessment.space_seq` of that cycle).
///
/// Two readers. The next cycle starts its `CHANGES AFTER SEQ` here, and the
/// Watch sweep fires a prose silence Watch only once this has reached the head
/// at which it first saw the deadline passed (Profile §5.11).
const DELTA_CONSUMED_SEQ_KEY: &str = "delta_consumed_seq";

/// The coordinate a completed maintenance cycle consumed the stream through:
/// the `assessment.space_seq` in the input it was run on.
///
/// Read back off the conversation rather than threaded through the agent,
/// because the conversation is the durable record of what the cycle was
/// handed — a value carried beside it could disagree with it.
fn consumed_seq_of(conversation: &Conversation) -> Option<u64> {
    let first: Message = serde_json::from_value(conversation.messages.first()?.clone()).ok()?;
    let input: MaintenanceInput = serde_json::from_str(&first.text()?).ok()?;
    input.assessment?.space_seq
}

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

/// A memory self-tested longer ago than this becomes eligible for re-sampling,
/// so re-encoded memories eventually get their grounding re-verified.
const SELF_TEST_RETEST_MS: u64 = 30 * 24 * 3_600 * 1_000;

/// The `key` of the Concept this brain treats as its semantic self (§5.6).
///
/// A `key`, not a name: a key is immutable identity and a name is a mutable
/// label. Nothing seeds this Concept — `init_nexus_kip` deliberately creates no
/// Person — so a Space has one only where the wiki digest minted it as the
/// actor its extracted claims are attributed to.
pub(crate) const SELF_ACTOR_KEY: &str = "$self";

const SHADOW_JUDGE_INSTRUCTIONS: &str = r#"You compare two answers an AI memory system gave to the same user query under two different internal configurations. Pick the answer that better serves the user: correct use of remembered facts, honoring later corrections, honest uncertainty. Ignore style differences.

Respond with ONLY a JSON object: {"winner": "a" | "b" | "tie", "reason": "..."}"#;

#[derive(Debug, serde::Deserialize)]
struct ShadowVerdict {
    winner: String,
    #[serde(default)]
    reason: String,
}

/// This Space's memory policy: the stored one, an eval override, or the
/// compiled defaults.
///
/// A free function rather than only a [`Space`] method because the agents are
/// built before the `Space` that owns them, and a policy knob read through a
/// second copy of this fallback chain is a knob that eventually disagrees with
/// itself.
fn memory_policy_of(db: &AndaDB) -> MemoryPolicy {
    db.get_extension_as(MemoryPolicy::EXTENSION_KEY)
        .or_else(MemoryPolicy::eval_override)
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

/// Where the dream self-test's sampling window sits.
///
/// A Space sequence coordinate, not a timestamp: it is the same monotonic
/// counter the engine stamps on every element, so "the memories formed after
/// the ones I last looked at" is exact rather than approximate.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize, serde::Serialize)]
struct SelfTestCursor {
    /// The highest `_system.space_seq` a previous pass already sampled.
    after: u64,
    /// When the cursor last wrapped back to the start of the graph.
    cycled_at: u64,
}

/// One memory sampled for the dream self-test (plan M7); serialized as the
/// query-generation prompt.
#[derive(Debug, serde::Serialize)]
struct SelfTestCandidate {
    id: String,
    #[serde(skip)]
    seq: u64,
    subject: String,
    object: String,
    subject_type: String,
    subject_name: String,
    object_name: String,
}

/// Reads the self-test sampling rows —
/// `FIND(?p.id, ?p._system.space_seq, ?s, ?o)` — into candidates.
///
/// Projecting a bare element variable returns the whole rendered element, so
/// the subject's type and name arrive with the row. An object that is a literal
/// rather than an element contributes its text and no id, which is correct: a
/// literal cannot be what a `SEARCH CONCEPT` surfaces.
fn self_test_candidates(result: &serde_json::Value) -> Vec<SelfTestCandidate> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let columns = row.as_array()?;
            let id = columns.first().and_then(serde_json::Value::as_str)?;
            let seq = columns.get(1).and_then(serde_json::Value::as_u64)?;
            let subject = columns.get(2)?;
            let object = columns.get(3);
            Some(SelfTestCandidate {
                id: id.to_string(),
                seq,
                subject: element_field(subject, "id"),
                object: object.map(|o| element_field(o, "id")).unwrap_or_default(),
                subject_type: assess::local_symbol_name(&element_field(subject, "schema_ref"))
                    .to_string(),
                subject_name: element_field(subject, "name"),
                object_name: object.map(element_label).unwrap_or_default(),
            })
        })
        .collect()
}

/// Reads one string field out of a rendered element, or `""`.
fn element_field(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}


/// How a Proposition endpoint reads in a prompt: a Concept's name, or the
/// literal itself when the endpoint is one.
fn element_label(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(_) => element_field(value, "name"),
        other if other.is_null() => String::new(),
        other => other.to_string(),
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

#[derive(Debug, Default, serde::Deserialize)]
struct SelfTestQueries {
    #[serde(default)]
    queries: Vec<SelfTestQuery>,
}

#[derive(Debug, serde::Deserialize)]
struct SelfTestQuery {
    id: String,
    query: String,
}

const SELF_TEST_INSTRUCTIONS: &str = r#"You test the searchability of an AI's memory graph. You will receive a JSON array of memories, each a proposition with a subject, predicate, and object.

For each memory, write ONE short natural-language query a real user would plausibly ask that this memory should answer. Use the everyday words of the subject/object names — never internal ids, never the predicate name verbatim unless a user would say it.

Respond with ONLY a JSON object:
{"queries": [{"id": "<memory id>", "query": "..."}]}"#;

/// Self-test write: enqueue a SleepTask for a memory that search could not
/// surface, pointed at its subject Concept — re-encoding (aliases, a richer
/// description, links to neighbouring memory) happens at the Concept level.
///
/// The task is keyed by the Concept it is about, so a memory that stays
/// ungroundable across several passes accumulates one task rather than one per
/// pass, and `UPSERT` refreshes the existing one instead of colliding with it.
/// `assigned_to` is deliberately absent: KIP 1.x assigned these to a `$system`
/// Person, and the Profile is explicit that semantic assignment — to `$system`
/// least of all — grants no Principal any permission.
fn self_test_task_request(candidate: &SelfTestCandidate, query: &str, now_ms: u64) -> Request {
    let summary = format!(
        "memory self-test: the query {query:?} did not surface `{}` ({}) via search; re-encode the \
         Concept with aliases, a richer description, or links to neighbouring memory so it becomes \
         findable",
        candidate.subject_name, candidate.id
    );
    let parameters = serde_json::Map::from_iter([
        (
            "key".to_string(),
            serde_json::Value::from(format!("self_test:{}", candidate.subject)),
        ),
        (
            "name".to_string(),
            serde_json::Value::from(format!("Re-encode {}", candidate.subject_name)),
        ),
        ("summary".to_string(), serde_json::Value::from(summary)),
        (
            "created_at".to_string(),
            serde_json::Value::from(kip::timestamp(now_ms)),
        ),
        (
            "target".to_string(),
            serde_json::Value::from(candidate.subject.clone()),
        ),
    ]);
    kip::request_with(
        r#"UPSERT CONCEPT ?task {
  MATCH { type: "SleepTask", key: :key }
  SET FIELDS { name: :name }
  SET ATTRIBUTES {
    task_class: "consolidate",
    summary: :summary,
    status: "pending",
    priority: 2,
    created_at: :created_at
  }
  SET STRUCTURAL { ("about", :target) }
}"#,
        parameters,
    )
}

/// Copies every object of a space (`{space_id}/**`) from one object store to
/// another, preserving paths. This is the eval fork primitive: AndaDB
/// metadata embeds its own base path, so a space must keep its id and be
/// forked into a *different* store — never renamed inside the same store.
async fn copy_space_objects(
    src: &Arc<dyn ObjectStore>,
    dst: &Arc<dyn ObjectStore>,
    space_id: &str,
) -> Result<u64, BoxError> {
    use futures::TryStreamExt;
    use object_store::ObjectStoreExt;

    let prefix = object_store::path::Path::from(space_id);
    let mut objects = src.list(Some(&prefix));
    let mut copied = 0u64;
    while let Some(meta) = objects.try_next().await? {
        let payload = src.get(&meta.location).await?.bytes().await?;
        dst.put(&meta.location, payload.into()).await?;
        copied += 1;
    }
    if copied == 0 {
        return Err(format!("space {space_id} has no objects to copy").into());
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, Hooks, MAINTENANCE_MAX_INTERVAL_MS, Space, SpaceEntry,
        init_conversation_collection, init_resource_collection,
    };
    use crate::settlement;
    use crate::{
        agents::{BrainHook, FormationAgent, MaintenanceAgent, SELF_USER_ID, TimedMemoryReadonly},
        kip,
        payload::StringOr,
        testkit::{app_state_core, create_loaded_space, signed_token, signing_key},
        types::{
            AddSpaceTokenInput, FormationInput, InputContext, MaintenanceInput,
            MaintenanceParameters, MaintenanceScope, MemoryPolicy, ModelConfig, RecallInput,
            SpaceTier, SpaceToken, TokenScope, UpdateSpaceInput,
        },
    };
    use anda_core::{
        AgentOutput, BoxError, BoxPinFut, CompletionRequest, Message, Principal, Resource, Tool,
        Usage,
    };
    use anda_db::collection::CollectionConfig;
    use anda_engine::{
        context::BaseCtx,
        memory::{Conversation, ConversationRef, ConversationStatus, KipArgs, MemoryReadonly},
        model::{CompletionFeaturesDyn, Model, Models},
        unix_ms,
    };
    use ic_cose_types::cose::ed25519::{SigningKey, VerifyingKey};
    use object_store::memory::InMemory;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::time::{Duration, sleep};
    use tokio_util::sync::CancellationToken;

    #[derive(Debug)]
    struct FinalCompleter;

    impl CompletionFeaturesDyn for FinalCompleter {
        fn model_name(&self) -> String {
            "final-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("processed: {}", req.prompt).into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    /// Answers the self-test query-generation call: the first candidate (by
    /// id order) gets a query matching its subject name, the second gets
    /// unfindable gibberish — one grounded, one not.
    #[derive(Debug)]
    struct SelfTestCompleter;

    impl CompletionFeaturesDyn for SelfTestCompleter {
        fn model_name(&self) -> String {
            "self-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let candidates: Vec<serde_json::Value> =
                    serde_json::from_str(&req.prompt).unwrap_or_default();
                let mut ids: Vec<(String, String)> = candidates
                    .iter()
                    .filter_map(|candidate| {
                        Some((
                            candidate.get("id")?.as_str()?.to_string(),
                            candidate.get("subject_name")?.as_str()?.to_string(),
                        ))
                    })
                    .collect();
                ids.sort();
                let queries: Vec<serde_json::Value> = ids
                    .iter()
                    .enumerate()
                    .map(|(index, (id, subject_name))| {
                        let query = if index == 0 {
                            subject_name.clone()
                        } else {
                            "qqqzzzxxx nonsense".to_string()
                        };
                        serde_json::json!({"id": id, "query": query})
                    })
                    .collect();
                Ok(AgentOutput {
                    content: serde_json::json!({ "queries": queries }).to_string(),
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 10,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    fn test_app_state_with_self_test_model(name: &str) -> AppState {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(SelfTestCompleter)));
        test_app_state_with_models(name, Arc::new(models))
    }

    #[derive(Debug)]
    struct SlowCompleter;

    impl CompletionFeaturesDyn for SlowCompleter {
        fn model_name(&self) -> String {
            "slow-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                sleep(Duration::from_millis(150)).await;
                Ok(AgentOutput {
                    content: "slow done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("slow processed: {}", req.prompt).into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    fn test_app_state(name: &str) -> AppState {
        test_app_state_with_models(name, Arc::new(Models::default()))
    }

    fn test_app_state_with_final_model(name: &str) -> AppState {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(FinalCompleter)));
        test_app_state_with_models(name, Arc::new(models))
    }

    fn test_app_state_with_slow_model(name: &str) -> AppState {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(SlowCompleter)));
        test_app_state_with_models(name, Arc::new(models))
    }

    fn test_app_state_with_pubkeys(name: &str) -> AppState {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        let key = VerifyingKey::from_bytes(&bytes).unwrap();
        app_state_core(name, Arc::new(Models::default()), vec![key], "test", 0)
    }

    fn test_app_state_with_signing_key(name: &str, signing_key: &SigningKey) -> AppState {
        app_state_core(
            name,
            Arc::new(Models::default()),
            vec![signing_key.verifying_key()],
            "test",
            0,
        )
    }

    fn test_app_state_with_models(name: &str, models: Arc<Models>) -> AppState {
        app_state_core(name, models, vec![], "test", 0)
    }

    async fn wait_until_idle(space: &Space) {
        for _ in 0..100 {
            if !space.is_processing() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("space did not become idle");
    }

    #[tokio::test]
    async fn copy_space_objects_forks_space_into_isolated_store() {
        let app = test_app_state("fork_src");
        let space = create_loaded_space(&app, "fork_space").await;
        space
            .update(
                UpdateSpaceInput {
                    name: Some("before fork".to_string()),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        space.db.close().await.unwrap();

        let fork_store: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        let copied = super::copy_space_objects(&app.object_store(), &fork_store, "fork_space")
            .await
            .unwrap();
        assert!(copied > 0);

        // Copying a missing space fails loudly instead of forking nothing.
        let empty: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        assert!(
            super::copy_space_objects(&app.object_store(), &empty, "missing_space")
                .await
                .is_err()
        );

        // The fork opens under the same id in its own store, sees the same
        // state, and mutations do not leak back to the source store.
        let fork_state = app.fork_with_store(fork_store);
        let fork = fork_state.load_space("fork_space", true).await.unwrap();
        assert_eq!(fork.get_info().name.as_deref(), Some("before fork"));
        fork.update(
            UpdateSpaceInput {
                name: Some("after fork".to_string()),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();
        fork.db.close().await.unwrap();

        let fork_store2: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
        super::copy_space_objects(&app.object_store(), &fork_store2, "fork_space")
            .await
            .unwrap();
        let fork_state2 = app.fork_with_store(fork_store2);
        let fork2 = fork_state2.load_space("fork_space", true).await.unwrap();
        assert_eq!(fork2.get_info().name.as_deref(), Some("before fork"));
        fork2.db.close().await.unwrap();
    }

    #[test]
    fn space_entry_starts_uninitialized_with_recent_access_time() {
        let before = unix_ms();
        let entry = SpaceEntry::new();
        let after = unix_ms();

        assert!(!entry.cell.initialized());
        assert!(entry.last_access_ms() >= before);
        assert!(entry.last_access_ms() <= after);
    }

    #[test]
    fn space_entry_touch_refreshes_last_access_time() {
        let entry = SpaceEntry::new();
        entry.last_access_ms.store(0, Ordering::Relaxed);
        let before_touch = unix_ms();

        entry.touch();

        assert!(entry.last_access_ms() >= before_touch);
    }

    #[tokio::test]
    async fn create_space_persists_metadata_before_returning() {
        let object_store = Arc::new(InMemory::new());
        let db_config = crate::testkit::db_config("create_space_persists_metadata");
        let creator = Principal::from_slice(&[1]);
        let owner = Principal::from_slice(&[2]);

        let info = Space::create(
            object_store.clone(),
            db_config.clone(),
            creator,
            owner,
            1,
            123,
        )
        .await
        .unwrap();

        assert_eq!(info.owner, owner.to_string());
        assert_eq!(info.tier.tier, 1);

        let db = anda_db::database::AndaDB::open(object_store, db_config)
            .await
            .unwrap();
        let persisted_owner: String = db.get_extension_as("owner").unwrap();
        let persisted_tier: SpaceTier = db.get_extension_as("tier").unwrap();

        assert_eq!(persisted_owner, owner.to_string());
        assert_eq!(persisted_tier.tier, 1);

        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn collection_bootstrap_helpers_create_and_prune_indexes() {
        let object_store = Arc::new(InMemory::new());
        let db_config = crate::testkit::db_config("collection_bootstrap_helpers");
        let db = anda_db::database::AndaDB::create(object_store, db_config)
            .await
            .unwrap();
        let mut conversation_schema = Conversation::schema().unwrap();
        conversation_schema.with_version(4);

        let conversations = db
            .open_or_create_collection(
                conversation_schema,
                CollectionConfig {
                    name: "conversations".to_string(),
                    description: "conversations collection".to_string(),
                },
                async |collection| {
                    collection.create_btree_index_nx(&["thread"]).await?;
                    collection.create_btree_index_nx(&["period"]).await?;
                    collection
                        .create_bm25_index_nx(&["messages", "resources", "artifacts"])
                        .await?;
                    init_conversation_collection(collection).await
                },
            )
            .await
            .unwrap();
        let meta = conversations.metadata();
        assert!(meta.btree_indexes.contains_key("user"));
        assert!(!meta.btree_indexes.contains_key("thread"));
        assert!(!meta.btree_indexes.contains_key("period"));
        assert!(
            !meta
                .bm25_indexes
                .contains_key("messages-resources-artifacts")
        );

        let resources = db
            .open_or_create_collection(
                Resource::schema().unwrap(),
                CollectionConfig {
                    name: "resources".to_string(),
                    description: "Resources collection".to_string(),
                },
                async |collection| {
                    collection
                        .create_bm25_index_nx(&["name", "description", "metadata"])
                        .await?;
                    init_resource_collection(collection).await
                },
            )
            .await
            .unwrap();
        let meta = resources.metadata();
        assert!(meta.btree_indexes.contains_key("tags"));
        assert!(meta.btree_indexes.contains_key("hash"));
        assert!(meta.btree_indexes.contains_key("mime_type"));
        assert!(!meta.bm25_indexes.contains_key("name-description-metadata"));

        db.close().await.unwrap();
    }

    /// `Space::connect` opens every conversation collection itself, then hands
    /// the same names to `MemoryManagement::connect` / `Conversations::connect`
    /// so the engine wrappers adopt the already-open handles. If they ever
    /// re-ran their own bootstrap instead, they would recreate exactly the
    /// indexes `init_conversation_collection` drops.
    #[tokio::test]
    async fn connected_space_keeps_the_trimmed_conversation_index_layout() {
        let app = test_app_state("space_index_layout");
        let space = create_loaded_space(&app, "space_index_layout").await;

        for collection in [
            &space.conversations,
            &space.recall.conversations_collection,
            &space.maintenance.conversations_collection,
        ] {
            let meta = collection.metadata();
            assert!(meta.btree_indexes.contains_key("user"));
            assert!(!meta.btree_indexes.contains_key("thread"));
            assert!(!meta.btree_indexes.contains_key("period"));
            assert!(
                !meta
                    .bm25_indexes
                    .contains_key("messages-resources-artifacts")
            );
        }
    }

    #[test]
    fn app_state_allows_local_auth_when_no_pubkeys_are_configured() {
        let app = test_app_state("local_auth");
        let now_ms = 123;

        let admin = app
            .check_admin("", "space", TokenScope::Write, now_ms)
            .unwrap();
        assert_eq!(admin.user, Principal::management_canister());
        assert_eq!(admin.audience, "space");
        assert_eq!(admin.scope, TokenScope::Write);

        let user = app
            .check_auth("", "space", TokenScope::Read, now_ms)
            .unwrap();
        assert_eq!(user.user, SELF_USER_ID);

        let optional = app
            .check_auth_if("", "space", TokenScope::Read, now_ms)
            .unwrap()
            .unwrap();
        assert_eq!(optional.user, SELF_USER_ID);
    }

    #[test]
    fn app_state_rejects_invalid_tokens_when_pubkeys_are_configured() {
        let app = test_app_state_with_pubkeys("configured_auth");
        let now_ms = 123;

        assert!(
            app.check_auth_if("short", "space", TokenScope::Read, now_ms)
                .unwrap()
                .is_none()
        );
        assert!(
            app.check_auth("not-base64", "space", TokenScope::Read, now_ms)
                .is_err()
        );
        assert!(
            app.check_admin("not-base64", "space", TokenScope::Write, now_ms)
                .is_err()
        );
    }

    #[test]
    fn app_state_accepts_valid_signed_tokens_and_rejects_scope_mismatches() {
        let signing_key = signing_key(7);
        let app = test_app_state_with_signing_key("signed_auth", &signing_key);
        let now_ms = 1_725_000_000_000;

        let read_token = signed_token(&signing_key, SELF_USER_ID, "space-a", "read");
        let auth = app
            .check_auth(&read_token, "space-a", TokenScope::Read, now_ms)
            .unwrap();
        assert_eq!(auth.user, SELF_USER_ID);
        assert_eq!(auth.audience, "space-a");
        assert_eq!(auth.scope, TokenScope::Read);
        assert!(
            app.check_auth(&read_token, "space-a", TokenScope::Write, now_ms)
                .err()
                .unwrap()
                .to_string()
                .contains("insufficient scope")
        );
        assert!(
            app.check_auth(&read_token, "space-b", TokenScope::Read, now_ms)
                .err()
                .unwrap()
                .to_string()
                .contains("invalid audience")
        );

        let admin_token = signed_token(&signing_key, SELF_USER_ID, "*", "*");
        let admin = app
            .check_admin(&admin_token, "any-space", TokenScope::Write, now_ms)
            .unwrap();
        assert_eq!(admin.user, SELF_USER_ID);
        assert_eq!(admin.scope, TokenScope::All);

        let optional = app
            .check_auth_if(&admin_token, "any-space", TokenScope::Read, now_ms)
            .unwrap()
            .unwrap();
        assert_eq!(optional.audience, "*");

        let non_admin = signed_token(&signing_key, Principal::from_slice(&[99]), "*", "*");
        assert!(
            app.check_admin(&non_admin, "any-space", TokenScope::Read, now_ms)
                .err()
                .unwrap()
                .to_string()
                .contains("admin access required")
        );
    }

    #[tokio::test]
    async fn app_state_loads_spaces_once_and_rejects_duplicate_loaded_space() {
        let app = test_app_state("load_cache");
        let id = "load_cache_space";
        let owner = Principal::from_slice(&[3]);

        let info = app
            .admin_create_space(Principal::from_slice(&[1]), owner, id.to_string(), 2, 456)
            .await
            .unwrap();
        assert_eq!(info.id, id);
        assert_eq!(info.owner, owner.to_string());

        let loaded = app.load_space(id, false).await.unwrap();
        let loaded_again = app.load_space(id, false).await.unwrap();
        assert!(Arc::ptr_eq(&loaded, &loaded_again));

        let err = app
            .admin_create_space(Principal::from_slice(&[1]), owner, id.to_string(), 2, 456)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn app_state_background_shutdown_and_idle_eviction_paths() {
        let app = test_app_state("background_eviction");
        let space_id = "background_eviction_space";
        let space = create_loaded_space(&app, space_id).await;

        let cancel = CancellationToken::new();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), app.start_background_tasks(cancel))
            .await
            .unwrap();

        let entry = {
            let spaces = app.spaces.read().await;
            spaces.get(space_id).unwrap().clone()
        };
        app.flush_and_evict_once(unix_ms(), 10_000).await;
        assert!(app.spaces.read().await.contains_key(space_id));

        entry.last_access_ms.store(0, Ordering::Relaxed);
        assert!(!app.try_evict_idle_space(space_id, &entry, 10_000, 1).await);

        let wrong_entry = Arc::new(SpaceEntry::new());
        assert!(
            !app.try_evict_idle_space(space_id, &wrong_entry, 10_000, 1)
                .await
        );

        drop(space);
        for _ in 0..100 {
            let space_refs = entry.cell.get().map(Arc::strong_count).unwrap_or_default();
            if space_refs == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        drop(entry);
        app.flush_and_evict_once(10_000, 1).await;
        assert!(!app.spaces.read().await.contains_key(space_id));

        let missing_entry = Arc::new(SpaceEntry::new());
        assert!(
            !app.try_evict_idle_space("missing_space", &missing_entry, 10_000, 1)
                .await
        );

        assert!(app.load_space("never_created_space", false).await.is_err());
        let uninitialized = {
            let spaces = app.spaces.read().await;
            spaces.get("never_created_space").unwrap().clone()
        };
        assert!(
            !app.try_evict_idle_space("never_created_space", &uninitialized, 10_000, 1)
                .await
        );
    }

    #[tokio::test]
    async fn flush_and_evict_removes_idle_uninitialized_placeholders() {
        let app = test_app_state("placeholder_eviction");
        assert!(app.load_space("placeholder_space", false).await.is_err());
        {
            let spaces = app.spaces.read().await;
            let entry = spaces.get("placeholder_space").unwrap();
            assert!(!entry.cell.initialized());
        }

        // Not idle yet: the placeholder entry is kept for retrying.
        app.flush_and_evict_once(unix_ms(), 10_000).await;
        assert!(app.spaces.read().await.contains_key("placeholder_space"));

        // Idle: the placeholder is dropped so probes for unknown space IDs
        // cannot grow the map unboundedly.
        app.flush_and_evict_once(unix_ms() + 20_000, 10_000).await;
        assert!(!app.spaces.read().await.contains_key("placeholder_space"));
    }

    #[tokio::test]
    async fn space_metadata_tier_byok_and_tokens_roundtrip() {
        let app = test_app_state("space_metadata");
        let space = create_loaded_space(&app, "space_metadata").await;

        let tier = space.admin_update_tier(3, 999).await.unwrap();
        assert_eq!(tier.tier, 3);
        assert_eq!(space.get_tier().tier, 3);

        space
            .update(
                UpdateSpaceInput {
                    name: Some("Research Brain".to_string()),
                    description: Some("memory space".to_string()),
                    public: Some(true),
                    ..Default::default()
                },
                1000,
            )
            .await
            .unwrap();
        assert!(space.is_public());

        let info = space.get_info();
        assert_eq!(info.name.as_deref(), Some("Research Brain"));
        assert_eq!(info.description.as_deref(), Some("memory space"));
        assert_eq!(info.tier.tier, 3);

        let byok = ModelConfig {
            family: "openai".to_string(),
            model: "gpt-test".to_string(),
            api_base: "https://api.example.test".to_string(),
            api_key: "test-key".to_string(),
            ..Default::default()
        };
        space.update_byok(byok.clone()).await.unwrap();
        assert_eq!(space.get_byok().unwrap().model, byok.model);

        let disabled_byok = ModelConfig {
            family: "openai".to_string(),
            model: "disabled-test".to_string(),
            api_base: "https://api.example.test".to_string(),
            api_key: "test-key".to_string(),
            disabled: true,
            ..Default::default()
        };
        let err = space.update_byok(disabled_byok).await.unwrap_err();
        assert!(err.to_string().contains("model is disabled"));
        assert_eq!(space.get_byok().unwrap().model, byok.model);

        let token = "STtest-token".to_string();
        let st = space
            .add_space_token(
                token.clone(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "reader".to_string(),
                    expires_at: Some(2000),
                    labels: None,
                },
                1100,
            )
            .await
            .unwrap();
        assert_eq!(st.scope, TokenScope::Read);
        assert_eq!(st.name, "reader");

        space
            .verify_space_token(token.clone(), TokenScope::Read, 1200)
            .unwrap();
        assert!(
            space
                .verify_space_token(token.clone(), TokenScope::Write, 1200)
                .is_err()
        );
        assert!(
            space
                .verify_space_token(token.clone(), TokenScope::Read, 2500)
                .is_err()
        );

        let tokens = space.list_space_tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        // The listing redacts the credential to a display prefix.
        assert_eq!(tokens[0].token, "STtest-t…");
        assert_eq!(tokens[0].usage, 1);

        // Revocation works by name (the listing no longer echoes values)…
        assert!(space.revoke_space_token_by_name("reader").await.unwrap());
        assert!(!space.revoke_space_token_by_name("reader").await.unwrap());
        // …and by full token value.
        let st2 = space
            .add_space_token(
                "STtest-token".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "reader".to_string(),
                    expires_at: None,
                    labels: None,
                },
                1300,
            )
            .await
            .unwrap();
        assert_eq!(st2.token, "STtest-token");
        assert!(space.revoke_space_token("STtest-token").await.unwrap());
        assert!(!space.revoke_space_token("STtest-token").await.unwrap());

        // Platform-managed extensions must not be deletable through the
        // space-token revoke API.
        assert!(space.revoke_space_token("tier").await.is_err());
        assert_eq!(space.get_tier().tier, 3);
        assert!(space.revoke_space_token("byok").await.is_err());
        assert!(space.get_byok().is_some());

        space
            .update(
                UpdateSpaceInput {
                    ..Default::default()
                },
                3000,
            )
            .await
            .unwrap();
        assert!(space.get_byok().is_some());
    }

    #[tokio::test]
    async fn labeled_space_tokens_are_read_only_wiki_viewers() {
        let app = test_app_state("labeled_tokens");
        let space = create_loaded_space(&app, "labeled_tokens").await;

        // Labels are trimmed and deduped.
        let st = space
            .add_space_token(
                "STlabeled".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "auditor".to_string(),
                    expires_at: None,
                    labels: Some(vec![" hr ".to_string(), "hr".to_string(), " ".to_string()]),
                },
                1000,
            )
            .await
            .unwrap();
        assert_eq!(st.labels, Some(vec!["hr".to_string()]));

        // Labels with a write-capable scope are rejected at creation: they
        // would allow committing to / exporting documents behind labels the
        // token cannot read (launch review P0-2/P1-1).
        for (idx, scope) in [TokenScope::Write, TokenScope::All].into_iter().enumerate() {
            let err = space
                .add_space_token(
                    format!("STw{idx}"),
                    AddSpaceTokenInput {
                        scope,
                        name: format!("writer{idx}"),
                        expires_at: None,
                        labels: Some(vec!["hr".to_string()]),
                    },
                    1000,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("read scope"), "{err}");
        }

        // A legacy labeled row carrying a write scope fails closed at verify
        // but still works as the read-only viewer it was meant to be.
        let legacy = SpaceToken {
            token: "STlegacy".to_string(),
            scope: TokenScope::All,
            name: "legacy".to_string(),
            labels: Some(vec!["hr".to_string()]),
            ..Default::default()
        };
        space
            .db
            .save_extension_from("STlegacy".to_string(), &legacy.to_ref())
            .await
            .unwrap();
        assert!(
            space
                .verify_space_token("STlegacy".to_string(), TokenScope::All, 2000)
                .is_err()
        );
        assert!(
            space
                .verify_space_token("STlegacy".to_string(), TokenScope::Write, 2000)
                .is_err()
        );
        assert!(
            space
                .verify_space_token("STlegacy".to_string(), TokenScope::Read, 2000)
                .is_ok()
        );

        // Token names are audit identities (`st:{name}`): duplicates would
        // make two tokens indistinguishable in the event log.
        let err = space
            .add_space_token(
                "STdup".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "auditor".to_string(),
                    expires_at: None,
                    labels: None,
                },
                1000,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[tokio::test]
    async fn memory_policy_round_trips_and_rejects_invalid_values() {
        let app = test_app_state("memory_policy");
        let space = create_loaded_space(&app, "memory_policy").await;

        // Absent policy means defaults (compiled-in behavior).
        assert_eq!(space.memory_policy(), MemoryPolicy::default());

        let policy = MemoryPolicy {
            memory_strength_decay_factor: 0.9,
            orphan_max_count: 5,
            ..Default::default()
        };
        space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(policy.clone()),
                    ..Default::default()
                },
                1000,
            )
            .await
            .unwrap();
        assert_eq!(space.memory_policy(), policy);

        // Invalid values reject the update and leave the stored policy alone.
        let invalid = MemoryPolicy {
            memory_strength_decay_factor: 0.0,
            ..Default::default()
        };
        let err = space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(invalid),
                    ..Default::default()
                },
                1001,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("memory_strength_decay_factor"));
        assert_eq!(space.memory_policy(), policy);

        // Budget knobs are capped, not just floored: this object is settable
        // over HTTP, and an unbounded self-test budget is a cost bomb.
        let bomb = MemoryPolicy {
            self_test_queries_per_cycle: u32::MAX,
            ..Default::default()
        };
        let err = space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(bomb),
                    ..Default::default()
                },
                1002,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("self_test_queries_per_cycle"));
        assert_eq!(space.memory_policy(), policy);
    }

    #[tokio::test]
    async fn maintenance_fills_parameters_from_memory_policy() {
        let app = test_app_state_with_slow_model("maintenance_policy_params");
        let space = create_loaded_space(&app, "maintenance_policy_params").await;
        space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(MemoryPolicy {
                        unconsolidated_max_backlog: 42,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                1000,
            )
            .await
            .unwrap();

        let output = space
            .maintenance(SELF_USER_ID, MaintenanceInput::default())
            .await
            .unwrap();
        let conversation = space
            .get_conversation(
                Some("maintenance".to_string()),
                output.conversation.unwrap(),
            )
            .await
            .unwrap();
        let encoded = serde_json::to_string(&conversation.messages).unwrap();
        // The prompt is the pretty-printed MaintenanceInput JSON, escaped
        // inside the stored message text.
        assert!(encoded.contains("\\\"unconsolidated_max_backlog\\\": 42"));
        assert!(encoded.contains("\\\"memory_strength_decay_factor\\\": 0.95"));
    }

    #[tokio::test]
    async fn maintenance_keeps_explicit_parameters() {
        let app = test_app_state_with_slow_model("maintenance_explicit_params");
        let space = create_loaded_space(&app, "maintenance_explicit_params").await;

        let input = MaintenanceInput {
            parameters: Some(MaintenanceParameters {
                stale_event_threshold_days: Some(3),
                memory_strength_decay_factor: None,
                unconsolidated_max_backlog: None,
                orphan_max_count: None,
            }),
            ..Default::default()
        };
        let output = space.maintenance(SELF_USER_ID, input).await.unwrap();
        let conversation = space
            .get_conversation(
                Some("maintenance".to_string()),
                output.conversation.unwrap(),
            )
            .await
            .unwrap();
        let encoded = serde_json::to_string(&conversation.messages).unwrap();
        assert!(encoded.contains("\\\"stale_event_threshold_days\\\": 3"));
        // The policy must not overwrite explicit parameters.
        assert!(!encoded.contains("memory_strength_decay_factor"));
    }

    #[tokio::test]
    async fn usage_ledger_counts_recalls_and_corrections_without_touching_the_graph() {
        let app = test_app_state("usage_ledger");
        let space = create_loaded_space(&app, "usage_ledger").await;

        let entities =
            std::collections::BTreeSet::from(["P:1:prefers".to_string(), "C:9".to_string()]);
        space.ledger.record_recall(&entities, 100).await.unwrap();
        space
            .ledger
            .record_recall(
                &std::collections::BTreeSet::from(["P:1:prefers".to_string()]),
                200,
            )
            .await
            .unwrap();

        let row = space.ledger.get("P:1:prefers").await.unwrap().unwrap();
        assert_eq!(row.recall_count, 2);
        assert_eq!(row.last_recalled_at, 200);
        assert_eq!(
            space.ledger.get("C:9").await.unwrap().unwrap().recall_count,
            1
        );

        // Corrections record once per entity.
        assert!(
            space
                .ledger
                .record_correction("P:1:prefers", 300)
                .await
                .unwrap()
        );
        assert!(
            !space
                .ledger
                .record_correction("P:1:prefers", 400)
                .await
                .unwrap()
        );
        let row = space.ledger.get("P:1:prefers").await.unwrap().unwrap();
        assert_eq!(row.correction_count, 1);
        assert_eq!(row.last_corrected_at, 300);

        // The counts stay in the ledger and reach the graph through nothing:
        // the entities recalled above carry no `MnemonicState` at all, which
        // is what "reading does not reinforce" has to look like from the
        // graph's side.
        assert_eq!(mnemonic_state(&space, "C:9").await, serde_json::Value::Null);
    }

    /// The gate is only worth anything if the engine really does tell the
    /// tool which agent called it. `GuardedMemory` branches on
    /// `BaseCtx::agent`, so this drives the real dispatch path — an agent
    /// context, its `child_base` tool context — rather than the predicate,
    /// which `kip.rs` already covers on its own.
    ///
    /// Both halves, because the divergence this closes was a default-open
    /// `else`: Formation was gated by name and every other agent fell through
    /// to the raw tool. Maintenance being admitted where Formation is refused
    /// proves the branch; Maintenance being refused where nobody is admitted
    /// proves it is a branch and not a bypass.
    #[tokio::test(flavor = "multi_thread")]
    async fn each_writing_agent_reaches_its_own_half_of_the_gate() {
        use anda_core::Tool;
        use anda_engine::memory::KipArgs;

        let app = test_app_state("formation_gate");
        let space = create_loaded_space(&app, "formation_gate").await;
        let guarded = crate::agents::GuardedMemory::new(space.memory.clone());

        // Concepts to aim at, so a refusal cannot be confused with a miss.
        seed_kip(
            &space,
            kip::request(
                r#"MUTATE {
  UPSERT CONCEPT ?p { MATCH {type: "Person", key: "victim"} SET FIELDS {name: "Victim"} }
  UPSERT CONCEPT ?b { MATCH {type: "Person", key: "bystander"} SET FIELDS {name: "Bystander"} }
}"#,
            ),
        )
        .await;

        const ARCHIVE_VICTIM: &str = r#"TRANSITION ?c TO "archived"
WHERE { ?c CONCEPT {type: "Person", key: "victim"} } LIMIT 1"#;
        let archive = || KipArgs {
            command: Some(ARCHIVE_VICTIM.to_string()),
            ..Default::default()
        };

        // Formation: refused on what the command parses to.
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let out = guarded
            .call(ctx.child_base("execute_kip").unwrap(), archive(), vec![])
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        let refusal = kip::error_message(&out.output);
        assert!(refusal.contains("archived"), "{refusal}");

        // Formation's own writes still go through the same tool.
        let write = guarded
            .call(
                ctx.child_base("execute_kip").unwrap(),
                KipArgs {
                    command: Some(
                        r#"MUTATE { CREATE ACTIVITY ?a { SET FIELDS { activity_class: "extraction", status: "completed" } } }"#
                            .to_string(),
                    ),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(write.is_error, None, "{:?}", write.output);

        // Maintenance: the same tool, the same command, admitted.
        let ctx = space
            .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
            .unwrap();
        let out = guarded
            .call(ctx.child_base("execute_kip").unwrap(), archive(), vec![])
            .await
            .unwrap();
        assert_eq!(out.is_error, None, "{:?}", out.output);
        assert_eq!(
            kip::transitioned(&out.output, "archived"),
            1,
            "{:?}",
            out.output
        );

        // ... and still refused the two things no plan gets: erasure, and a
        // hold that would block somebody else's. Aimed at a Concept the
        // archive above did not touch, so the survival check below reads a
        // live element rather than an archived one.
        for (command, expected) in [
            (
                r#"PURGE ?c WHERE { ?c CONCEPT {type: "Person", key: "bystander"} } LIMIT 1 CONFIRM "PURGE""#,
                "PURGE",
            ),
            (
                r#"SET RETENTION ?c { retention_class: "standard", legal_hold: true } WHERE { ?c CONCEPT {type: "Person", key: "bystander"} } LIMIT 1"#,
                "legal hold",
            ),
        ] {
            let out = guarded
                .call(
                    ctx.child_base("execute_kip").unwrap(),
                    KipArgs {
                        command: Some(command.to_string()),
                        ..Default::default()
                    },
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(out.is_error, Some(true), "{command}");
            let refusal = kip::error_message(&out.output);
            assert!(refusal.contains(expected), "{command}: {refusal}");
        }

        // The Concept the purge aimed at is still there, so the refusal was a
        // refusal and not a failed erasure.
        let survivor = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: "bystander"} } LIMIT 1"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            kip::ok_result(&survivor)
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1),
            "{survivor:?}"
        );
    }

    /// The runtime's copy of what was said is the copy that gets stored.
    ///
    /// Spec §71.1 exists because a model retyping an observation into a
    /// `payload` truncates it, normalizes its whitespace, fixes its spelling or
    /// paraphrases it, and the record then says the source said something they
    /// did not (§88.12). The command below never contains the sentence — it
    /// cites `:msg1` — so finding the sentence verbatim proves it did not pass
    /// through model-generated text on the way in.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_runtime_mints_the_observation_the_model_only_cites() {
        use anda_core::Tool;
        use anda_engine::memory::KipArgs;

        let app = test_app_state("formation_ingest");
        let space = create_loaded_space(&app, "formation_ingest").await;
        let guarded = crate::agents::GuardedMemory::new(space.memory.clone());

        let said = "Please keep answers concise — I mean it, ≤ 3 sentences.";
        let messages = vec![anda_core::Message {
            role: "user".to_string(),
            content: vec![said.to_string().into()],
            ..Default::default()
        }];
        let observation =
            kip::observation_ingest(&messages, "2026-08-20T00:00:00Z", "formation:chat-42", None)
                .expect("one message, one entry");

        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        ctx.base
            .set_state(crate::agents::Observation(Some(Arc::new(observation))));

        let plan = || KipArgs {
            command: Some(
                r#"MUTATE {
  UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} SET FIELDS {name: "Alice"} }
  CREATE CONCEPT ?concise {
    TYPE "Preference"
    NAME "Alice concise answers"
    SET ATTRIBUTES {preference_class: "communication"}
  }
  ASSERT ?a (?alice, "prefers", ?concise) {
    by: ?alice, mode: "stated", confidence: 0.95, evidence: :msg1
  }
}"#
                .to_string(),
            ),
            ..Default::default()
        };
        let written = guarded
            .call(ctx.child_base("execute_kip").unwrap(), plan(), vec![])
            .await
            .unwrap();
        assert_eq!(written.is_error, None, "{:?}", written.output);

        let stored = |space: Arc<Space>| async move {
            let response = space
                .execute_kip_readonly(kip::request(
                    "FIND(?e.payload, ?e.evidence_class, ?e.observed_at) WHERE { ?e EVIDENCE {} } \
                     LIMIT 5",
                ))
                .await
                .unwrap();
            kip::ok_result(&response)
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let rows = stored(space.clone()).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0],
            serde_json::json!([
                // The whole message, role included — who said a thing is part
                // of what was observed — and byte for byte, em dash and `≤`
                // intact.
                {"mode": "inline", "inline": serde_json::to_value(&messages[0]).unwrap()},
                // From the speaker's role, never from anything a model chose.
                "user_statement",
                "2026-08-20T00:00:00.000Z",
            ])
        );

        // The same pass writing again resolves to the record it already minted
        // rather than observing the same sentence twice: the `client_key` is
        // what makes attaching this to every request in a multi-turn formation
        // safe (§52.1).
        let again = guarded
            .call(ctx.child_base("execute_kip").unwrap(), plan(), vec![])
            .await
            .unwrap();
        assert_eq!(again.is_error, None, "{:?}", again.output);
        assert_eq!(stored(space.clone()).await.len(), 1);
    }

    /// The lifecycle end to end, against a real graph: compile a Skill, feed
    /// its family graded Outcome Evidence, and watch deterministic code —
    /// never a model — move it. Profile §14 rule 1: "The Brain proposes,
    /// compiles, and narrates; it never promotes."
    #[tokio::test]
    async fn a_skill_is_promoted_by_its_outcome_stream_and_never_by_assertion() {
        let app = test_app_state("skill_lifecycle");
        let space = create_loaded_space(&app, "skill_lifecycle").await;
        let now_ms = unix_ms();

        seed_kip(
            &space,
            kip::request(
                r#"MUTATE {
  UPSERT CONCEPT ?s {
    MATCH { type: "Skill", key: "redeploy" }
    SET FIELDS { name: "Redeploy after a schema change" }
    SET ATTRIBUTES {
      skill_class: "recovery",
      task_family: "deploy",
      summary: "check the migration target before redeploying",
      procedure: "1. verify the active database target 2. redeploy",
      status: "proposed"
    }
  }
}"#,
            ),
        )
        .await;

        // The gate below has to name the Skill it applied, and a Structural
        // Reference names an element, never a key.
        let skill_id = {
            let response = space
                .execute_kip_readonly(kip::request(
                    r#"FIND(?s.id) WHERE { ?s CONCEPT {type: "Skill", key: "redeploy"} } LIMIT 1"#,
                ))
                .await
                .unwrap();
            let rows = kip::ok_result(&response).cloned().unwrap_or_default();
            let row = rows
                .as_array()
                .and_then(|rows| rows.first())
                .cloned()
                .unwrap_or_default();
            // A single-projection row may come back bare or as a one-column
            // array depending on the shape the engine chose; take either.
            let id = match row.as_array().and_then(|columns| columns.first()) {
                Some(value) => value.clone(),
                None => row,
            };
            id.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| panic!("the seeded Skill has an id: {rows}"))
        };

        // One graded run, attributed the way Profile §8.1 requires: a gate
        // that names the Skill it applied, and an instrument's observation
        // that names the gate and the outcome it wrote. Without both hops the
        // outcome belongs to the family's baseline and grades nothing.
        async fn grade(space: &Space, skill_id: &str, outcomes: &[(&str, f64)]) {
            for (status, magnitude) in outcomes {
                seed_kip(
                    space,
                    kip::request_with(
                        r#"MUTATE {
  CREATE ACTIVITY ?gate {
    SET FIELDS { activity_class: "action_gate", status: "completed" }
    SET FACET "DecisionRecord" { decision: "act", rationale: "redeploy under the compiled Skill" }
    SET STRUCTURAL { ("inputs", :skill) }
  }
  CREATE EVIDENCE ?e {
    SET FIELDS {
      evidence_class: "outcome",
      payload: {instrument: "ci", run: "x"},
      observed_at: "2026-08-31T00:00:00Z"
    }
    SET FACET "OutcomeRecord" {
      task_family: "deploy",
      outcome_status: :status,
      magnitude: :magnitude
    }
  }
  CREATE ACTIVITY ?obs {
    SET FIELDS { activity_class: "outcome_observation", status: "completed" }
    SET STRUCTURAL { ("inputs", ?gate) ("outputs", ?e) }
  }
}"#,
                        serde_json::Map::from_iter([
                            ("skill".to_string(), serde_json::Value::from(skill_id)),
                            ("status".to_string(), serde_json::Value::from(*status)),
                            ("magnitude".to_string(), serde_json::Value::from(*magnitude)),
                        ]),
                    ),
                )
                .await;
            }
        }

        /// An outcome in the same family that nobody attributed to the Skill:
        /// it belongs to the baseline and must never move a lifecycle.
        async fn unattributed(space: &Space, outcomes: &[(&str, f64)]) {
            for (status, magnitude) in outcomes {
                seed_kip(
                    space,
                    kip::request_with(
                        r#"MUTATE {
  CREATE EVIDENCE ?e {
    SET FIELDS {
      evidence_class: "outcome",
      payload: {instrument: "ci", run: "other"},
      observed_at: "2026-08-31T00:00:00Z"
    }
    SET FACET "OutcomeRecord" {
      task_family: "deploy",
      outcome_status: :status,
      magnitude: :magnitude
    }
  }
}"#,
                        serde_json::Map::from_iter([
                            ("status".to_string(), serde_json::Value::from(*status)),
                            ("magnitude".to_string(), serde_json::Value::from(*magnitude)),
                        ]),
                    ),
                )
                .await;
            }
        }

        async fn standing(space: &Space) -> (String, f64, u64) {
            let response = space
                .execute_kip_readonly(kip::request(
                    r#"FIND(?s.attributes.status, ?s.facets["MnemonicState"].utility, ?s.facets["GradingState"].graded_count)
WHERE { ?s CONCEPT {type: "Skill", key: "redeploy"} } LIMIT 1"#,
                ))
                .await
                .unwrap();
            let rows = kip::ok_result(&response).cloned().unwrap_or_default();
            let row = rows
                .as_array()
                .and_then(|r| r.first())
                .cloned()
                .unwrap_or_default();
            let columns = row.as_array().cloned().unwrap_or_default();
            (
                columns
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                columns
                    .get(1)
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                columns
                    .get(2)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            )
        }

        /// The trial's recorded baseline rate and how many runs it counted.
        async fn trial_state(space: &Space) -> Option<(f64, u64)> {
            let response = space
                .execute_kip_readonly(kip::request(
                    r#"FIND(?s.facets["TrialState"])
WHERE { ?s CONCEPT {type: "Skill", key: "redeploy"} } LIMIT 1"#,
                ))
                .await
                .unwrap();
            let rows = kip::ok_result(&response).cloned().unwrap_or_default();
            let row = rows.as_array()?.first()?.clone();
            let facet = match row.as_array().and_then(|columns| columns.first()) {
                Some(value) => value.clone(),
                None => row,
            };
            let count = |name: &str| facet.get(name).and_then(serde_json::Value::as_u64);
            let success = count("baseline_success_count")?;
            let failure = count("baseline_failure_count")?;
            let graded = count("baseline_graded_count")?;
            let decided = success + failure;
            (decided > 0).then(|| (success as f64 / decided as f64, graded))
        }

        // The family was already running at 1 success in 4 before anybody
        // tried this Skill. Those runs are nobody's treatment set — they are
        // what "how things were going" means (§6.5).
        unattributed(
            &space,
            &[
                ("success", 0.2),
                ("failure", 0.2),
                ("failure", 0.2),
                ("failure", 0.2),
            ],
        )
        .await;

        // Runs nobody attributed to the Skill grade nothing: rule 7 makes the
        // link the only path from an outcome to a tally.
        let report = settlement::settle_skills(space.as_ref(), now_ms).await;
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!(report.graded, 0, "{report:?}");
        assert_eq!(standing(&space).await.0, "proposed");

        // A poor first showing opens the trial and records the baseline it
        // will later be judged against — the family's 1 of 4, so 0.250.
        grade(
            &space,
            &skill_id,
            &[("success", 0.5), ("failure", 0.2), ("failure", 0.2)],
        )
        .await;
        let report = settlement::settle_skills(space.as_ref(), now_ms).await;
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!(report.transitions, 1, "{report:?}");
        assert_eq!(standing(&space).await.0, "trialed");
        assert_eq!(
            trial_state(&space).await,
            Some((0.25, 4)),
            "the baseline is the family without this Skill's own runs"
        );

        // The stream then does better than that baseline, over enough runs.
        grade(&space, &skill_id, &[("success", 0.5); 6]).await;
        let report = settlement::settle_skills(space.as_ref(), now_ms).await;
        assert_eq!(report.transitions, 1, "{report:?}");
        let (status, utility, graded) = standing(&space).await;
        assert_eq!(status, "adopted");
        assert_eq!(graded, 9, "every graded outcome counted exactly once");
        assert!((utility - 7.0 / 9.0).abs() < 1e-9, "utility {utility}");

        // Idempotent: the cursor advanced, so a replayed pass grades nothing
        // and cannot promote on arithmetic instead of evidence.
        let report = settlement::settle_skills(space.as_ref(), now_ms).await;
        assert_eq!(report.graded, 0, "{report:?}");
        assert_eq!(standing(&space).await.1, utility);

        // One severe matching-condition failure revokes an adopted Skill
        // without waiting for a re-verdict — the Profile's one sanctioned
        // asymmetry, and it favours demotion.
        grade(&space, &skill_id, &[("failure", 0.95)]).await;
        let report = settlement::settle_skills(space.as_ref(), now_ms).await;
        assert_eq!(report.transitions, 1, "{report:?}");
        assert_eq!(standing(&space).await.0, "revoked");

        // Every move left a recomputable verdict behind: Profile §9 wants the
        // linked Evidence as `inputs`, the Skill it moved as `outputs`, the
        // rule identity pinned in `parameters_digest`, and the basis on the
        // Skill's own `TrialState` — so an auditor can re-run it from state.
        let activities = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.parameters_digest) WHERE { ?a ACTIVITY {activity_class: "lifecycle_verdict"} } LIMIT 20"#,
            ))
            .await
            .unwrap();
        let digests: Vec<String> =
            serde_json::from_value(kip::ok_result(&activities).cloned().unwrap_or_default())
                .unwrap_or_default();
        assert_eq!(digests.len(), 3, "{digests:?}");
        for digest in &digests {
            assert!(
                digest.contains(crate::settlement::skill::VERDICT_RULE),
                "{digest}"
            );
            assert!(digest.contains("window=("), "{digest}");
            assert!(digest.contains("basis_seq="), "{digest}");
            assert!(digest.contains("baseline="), "{digest}");
        }

        // The Evidence is what the verdict read; the Skill is what it moved.
        // The other way round would read as though the Skill caused the runs
        // that graded it.
        let cited = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.inputs, ?a.outputs) WHERE { ?a ACTIVITY {activity_class: "lifecycle_verdict"} } LIMIT 20"#,
            ))
            .await
            .unwrap();
        let rows = kip::ok_result(&cited).cloned().unwrap_or_default();
        let rows = rows.as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 3, "{rows:?}");
        let ids = |value: &serde_json::Value| -> Vec<String> {
            value
                .as_array()
                .map(|refs| {
                    refs.iter()
                        .filter_map(|r| r.get("id").and_then(|id| id.as_str()))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut graded_evidence = Vec::new();
        for row in &rows {
            let columns = row.as_array().unwrap();
            let inputs = ids(&columns[0]);
            assert!(
                inputs.iter().all(|id| id.starts_with("E-")),
                "inputs are the graded Outcome Evidence: {inputs:?}"
            );
            assert!(!inputs.is_empty(), "a verdict cites the outcomes it read");
            graded_evidence.extend(inputs);
            assert_eq!(
                ids(&columns[1]),
                vec!["C-1".to_string()],
                "outputs are the Skill whose lifecycle it moved"
            );
        }
        // Each graded outcome is cited by exactly one verdict: the cursor is
        // what stops a replay from counting the same run twice.
        let unique: std::collections::BTreeSet<_> = graded_evidence.iter().collect();
        assert_eq!(unique.len(), graded_evidence.len(), "{graded_evidence:?}");
        assert_eq!(graded_evidence.len(), 10);
    }

    /// The status of one Watch, by key.
    async fn watch_status(space: &Space, key: &str) -> String {
        watch_attribute(space, key, "status").await
    }

    /// One attribute of one Watch, as text.
    async fn watch_attribute(space: &Space, key: &str, attribute: &str) -> String {
        let response = space
            .execute_kip_readonly(kip::request_with(
                format!(
                    r#"FIND(?w.attributes.{attribute}) WHERE {{ ?w CONCEPT {{type: "Watch", key: :key}} }} LIMIT 1"#
                ),
                kip::param("key", key),
            ))
            .await
            .unwrap();
        kip::ok_result(&response)
            .and_then(serde_json::Value::as_array)
            .and_then(|rows| rows.first())
            .map(crate::types::attribute_text)
            .unwrap_or_default()
    }

    /// Arms one Watch.
    async fn arm_watch(space: &Space, key: &str, class: &str, condition: serde_json::Value, due: &str) {
        seed_kip(
            space,
            kip::request_with(
                r#"MUTATE {
  UPSERT CONCEPT ?w {
    MATCH { type: "Watch", key: :key }
    SET FIELDS { name: :key }
    SET ATTRIBUTES {
      watch_class: :class,
      summary: "escalate if nothing lands",
      condition: :condition,
      status: "armed",
      due_at: :due
    }
  }
}"#,
                serde_json::Map::from_iter([
                    ("key".to_string(), serde_json::Value::from(key)),
                    ("class".to_string(), serde_json::Value::from(class)),
                    ("condition".to_string(), condition),
                    ("due".to_string(), serde_json::Value::from(due)),
                ]),
            ),
        )
        .await;
    }

    /// The `activity_class` of every Activity in the Space.
    async fn activity_classes(space: &Space) -> Vec<String> {
        let activities = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.activity_class) WHERE { ?a ACTIVITY {} } LIMIT 20"#,
            ))
            .await
            .unwrap();
        serde_json::from_value(kip::ok_result(&activities).cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    /// A prose silence Watch fires on its deadline only once the Brain has
    /// consumed the Change Stream through the head at which the sweep first
    /// saw the deadline passed (Profile §5.11). The clock alone proves
    /// nothing: a matching change committed before the deadline may still be
    /// waiting for the model whose job it is to decide whether a change
    /// matched a condition written in prose.
    #[tokio::test]
    async fn a_silence_watch_fires_when_its_deadline_passes() {
        let app = test_app_state("watch_expiry");
        let space = create_loaded_space(&app, "watch_expiry").await;
        let now_ms = unix_ms();
        let past = kip::timestamp(now_ms.saturating_sub(86_400_000));
        let future = kip::timestamp(now_ms.saturating_add(86_400_000));
        let prose = serde_json::Value::from("no reply from the vendor");

        arm_watch(&space, "overdue", "silence", prose.clone(), &past).await;
        arm_watch(&space, "not_yet", "silence", prose.clone(), &future).await;
        // A delta Watch has no deadline semantics at all: it waits for a
        // matching change, and in prose only the model can say what matches.
        arm_watch(&space, "delta", "delta", prose, &past).await;

        // First sight of the passed deadline: the head is recorded on the
        // Watch and nothing fires.
        let head = space.current_space_seq().await;
        assert!(head.is_some());
        let report = settlement::sweep_watches(space.as_ref(), now_ms, head, None).await;
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!((report.fired, report.deferred), (0, 1), "{report:?}");
        assert_eq!(watch_status(&space, "overdue").await, "armed");
        let seen: u64 = watch_attribute(&space, "overdue", "due_seen_seq")
            .await
            .parse()
            .unwrap();
        assert_eq!(Some(seen), head);

        // The Brain has read the stream only up to before that head: held.
        let report = settlement::sweep_watches(
            space.as_ref(),
            now_ms,
            space.current_space_seq().await,
            Some(seen - 1),
        )
        .await;
        assert_eq!((report.fired, report.deferred), (0, 1), "{report:?}");
        assert_eq!(watch_status(&space, "overdue").await, "armed");

        // Consumed through it: silence is a fact, and the Watch fires.
        let consumed = space.current_space_seq().await;
        let report = settlement::sweep_watches(space.as_ref(), now_ms, consumed, consumed).await;
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!((report.fired, report.deferred), (1, 0), "{report:?}");
        assert_eq!(watch_status(&space, "overdue").await, "fired");
        assert_eq!(watch_status(&space, "not_yet").await, "armed");
        assert_eq!(watch_status(&space, "delta").await, "armed");

        // Firing wrote its own provenance, and nothing else. An `action_gate`
        // here would be the runtime inventing a decision — act, ask, defer and
        // silence are all judgements about what the deadline means.
        assert_eq!(
            activity_classes(&space).await,
            vec!["watch_fire".to_string()]
        );

        // Idempotent: the fired Watch is no longer armed, so a second sweep
        // finds nothing and cannot fire it twice.
        let consumed = space.current_space_seq().await;
        let again = settlement::sweep_watches(space.as_ref(), now_ms, consumed, consumed).await;
        assert_eq!((again.fired, again.deferred), (0, 0), "{again:?}");

        // The decision is still outstanding, and the next cycle is handed the
        // queue. Firing produced attention; what it means is the action gate's,
        // and the gate is cognition.
        let assessment = space.maintenance_assessment().await;
        let fired: Vec<&str> = assessment
            .fired_watches
            .iter()
            .map(|watch| watch.name.as_str())
            .collect();
        assert_eq!(fired, ["overdue"], "{assessment:?}");
        let armed: Vec<&str> = assessment
            .armed_watches
            .iter()
            .map(|watch| watch.name.as_str())
            .collect();
        assert_eq!(armed.len(), 2, "{armed:?}");
    }

    /// A structured condition is the runtime's to evaluate (Profile §5.11):
    /// the sweep reads the Change Stream from where each Watch was last
    /// evaluated and matches the entries, so a delta Watch fires on the
    /// change it watches, a silence Watch whose change arrived stands down,
    /// and a silence Watch past its deadline with nothing matched fires in
    /// the same sweep — silence concluded over a consumed stream.
    #[tokio::test]
    async fn a_structured_watch_is_evaluated_against_the_change_stream() {
        let app = test_app_state("watch_structured");
        let space = create_loaded_space(&app, "watch_structured").await;
        let now_ms = unix_ms();
        let past = kip::timestamp(now_ms.saturating_sub(86_400_000));

        seed_kip(
            &space,
            kip::request(
                r#"MUTATE { UPSERT CONCEPT ?p { MATCH { type: "Person", key: "vendor" } SET FIELDS { name: "Vendor" } } }"#,
            ),
        )
        .await;
        let vendor = {
            let response = space
                .execute_kip_readonly(kip::request(
                    r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: "vendor"} } LIMIT 1"#,
                ))
                .await
                .unwrap();
            serde_json::from_value::<Vec<String>>(kip::ok_result(&response).cloned().unwrap())
                .unwrap()
                .remove(0)
        };

        // Armed now, so only changes committed after arming count.
        arm_watch(&space, "profile", "delta", serde_json::json!({"element": vendor}), "").await;
        arm_watch(
            &space,
            "reply",
            "silence",
            serde_json::json!({"slot": {"subject": vendor, "predicate": "prefers"}}),
            &past,
        )
        .await;
        arm_watch(
            &space,
            "quiet",
            "silence",
            serde_json::json!({"element": vendor, "ops": ["lifecycle"]}),
            &past,
        )
        .await;

        // The vendor's profile changes, and a claim lands in the watched slot.
        seed_kip(
            &space,
            kip::request_with(
                r#"MUTATE {
  UPDATE :vendor_id SET FIELDS { name: "Vendor Inc" }
  UPSERT CONCEPT ?channel { MATCH { type: "Preference", key: "email" } SET FIELDS { name: "email" } }
  ASSERT (:vendor, "prefers", ?channel) { by: :vendor, mode: "stated" }
}"#,
                serde_json::Map::from_iter([
                    ("vendor_id".to_string(), serde_json::Value::from(vendor.as_str())),
                    ("vendor".to_string(), serde_json::json!({"id": vendor})),
                ]),
            ),
        )
        .await;

        let head = space.current_space_seq().await;
        let report = settlement::sweep_watches(space.as_ref(), now_ms, head, None).await;
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!(
            (report.fired, report.disarmed, report.deferred, report.conflicted),
            (2, 1, 0, 0),
            "{report:?}"
        );
        assert_eq!(watch_status(&space, "profile").await, "fired");
        assert_eq!(watch_status(&space, "reply").await, "disarmed");
        assert_eq!(watch_status(&space, "quiet").await, "fired");
        // A delta fire names the change it fired on; a stand-down names the
        // change that answered the silence.
        assert!(!watch_attribute(&space, "profile", "matched_seq").await.is_empty());
        assert!(!watch_attribute(&space, "reply", "matched_seq").await.is_empty());
        // Silence was concluded over a stream consumed through the head.
        assert_eq!(
            watch_attribute(&space, "quiet", "evaluated_seq").await.parse::<u64>().ok(),
            head
        );

        // Two fires, two `watch_fire` Activities; standing down is not a fire.
        assert_eq!(
            activity_classes(&space).await,
            vec!["watch_fire".to_string(), "watch_fire".to_string()]
        );

        // Nothing armed is left, so the next sweep has nothing to evaluate.
        let head = space.current_space_seq().await;
        let again = settlement::sweep_watches(space.as_ref(), now_ms, head, None).await;
        assert_eq!((again.fired, again.disarmed, again.deferred), (0, 0, 0), "{again:?}");
    }

    /// Maintenance was reachable only by counting formation conversations, so
    /// a Space that stopped ingesting stopped metabolizing: no Commitment
    /// review, no retention expiry, no self-test. The reference policy's
    /// triggers are "scheduled, threshold, or change-driven".
    #[tokio::test]
    async fn maintenance_comes_due_on_the_clock_not_only_on_traffic() {
        let app = test_app_state("maintenance_clock");
        let space = create_loaded_space(&app, "maintenance_clock").await;
        let now_ms = unix_ms();

        // A Space with nothing in it is never overdue: firing a cycle at every
        // freshly created Space would spend a model call to learn there is
        // nothing to consolidate.
        assert!(!space.maintenance_overdue(now_ms));

        // Formed something, never maintained: due now, whatever the count of
        // conversations says.
        space.formation.set_processed_for_test(1).await;
        assert!(space.maintenance_overdue(now_ms));

        // Maintained just now: not due again until the interval passes.
        space.maintenance.set_start_at(now_ms).await.unwrap();
        assert!(!space.maintenance_overdue(now_ms));
        assert!(!space.maintenance_overdue(now_ms + MAINTENANCE_MAX_INTERVAL_MS - 1));
        assert!(space.maintenance_overdue(now_ms + MAINTENANCE_MAX_INTERVAL_MS));
    }

    async fn seed_kip(space: &Space, request: anda_kip::Request) {
        let response = space.run_kip_settlement(request).await.unwrap();
        assert!(
            kip::succeeded(&response),
            "seed failed: {}",
            kip::error_message(&response)
        );
    }

    /// One Concept's `MnemonicState`, or `Json::Null` when it has none.
    async fn mnemonic_state(space: &Space, id: &str) -> serde_json::Value {
        let response = space
            .execute_kip_readonly(kip::request_with(
                r#"FIND(?c.facets["MnemonicState"]) WHERE { ?c {id: :id} } LIMIT 1"#,
                kip::param("id", id),
            ))
            .await
            .unwrap();
        kip::ok_result(&response)
            .and_then(|result| result.as_array())
            .and_then(|rows| rows.first())
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    /// True when the element still exists.
    async fn element_exists(space: &Space, id: &str) -> bool {
        let command = if crate::assess::is_concept_entity_id(id) {
            "FIND(?c) WHERE { ?c {id: :id} } LIMIT 1"
        } else {
            "FIND(?p) WHERE { ?p (id: :id) } LIMIT 1"
        };
        let response = space
            .execute_kip_readonly(kip::request_with(command, kip::param("id", id)))
            .await
            .unwrap();
        kip::ok_result(&response)
            .and_then(|result| result.as_array())
            .is_some_and(|rows| !rows.is_empty())
    }

    /// Seeds three Person Concepts and two `prefers` claims from `alpha`,
    /// each at `memory_strength` 0.8. Returns `(concept ids, proposition ids)`,
    /// both sorted, with the Concepts in `alpha, beta, gamma` order.
    ///
    /// `Person` and `prefers` come from the Cognitive Memory Profile the space
    /// activates at open: KIP 2.0 resolves every symbol through the Schema
    /// Environment, so a test cannot invent a `Topic` type on the way in the way
    /// the 1.x fixtures did.
    async fn seed_people(space: &Space) -> (Vec<String>, Vec<String>) {
        seed_kip(
            space,
            kip::request(
                r#"MUTATE {
  UPSERT CONCEPT ?alpha { MATCH {type: "Person", key: "alpha"} SET FIELDS {name: "alpha"}
                          SET FACET "MnemonicState" {memory_strength: 0.8, salience: 0.5} }
  UPSERT CONCEPT ?beta  { MATCH {type: "Person", key: "beta"}  SET FIELDS {name: "beta"}
                          SET FACET "MnemonicState" {memory_strength: 0.8, salience: 0.5} }
  UPSERT CONCEPT ?gamma { MATCH {type: "Person", key: "gamma"} SET FIELDS {name: "gamma"}
                          SET FACET "MnemonicState" {memory_strength: 0.8, salience: 0.5} }
  ASSERT ?ab (?alpha, "prefers", ?beta) { by: ?alpha, mode: "stated", confidence: 0.8 }
  ASSERT ?ag (?alpha, "prefers", ?gamma) { by: ?alpha, mode: "stated", confidence: 0.8 }
}"#,
            ),
        )
        .await;

        let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person"} } ORDER BY ?c.name"#,
            ))
            .await
            .unwrap();
        let concepts: Vec<String> =
            serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
        assert_eq!(concepts.len(), 3, "{response:?}");

        let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?p.id) WHERE { ?p (?s, "prefers", ?o) }"#,
            ))
            .await
            .unwrap();
        let mut propositions: Vec<String> =
            serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
        propositions.sort();
        assert_eq!(propositions.len(), 2, "{response:?}");
        (concepts, propositions)
    }

    /// The id of the Assertion about one exact `(subject, "prefers", object)`.
    async fn assertion_about(space: &Space, subject: &str, object: &str) -> String {
        let response = space
            .execute_kip_readonly(kip::request_with(
                r#"FIND(?a.id) WHERE {
  ?s CONCEPT {id: :subject}
  ?o CONCEPT {id: :object}
  ?p (?s, "prefers", ?o)
  ?a ASSERTION {proposition: ?p}
} LIMIT 1"#,
                serde_json::Map::from_iter([
                    ("subject".to_string(), serde_json::Value::from(subject)),
                    ("object".to_string(), serde_json::Value::from(object)),
                ]),
            ))
            .await
            .unwrap();
        serde_json::from_value::<Vec<String>>(
            kip::ok_result(&response).cloned().unwrap_or_default(),
        )
        .unwrap_or_default()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no assertion about ({subject}, prefers, {object})"))
    }

    fn strength(state: &serde_json::Value) -> f64 {
        state["memory_strength"]
            .as_f64()
            .unwrap_or_else(|| panic!("no memory_strength in {state}"))
    }

    #[tokio::test]
    async fn settlement_metabolizes_every_memory_and_reinforces_none() {
        let app = test_app_state("settlement");
        let space = create_loaded_space(&app, "settlement").await;
        let now_ms = unix_ms();
        let (concepts, _) = seed_people(&space).await;
        let (alpha, beta, gamma) = (
            concepts[0].clone(),
            concepts[1].clone(),
            concepts[2].clone(),
        );

        // One Concept was surfaced by a recall; the others never were. Under
        // the reference Recall policy that must make no difference to the
        // graph: reading is observed, never rewarded (§1, §32, invariant 2).
        space
            .ledger
            .record_recall(&BTreeSet::from([beta.clone()]), now_ms)
            .await
            .unwrap();

        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();
        assert!(report.decay_ran);
        assert_eq!(report.decayed, 3, "{report:?}");
        assert_eq!(report.new_corrections, 0);

        // All three decayed by the policy factor (0.8 × 0.95), the recalled
        // one included. It earned no gain and bought no exemption — the
        // ledger row exists, and the graph does not know about it.
        for id in [&alpha, &beta, &gamma] {
            let state = mnemonic_state(&space, id).await;
            assert!((strength(&state) - 0.76).abs() < 1e-9, "{id}: {state}");
        }
        assert_eq!(
            space.ledger.get(&beta).await.unwrap().unwrap().recall_count,
            1,
            "the recall is still recorded, just not paid out"
        );

        // Idempotence: an immediate re-settlement does not re-decay (weekly
        // rate limit).
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms + 1)
            .await
            .unwrap();
        assert_eq!(report.decayed, 0, "{report:?}");

        // Nothing decayed the *claims*: KIP 2.0 forbids letting time erode a
        // stance, and a settlement that quietly did would be the single
        // easiest way for this brain to start lying slowly.
        let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.confidence) WHERE { ?a ASSERTION {} }"#,
            ))
            .await
            .unwrap();
        let confidences: Vec<f64> =
            serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
        assert_eq!(confidences.len(), 2, "{response:?}");
        assert!(
            confidences.iter().all(|c| (c - 0.8).abs() < 1e-9),
            "{confidences:?}"
        );
    }

    #[tokio::test]
    async fn settlement_records_an_actors_own_revision_as_a_correction() {
        let app = test_app_state("corrections");
        let space = create_loaded_space(&app, "corrections").await;
        let now_ms = unix_ms();
        let (concepts, _) = seed_people(&space).await;
        let alpha = concepts[0].clone();

        // The asserting actor revises their own claim. In KIP 1.x this was a
        // `superseded: true` flag on the link; here it is a new Assertion plus
        // a supersession, and the old one keeps saying what it said.
        // Pick the Assertion about one exact tuple: a supersession must stay
        // inside one Proposition, and the engine refuses one that wanders.
        let old = assertion_about(&space, &alpha, &concepts[2]).await;

        // A mutation endpoint is a handle, a parameter or a literal — this
        // engine does not run the KQL solver on the write path — so the
        // endpoints arrive as element references.
        seed_kip(
            &space,
            kip::request_with(
                r#"ASSERT ?new (:alpha, "prefers", :gamma) { by: :alpha, mode: "stated", confidence: 0.4 }
SUPERSEDING :old"#,
                serde_json::Map::from_iter([
                    ("alpha".to_string(), serde_json::json!({"id": alpha})),
                    (
                        "gamma".to_string(),
                        serde_json::json!({"id": concepts[2]}),
                    ),
                    ("old".to_string(), serde_json::Value::from(old.as_str())),
                ]),
            ),
        )
        .await;

        let report = space
            .settle_memory_metabolism(MaintenanceScope::Quick, now_ms)
            .await
            .unwrap();
        // Disuse metabolism is paced by `DECAY_MIN_INTERVAL_MS`, not by the
        // cycle scope: a `quick` cycle sweeps too. These Concepts have never
        // carried `MnemonicState`, so they metabolize from the baseline rather
        // than being skipped — "the model forgot to set MnemonicState" must
        // not mean "this memory never fades".
        assert!(report.decay_ran);
        assert!(report.decayed > 0, "{report:?}");
        assert_eq!(report.new_corrections, 1, "{report:?}");
        let row = space.ledger.get(&old).await.unwrap().unwrap();
        assert_eq!(row.correction_count, 1);

        // The actor whose claim needed revising is charged, not the caller:
        // attribution is cognition, authority is Governance.
        let reliability: std::collections::BTreeMap<String, crate::types::SourceReliability> =
            space.db.get_extension_as("source_reliability").unwrap();
        assert_eq!(reliability[&alpha].corrections, 1, "{reliability:?}");

        // The sequence watermark, not a graph flag, is the scan cursor: the
        // processed revision falls behind it and the next pass finds nothing.
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Quick, now_ms + 1)
            .await
            .unwrap();
        assert_eq!(report.new_corrections, 0, "{report:?}");

        // The settlement report is persisted for observability.
        assert!(space.memory_settlement().is_some());
    }

    #[tokio::test]
    async fn probe_memory_uses_negative_knowledge_cache() {
        let app = test_app_state("probe_memory");
        let space = create_loaded_space(&app, "probe_memory").await;
        seed_people(&space).await;

        let hit = space.probe_memory("alpha", None).await.unwrap();
        assert!(hit.found, "{hit:?}");
        assert!(!hit.negative_cached);
        assert!(
            hit.hits
                .iter()
                .any(|citation| citation.name.as_deref() == Some("alpha"))
        );

        let miss = space
            .probe_memory("qqqzzzxxx nonsense", None)
            .await
            .unwrap();
        assert!(!miss.found);
        assert!(!miss.negative_cached);

        // The second identical miss is answered from the cache.
        let cached = space
            .probe_memory("qqqzzzxxx nonsense", None)
            .await
            .unwrap();
        assert!(!cached.found);
        assert!(cached.negative_cached);

        // Formation completion clears negative knowledge (hook calls this).
        space.miss_cache.clear().await.unwrap();
        let fresh = space
            .probe_memory("qqqzzzxxx nonsense", None)
            .await
            .unwrap();
        assert!(!fresh.negative_cached);

        // Oversized queries are never cached (unauthenticated probes on
        // public spaces must not be a disk-write amplifier): the identical
        // repeat still misses without a cache hit.
        let long_query = format!("qqqzzzxxx {}", "x".repeat(600));
        let miss = space.probe_memory(&long_query, None).await.unwrap();
        assert!(!miss.found);
        let repeat = space.probe_memory(&long_query, None).await.unwrap();
        assert!(!repeat.negative_cached);
    }

    /// A full settlement honours what retention wrote, and keeps the record
    /// clock apart from the claim clock.
    #[tokio::test]
    async fn settlement_expires_lapsed_records_and_claims() {
        let app = test_app_state("retention_sweep");
        let space = create_loaded_space(&app, "retention_sweep").await;
        let now_ms = unix_ms();
        let (concepts, _) = seed_people(&space).await;

        // One record whose retention lapsed, one whose claim's window closed,
        // and one held. `expires_at` and `valid_time.until` are two different
        // clocks, and the sweep must not confuse them.
        seed_kip(
            &space,
            kip::request_with(
                r#"MUTATE {
  SET RETENTION :lapsed { expires_at: "2020-01-01T00:00:00Z" }
  SET RETENTION :held { expires_at: "2020-01-01T00:00:00Z", legal_hold: true }
}"#,
                serde_json::Map::from_iter([
                    ("lapsed".to_string(), concepts[0].clone().into()),
                    ("held".to_string(), concepts[1].clone().into()),
                ]),
            ),
        )
        .await;

        // The claim is created with its window already closed rather than
        // edited into one: an Assertion's epistemic payload is immutable, and
        // a changed commitment is a new Assertion, not a rewrite.
        seed_kip(
            &space,
            kip::request(
                r#"MUTATE {
  UPSERT CONCEPT ?alpha { MATCH {type: "Person", key: "alpha"} }
  UPSERT CONCEPT ?gamma { MATCH {type: "Person", key: "gamma"} }
  ASSERT ?lapsed (?alpha, "prefers", ?gamma) {
    by: ?alpha, mode: "stated", confidence: 0.8,
    valid: {from: "2019-01-01T00:00:00Z", until: "2020-01-01T00:00:00Z"}
  }
}"#,
            ),
        )
        .await;

        // Quick scope does not sweep: forgetting is a full-cycle decision.
        let quick = space
            .settle_memory_metabolism(MaintenanceScope::Quick, now_ms)
            .await
            .unwrap();
        assert_eq!(quick.retention.archived, 0, "{quick:?}");
        assert_eq!(quick.retention.expired_assertions, 0, "{quick:?}");

        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();
        assert_eq!(report.retention.error, None, "{report:?}");
        // §163: the hold blocks the sweep that authorized it, and is counted
        // rather than dropped — "archived 1" when 2 lapsed is not the truth.
        assert_eq!(report.retention.archived, 1, "{report:?}");
        assert_eq!(report.retention.held, 1, "{report:?}");
        assert!(report.retention.expired_assertions >= 1, "{report:?}");

        // Archived, not destroyed: the element is still there to be read.
        let still_there = space
            .execute_kip_readonly(kip::request_with(
                "FIND(?c) WHERE { ?c CONCEPT {id: :id} } LIMIT 1",
                kip::param("id", concepts[0].as_str()),
            ))
            .await
            .unwrap();
        assert!(kip::succeeded(&still_there), "{still_there:?}");

        // §14.3: the lapsed claim is `expired` — not retracted and not
        // superseded, because nobody withdrew it and nothing replaced it.
        let status = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.lifecycle.status) WHERE {
  ?a ASSERTION {}
  FILTER(?a.lifecycle.status == "expired")
} LIMIT 5"#,
            ))
            .await
            .unwrap();
        assert!(
            kip::ok_result(&status)
                .and_then(|value| value.as_array())
                .is_some_and(|rows| !rows.is_empty()),
            "{status:?}"
        );

        // Idempotent: a second full cycle finds nothing left to act on.
        let again = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms + 1)
            .await
            .unwrap();
        assert_eq!(again.retention.archived, 0, "{again:?}");
        assert_eq!(again.retention.expired_assertions, 0, "{again:?}");
        assert_eq!(again.retention.held, 1, "{again:?}");
    }

    #[tokio::test]
    async fn pin_exempts_from_metabolism_and_forget_removes_for_real() {
        let app = test_app_state("pin_forget");
        let space = create_loaded_space(&app, "pin_forget").await;
        let now_ms = unix_ms();
        let (concepts, propositions) = seed_people(&space).await;
        let (alpha, pinned, plain) = (
            concepts[0].clone(),
            concepts[1].clone(),
            concepts[2].clone(),
        );

        // Pin one Concept: metabolism must skip it (plan M6 + M2 integration).
        assert_eq!(space.pin_memory(&pinned, true).await.unwrap(), 1);
        let report = space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();
        assert_eq!(report.decayed, 2, "{report:?}");
        let state = mnemonic_state(&space, &pinned).await;
        assert!((strength(&state) - 0.8).abs() < 1e-9, "{state}");
        assert!((strength(&mnemonic_state(&space, &plain).await) - 0.76).abs() < 1e-9);

        // Dry run reports without erasing.
        let doomed = propositions[0].clone();
        let report = space
            .forget_memory(crate::types::MemoryForgetInput {
                entities: vec![doomed.clone()],
                dry_run: true,
            })
            .await
            .unwrap();
        assert!(report.dry_run);
        assert!(report.entities[0].existed);
        assert_eq!(report.deleted_propositions, 0);
        assert!(element_exists(&space, &doomed).await);

        // A real forget purges the Proposition and its ledger row, and reports
        // a bogus id per entity without aborting the batch.
        space
            .ledger
            .record_recall(&BTreeSet::from([doomed.clone()]), now_ms)
            .await
            .unwrap();
        let report = space
            .forget_memory(crate::types::MemoryForgetInput {
                entities: vec![doomed.clone(), "bogus".to_string()],
                dry_run: false,
            })
            .await
            .unwrap();
        assert_eq!(report.deleted_propositions, 1, "{report:?}");
        assert!(
            report
                .entities
                .iter()
                .any(|entry| entry.entity == "bogus" && entry.error.is_some())
        );
        assert!(space.ledger.get(&doomed).await.unwrap().is_none());

        // Forgetting a Concept cascades to the Propositions that quote it —
        // and to their ledger rows: usage traces of a forgotten memory must
        // not survive the memory.
        let survivor = propositions[1].clone();
        space
            .ledger
            .record_recall(&BTreeSet::from([survivor.clone()]), now_ms)
            .await
            .unwrap();
        let report = space
            .forget_memory(crate::types::MemoryForgetInput {
                entities: vec![alpha.clone()],
                dry_run: false,
            })
            .await
            .unwrap();
        assert_eq!(report.deleted_concepts, 1, "{report:?}");
        assert!(report.deleted_propositions >= 1, "{report:?}");
        assert!(
            space.ledger.get(&survivor).await.unwrap().is_none(),
            "a cascaded proposition must lose its ledger row"
        );
    }

    #[tokio::test]
    async fn memory_self_test_flags_unfindable_memories() {
        let app = test_app_state_with_self_test_model("memory_self_test");
        let space = create_loaded_space(&app, "memory_self_test").await;
        let now_ms = unix_ms();
        let (_, propositions) = seed_people(&space).await;

        let report = space
            .run_memory_self_test(now_ms)
            .await
            .unwrap()
            .expect("self-test must run");
        assert_eq!(report.tested, 2, "{report:?}");
        assert_eq!(report.grounded, 1, "{report:?}");
        assert_eq!(report.reencode_tasks, 1, "{report:?}");
        assert_eq!(report.groundability(), Some(0.5));

        // The ungroundable memory produced one pending SleepTask about its
        // subject Concept.
        let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?task) WHERE { ?task CONCEPT {type: "SleepTask"} FILTER(?task.attributes.status == "pending") } LIMIT 10"#,
            ))
            .await
            .unwrap();
        let tasks = kip::ok_result(&response)
            .map(crate::assess::citations_from_json)
            .unwrap_or_default();
        assert_eq!(tasks.len(), 1, "{response:?}");

        // Guardrail: self-tests count only into self_test_count — never into
        // usage reinforcement.
        for id in &propositions {
            let row = space.ledger.get(id).await.unwrap().unwrap();
            assert_eq!(row.self_test_count, 1);
            assert_eq!(row.recall_count, 0);
            assert_eq!(row.last_recalled_at, 0);
        }

        // Every candidate was already tested: the next pass has nothing to do.
        assert!(
            space
                .run_memory_self_test(now_ms + 1)
                .await
                .unwrap()
                .is_none()
        );

        // The exclusion is the sequence cursor, not the ledger. KIP 1.x stamped
        // `self_tested_at` on the link; a Proposition is immutable, so coverage
        // now slides on `_system.space_seq` — and still holds with the ledger
        // rows gone.
        for id in &propositions {
            space.ledger.forget_entity(id).await.unwrap();
        }
        assert!(
            space
                .run_memory_self_test(now_ms + 2)
                .await
                .unwrap()
                .is_none()
        );

        // A newly formed memory enters the window on the next pass.
        let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person"} } ORDER BY ?c.name"#,
            ))
            .await
            .unwrap();
        let people: Vec<String> =
            serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
        seed_kip(
            &space,
            kip::request_with(
                r#"ASSERT (:beta, "prefers", :gamma) { by: :beta, mode: "stated", confidence: 0.8 }"#,
                serde_json::Map::from_iter([
                    ("beta".to_string(), serde_json::json!({"id": people[1]})),
                    ("gamma".to_string(), serde_json::json!({"id": people[2]})),
                ]),
            ),
        )
        .await;
        let report = space
            .run_memory_self_test(now_ms + 3)
            .await
            .unwrap()
            .expect("new memory must be sampled");
        assert_eq!(report.tested, 1, "{report:?}");

        // The report persists and surfaces as the groundability graph stat.
        let stored: crate::types::SelfTestReport = space
            .db
            .get_extension_as("memory_self_test")
            .expect("report stored");
        assert_eq!(stored.groundability(), Some(1.0));
    }

    #[derive(Debug)]
    struct JudgeCompleter;

    impl CompletionFeaturesDyn for JudgeCompleter {
        fn model_name(&self) -> String {
            "judge-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "judge verdict".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn judge_complete_routes_to_independent_model() {
        use crate::assess::AssessContext;
        let app = test_app_state_with_final_model("judge_route");
        let space = create_loaded_space(&app, "judge_route").await;
        let request = || CompletionRequest {
            prompt: "judge this".to_string(),
            ..Default::default()
        };

        // Without a judge model, judge completions share the space model.
        let out = AssessContext::judge_complete(space.as_ref(), request())
            .await
            .unwrap();
        assert_eq!(out.content, "done");

        space.set_judge_model_for_test(Model::with_completer(Arc::new(JudgeCompleter)));
        let out = AssessContext::judge_complete(space.as_ref(), request())
            .await
            .unwrap();
        assert_eq!(out.content, "judge verdict");

        // Non-judge completions (simulator, optimizer) keep the space model.
        let out = AssessContext::complete(space.as_ref(), request())
            .await
            .unwrap();
        assert_eq!(out.content, "done");
    }

    /// Answers the scenario-mining call with a fixed valid scenario that
    /// deliberately contains PII the miner must scrub.
    #[derive(Debug)]
    struct MinerCompleter;

    impl CompletionFeaturesDyn for MinerCompleter {
        fn model_name(&self) -> String {
            "miner-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let scenario = serde_json::json!({
                    "scenario": {
                        "id": "pref_fix",
                        "hidden_profile": {"contact": "work address"},
                        "timeline": [
                            {"turn": 1, "type": "normal",
                             "timestamp": "2026-06-01T10:00:00Z",
                             "user": "My email is bob@example.com and card 12345678901."},
                            {"turn": 2, "type": "normal",
                             "timestamp": "2026-06-05T10:00:00Z",
                             "user": "Correction: use my work address instead."},
                            {"turn": 3, "type": "maintenance",
                             "maintenance": {"trigger": "on_demand", "scope": "quick"}},
                            {"turn": 4, "type": "checkpoint_synthetic",
                             "timestamp": "2026-06-06T10:00:00Z",
                             "query": "Which contact should you use?",
                             "evaluation": {
                                 "scoring_rubric": "honor the correction",
                                 "required_answer_terms": ["work"],
                                 "forbidden_answer_terms": ["card"]
                             }}
                        ]
                    }
                });
                Ok(AgentOutput {
                    content: scenario.to_string(),
                    usage: Usage {
                        input_tokens: 30,
                        output_tokens: 15,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn mine_scenarios_distills_corrections_and_scrubs_pii() {
        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(MinerCompleter)));
        let app = test_app_state_with_models("mine_corrections", Arc::new(models));
        let space = create_loaded_space(&app, "mine_corrections").await;
        let now_ms = unix_ms();
        seed_people(&space).await;

        // One revised Assertion is the mining signal.
        let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.id) WHERE { ?a ASSERTION {} } LIMIT 1"#,
            ))
            .await
            .unwrap();
        let signal: String =
            serde_json::from_value::<Vec<String>>(kip::ok_result(&response).cloned().unwrap())
                .unwrap()
                .remove(0);
        space
            .ledger
            .record_correction(&signal, now_ms)
            .await
            .unwrap();

        let (mined, usage) = crate::eval::mine::mine_scenarios(
            space.as_ref(),
            &crate::eval::mine::MineConfig {
                since_ms: 0,
                max_scenarios: 4,
            },
        )
        .await
        .unwrap();

        assert_eq!(mined.len(), 1);
        assert_eq!(mined[0].signal, signal);
        let scenario = &mined[0].scenario;
        assert_eq!(scenario.id, "mined_pref_fix");
        assert!(
            scenario
                .description
                .as_deref()
                .unwrap()
                .contains("review before adding")
        );
        // PII scrubbed from the produced scenario.
        let encoded = serde_json::to_string(scenario).unwrap();
        assert!(encoded.contains("[email]"), "{encoded}");
        assert!(encoded.contains("[number]"), "{encoded}");
        assert!(!encoded.contains("bob@example.com"));
        assert!(!encoded.contains("12345678901"));
        assert!(usage.input_tokens > 0);
    }

    #[tokio::test]
    async fn memory_status_aggregates_counters_and_schema_audit() {
        let app = test_app_state("memory_status");
        let space = create_loaded_space(&app, "memory_status").await;
        let now_ms = unix_ms();
        let (concepts, _) = seed_people(&space).await;

        // Probe activity: one hit, one miss, one negative-cache hit.
        assert!(space.probe_memory("alpha", None).await.unwrap().found);
        assert!(!space.probe_memory("qqqzzz", None).await.unwrap().found);
        assert!(
            space
                .probe_memory("qqqzzz", None)
                .await
                .unwrap()
                .negative_cached
        );

        // One completed recall surfacing one entity.
        let message = serde_json::json!(Message {
            role: "assistant".to_string(),
            content: vec![
                anda_core::ContentPart::ToolCall {
                    name: "execute_kip_readonly".to_string(),
                    args: serde_json::json!({"command": "FIND"}),
                    call_id: Some("c1".to_string()),
                },
                anda_core::ContentPart::ToolOutput {
                    name: "execute_kip_readonly".to_string(),
                    // A rendered element, not a bare reference: the ledger
                    // meters what an answer actually read.
                    output: serde_json::json!([{
                        "id": concepts[0],
                        "name": "alpha",
                        "schema_ref": "kip://profiles/cognitive-memory@2.0.0/Person",
                    }]),
                    is_error: None,
                    call_id: Some("c1".to_string()),
                    remote_id: None,
                }
            ],
            ..Default::default()
        });
        space.record_recall_usage(&[message]).await.unwrap();

        // One correction + a full settlement (metabolism + schema census).
        let old = assertion_about(&space, &concepts[0], &concepts[1]).await;
        seed_kip(
            &space,
            kip::request_with(
                r#"ASSERT ?new (:alpha, "prefers", :beta) { by: :alpha, mode: "stated", confidence: 0.4 }
SUPERSEDING :old"#,
                serde_json::Map::from_iter([
                    (
                        "alpha".to_string(),
                        serde_json::json!({"id": concepts[0]}),
                    ),
                    ("beta".to_string(), serde_json::json!({"id": concepts[1]})),
                    ("old".to_string(), serde_json::Value::from(old.as_str())),
                ]),
            ),
        )
        .await;
        space
            .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
            .await
            .unwrap();

        let status = space.memory_status().await;
        assert_eq!(status.metrics.probe_hits, 1);
        assert_eq!(status.metrics.probe_misses, 1);
        assert_eq!(status.metrics.negative_cache_hits, 1);
        assert_eq!(status.metrics.recalls_completed, 1);
        assert_eq!(status.metrics.entities_recalled, 1, "{status:?}");
        assert_eq!(status.metrics.corrections, 1);
        assert_eq!(status.probe_hit_rate, Some(0.5));
        assert_eq!(status.correction_rate, Some(1.0));
        assert!(status.graph.concepts > 0);
        assert!(status.graph.predicate_types.unwrap_or(0) >= 1);
        assert!(status.last_settlement.is_some());

        // The full settlement also refreshed the per-predicate census. The
        // vocabulary is the Space's Schema Environment now, so the census
        // covers every declared predicate — including the ones nothing uses.
        let audit = status.last_schema_audit.expect("schema audit reported");
        // Two Propositions, not three: the revision above added an Assertion
        // about a tuple that already existed. A Proposition is the statement,
        // and how many actors have an opinion about it is a separate question.
        assert_eq!(audit.predicates.get("prefers"), Some(&2));
        assert_eq!(audit.predicates.get("same_as"), Some(&0));

        // ... and the same census reaches the Maintenance prompt, which its
        // deployment contract (§A.1) has always claimed. Correction discovery
        // recorded one revision above, so the actor tally travels with it.
        let assessment = space.maintenance_assessment().await;
        assert_eq!(assessment.predicates.get("prefers"), Some(&2));
        assert_eq!(assessment.audited_at, Some(audit.audited_at));
        assert_eq!(
            assessment
                .source_reliability
                .values()
                .map(|source| source.corrections)
                .sum::<u64>(),
            1,
            "{assessment:?}"
        );
    }

    /// Shadow judge: always votes for answer B — with deterministic A/B
    /// alternation this splits the wins 1:1, proving the swap works.
    #[derive(Debug)]
    struct ShadowJudgeCompleter;

    impl CompletionFeaturesDyn for ShadowJudgeCompleter {
        fn model_name(&self) -> String {
            "shadow-judge-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: serde_json::json!({"winner": "b", "reason": "richer"}).to_string(),
                    usage: Usage {
                        input_tokens: 5,
                        output_tokens: 2,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn shadow_eval_compares_policies_without_touching_live_space() {
        let app = test_app_state_with_final_model("shadow_eval");
        let space = create_loaded_space(&app, "shadow_eval").await;
        space.set_judge_model_for_test(Model::with_completer(Arc::new(ShadowJudgeCompleter)));

        // Two completed recall conversations: one stores a serialized
        // RecallInput, one a raw query string.
        for (id_suffix, prompt) in [
            (1u64, r#"{"query": "What tea do I drink?"}"#),
            (2u64, "Where do I work?"),
        ] {
            let now = unix_ms() + id_suffix;
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                messages: vec![serde_json::json!(Message {
                    role: "user".to_string(),
                    content: vec![prompt.to_string().into()],
                    ..Default::default()
                })],
                label: Some("recall".to_string()),
                created_at: now,
                updated_at: now,
                ..Default::default()
            };
            space
                .recall
                .conversations
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        let candidate = crate::types::MemoryPolicy {
            memory_strength_decay_factor: 0.9,
            ..Default::default()
        };
        let report = app
            .run_shadow_eval(
                "shadow_eval",
                crate::types::ShadowEvalInput {
                    policy: candidate.clone(),
                    replay_sample: Some(2),
                },
            )
            .await
            .unwrap();

        assert_eq!(report.replayed, 2, "{report:?}");
        assert_eq!(report.judge_errors, 0, "{report:?}");
        // The judge always votes "B"; the deterministic order alternation
        // maps that to one win per side.
        assert_eq!(report.candidate_wins, 1, "{report:?}");
        assert_eq!(report.baseline_wins, 1, "{report:?}");
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.candidate_policy.memory_strength_decay_factor, 0.9);

        // The report persists on the live space...
        let stored: crate::types::ShadowReport = space
            .db
            .get_extension_as("shadow_report")
            .expect("report stored");
        assert_eq!(stored.replayed, 2);
        // ...while the live space itself stayed untouched: no policy change,
        // no usage recorded by the fork replays (plan guardrail 4).
        assert_eq!(space.memory_policy(), crate::types::MemoryPolicy::default());
        assert_eq!(space.memory_status().await.metrics.recalls_completed, 0);
    }

    #[tokio::test]
    async fn space_token_limit_and_tier_node_limit_are_enforced() {
        let app = test_app_state("space_limits");
        let space = create_loaded_space(&app, "space_limits").await;
        space.admin_update_tier(0, 1).await.unwrap();

        for idx in 0..100 {
            space
                .add_space_token(
                    format!("STlimit-{idx}"),
                    AddSpaceTokenInput {
                        scope: TokenScope::Read,
                        name: format!("reader-{idx}"),
                        expires_at: None,
                        labels: None,
                    },
                    idx,
                )
                .await
                .unwrap();
        }
        let err = space
            .add_space_token(
                "STlimit-overflow".to_string(),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: "overflow".to_string(),
                    expires_at: None,
                    labels: None,
                },
                101,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("space token limit reached"));

        for idx in 0..101 {
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                created_at: idx,
                updated_at: idx,
                label: Some("formation".to_string()),
                ..Default::default()
            };
            space
                .memory
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }
        // Empty input is rejected before any other check…
        let err = space
            .ingest(
                SELF_USER_ID,
                StringOr::Value(FormationInput {
                    messages: vec![],
                    context: None,
                    timestamp: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
        // …including a non-empty content array holding only blank text parts
        // (the part-count check alone would let this burn a formation cycle)…
        let err = space
            .ingest(
                SELF_USER_ID,
                StringOr::Value(FormationInput {
                    messages: vec![Message {
                        role: "user".into(),
                        content: vec!["   ".to_string().into()],
                        ..Default::default()
                    }],
                    context: None,
                    timestamp: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
        // …while real input still hits the tier node limit.
        let err = space
            .ingest(
                SELF_USER_ID,
                StringOr::Value(FormationInput {
                    messages: vec![Message {
                        role: "user".into(),
                        content: vec!["remember this".to_string().into()],
                        ..Default::default()
                    }],
                    context: None,
                    timestamp: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("node limit exceeded"));
    }

    #[tokio::test]
    async fn space_conversations_are_accessible_across_collections() {
        let app = test_app_state("space_conversations");
        let space = create_loaded_space(&app, "space_conversations").await;
        let now = unix_ms();

        let formation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: now,
            updated_at: now,
            label: Some("formation".to_string()),
            ..Default::default()
        };
        let recall = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: now + 1,
            updated_at: now + 1,
            label: Some("recall".to_string()),
            ..Default::default()
        };
        let maintenance = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: now + 2,
            updated_at: now + 2,
            label: Some("maintenance".to_string()),
            ..Default::default()
        };

        let formation_id = space
            .memory
            .add_conversation(ConversationRef::from(&formation))
            .await
            .unwrap();
        let recall_id = space
            .recall
            .conversations
            .add_conversation(ConversationRef::from(&recall))
            .await
            .unwrap();
        let maintenance_id = space
            .maintenance
            .conversations
            .add_conversation(ConversationRef::from(&maintenance))
            .await
            .unwrap();

        assert_eq!(
            space
                .get_conversation(None, formation_id)
                .await
                .unwrap()
                .label,
            Some("formation".to_string())
        );
        assert_eq!(
            space
                .get_conversation(Some("recall".to_string()), recall_id)
                .await
                .unwrap()
                .label,
            Some("recall".to_string())
        );
        assert_eq!(
            space
                .get_conversation(Some("maintenance".to_string()), maintenance_id)
                .await
                .unwrap()
                .label,
            Some("maintenance".to_string())
        );

        let (items, cursor) = space.list_conversations(None, None, Some(1)).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(cursor.is_some());

        let (recall_items, _) = space
            .list_conversations(Some("recall".to_string()), None, Some(10))
            .await
            .unwrap();
        assert_eq!(recall_items.len(), 1);

        let status = space.formation_status();
        assert_eq!(status.conversations, 1);
        assert!(!status.formation_processing);
        assert!(!status.maintenance_processing);

        assert!(
            space
                .list_conversations(None, Some("not-a-cursor".to_string()), Some(1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn list_conversations_clamps_limit_to_safe_bounds() {
        let app = test_app_state("list_limit_clamp");
        let space = create_loaded_space(&app, "list_limit_clamp").await;

        // limit=0 on an empty collection must not panic on the cursor below.
        let (items, cursor) = space.list_conversations(None, None, Some(0)).await.unwrap();
        assert!(items.is_empty());
        assert!(cursor.is_none());

        for idx in 0..3 {
            let conversation = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                created_at: idx,
                updated_at: idx,
                label: Some("formation".to_string()),
                ..Default::default()
            };
            space
                .memory
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        // limit=0 is clamped to 1 instead of dumping the whole collection.
        let (items, cursor) = space.list_conversations(None, None, Some(0)).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(cursor.is_some());

        // "formation" is the documented name of the default collection (API
        // docs, MCP tool schemas): the canonical spelling must stay valid,
        // while typos keep erroring instead of silently reading formation.
        let (items, _) = space
            .list_conversations(Some("formation".to_string()), None, Some(10))
            .await
            .unwrap();
        assert_eq!(items.len(), 3);
        let got = space
            .get_conversation(Some("formation".to_string()), items[0]._id)
            .await
            .unwrap();
        assert_eq!(got._id, items[0]._id);
        assert!(
            space
                .list_conversations(Some("Formation".to_string()), None, Some(10))
                .await
                .is_err()
        );
        assert!(
            space
                .get_conversation(Some("Recall".to_string()), items[0]._id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn space_agent_entrypoints_use_memory_and_model_without_network() {
        let app = test_app_state_with_final_model("space_agent_entrypoints");
        let space = create_loaded_space(&app, "space_agent_entrypoints").await;

        let formation = FormationInput {
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![
                    "remember that the preferred color is blue"
                        .to_string()
                        .into(),
                ],
                ..Default::default()
            }],
            context: Some(InputContext {
                counterparty: Some("external-user-formation".to_string()),
                agent: Some("agent-a".to_string()),
                source: Some("thread-1".to_string()),
                topic: Some("preferences".to_string()),
            }),
            timestamp: Some("2026-06-05T00:00:00Z".to_string()),
        };
        let formation_output = space
            .ingest(SELF_USER_ID, StringOr::Value(formation))
            .await
            .unwrap();
        let formation_id = formation_output.conversation.unwrap();
        wait_until_idle(&space).await;

        let formation_conversation = space.get_conversation(None, formation_id).await.unwrap();
        assert_eq!(formation_conversation.status, ConversationStatus::Completed);
        assert_eq!(space.formation.get_processed(), Some(formation_id));

        let counterparty = space
            .formation
            .get_or_init_counterparty(
                "external-user-formation".to_string(),
                Some("Formation User".to_string()),
            )
            .await
            .unwrap();
        // A Concept names its type by the exact schema symbol it was created
        // under, so the meaning cannot drift when a package is republished.
        assert_eq!(
            counterparty["schema_ref"],
            "kip://profiles/cognitive-memory@2.0.0/Person"
        );
        assert_eq!(counterparty["key"], "external-user-formation");
        assert_eq!(counterparty["name"], "Formation User");

        let recall = RecallInput {
            query: "What color is preferred?".to_string(),
            context: Some(InputContext {
                counterparty: Some("external-user-formation".to_string()),
                agent: None,
                source: None,
                topic: Some("preferences".to_string()),
            }),
        };
        let recall_output = space
            .query(SELF_USER_ID, StringOr::Value(recall))
            .await
            .unwrap();
        let recall_id = recall_output.conversation.unwrap();
        let recall_conversation = space
            .get_conversation(Some("recall".to_string()), recall_id)
            .await
            .unwrap();
        assert_eq!(recall_conversation.status, ConversationStatus::Completed);

        let maintenance_output = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(maintenance_output.conversation.is_some());
        wait_until_idle(&space).await;
        assert_eq!(space.maintenance.get_processed_at().quick, formation_id);
        space
            .maintenance
            .set_processed_at(MaintenanceScope::Full, formation_id + 1)
            .await
            .unwrap();
        space
            .maintenance
            .set_processed_at(MaintenanceScope::Daydream, formation_id + 2)
            .await
            .unwrap();
        let maintenance_at = space.maintenance.get_processed_at();
        assert_eq!(maintenance_at.full, formation_id + 1);
        assert_eq!(maintenance_at.daydream, formation_id + 2);

        let primer = space
            .execute_kip_readonly(kip::request("DESCRIBE PRIMER"))
            .await
            .unwrap();
        assert!(kip::ok_result(&primer).is_some(), "{primer:?}");

        let restart_err = space
            .restart_formation(SELF_USER_ID, formation_id + 1)
            .await
            .unwrap_err();
        assert!(
            restart_err
                .to_string()
                .contains("No pending formation conversation")
        );
    }

    #[tokio::test]
    async fn space_agent_guards_and_readonly_tool_paths() {
        let app = test_app_state_with_final_model("space_agent_guards");
        let space = create_loaded_space(&app, "space_agent_guards").await;

        let readonly = TimedMemoryReadonly::new(space.memory.clone());
        assert_eq!(Tool::<BaseCtx>::name(&readonly), MemoryReadonly::NAME);
        // The read-only definition is the one `anda_kip` ships with the
        // protocol: no write vocabulary, and no `execution` modes for a read
        // path to choose between.
        let definition = Tool::<BaseCtx>::definition(&readonly);
        assert_eq!(definition.name, MemoryReadonly::NAME);
        assert!(
            definition.parameters["properties"]
                .get("execution")
                .is_none()
        );

        let ok_ctx = space
            .engine
            .base_ctx_with(
                SELF_USER_ID,
                "recall_memory",
                MemoryReadonly::NAME,
                Default::default(),
            )
            .unwrap();
        let ok = Tool::<BaseCtx>::call(
            &readonly,
            ok_ctx,
            KipArgs {
                command: Some("DESCRIBE PRIMER".to_string()),
                ..Default::default()
            },
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(ok.is_error, None);

        let err_ctx = space
            .engine
            .base_ctx_with(
                SELF_USER_ID,
                "recall_memory",
                MemoryReadonly::NAME,
                Default::default(),
            )
            .unwrap();
        let err = Tool::<BaseCtx>::call(
            &readonly,
            err_ctx,
            KipArgs {
                command: Some("NOT A VALID KIP COMMAND".to_string()),
                ..Default::default()
            },
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(err.is_error, Some(true));
    }

    #[tokio::test]
    async fn maintenance_rejects_concurrent_runs() {
        let app = test_app_state_with_slow_model("maintenance_concurrent");
        let space = create_loaded_space(&app, "maintenance_concurrent").await;

        let first = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(first.conversation.is_some());

        // The maintenance slot is claimed before settlement (review P1-3),
        // so a concurrent start fails the claim and errors out instead of
        // returning a placeholder output.
        let second = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    formation_id: 2,
                    ..Default::default()
                },
            )
            .await;
        assert!(
            second
                .unwrap_err()
                .to_string()
                .contains("already in progress")
        );

        wait_until_idle(&space).await;
    }

    #[tokio::test]
    async fn hooks_handle_unbound_space_and_accumulate_usage() {
        let app = test_app_state_with_final_model("hooks_usage");
        let space = create_loaded_space(&app, "hooks_usage").await;
        let unbound = Hooks::new(space.db.clone());

        assert!(!BrainHook::is_maintenance_processing(&unbound));
        BrainHook::try_start_formation(&unbound).await;
        assert!(
            BrainHook::try_start_maintenance(&unbound, 168)
                .await
                .is_none()
        );

        let hooks = Hooks::new(space.db.clone());
        hooks.bind_space(Arc::downgrade(&space));
        assert!(!BrainHook::is_maintenance_processing(&hooks));
        space
            .conversations
            .save_extension("brain_processed".to_string(), 7_u64.into())
            .await
            .unwrap();
        BrainHook::try_start_formation(&hooks).await;

        let conversation = Conversation {
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                cached_tokens: 3,
                requests: 2,
            },
            ..Default::default()
        };

        BrainHook::on_conversation_end(&hooks, "recall_memory", &conversation).await;
        BrainHook::on_conversation_end(&hooks, "formation_memory", &conversation).await;
        BrainHook::on_conversation_end(&hooks, "maintenance_memory", &conversation).await;
        BrainHook::on_conversation_end(&hooks, "unknown_agent", &conversation).await;

        let info = space.get_info();
        assert_eq!(info.recall_usage.requests, 2);
        assert_eq!(info.formation_usage.input_tokens, 11);
        assert_eq!(info.maintenance_usage.output_tokens, 7);
        assert_eq!(info.maintenance_usage.cached_tokens, 3);
    }

    #[tokio::test]
    async fn hooks_schedule_maintenance_at_thresholds() {
        let app = test_app_state_with_final_model("hooks_thresholds");
        let space = create_loaded_space(&app, "hooks_thresholds").await;
        let hooks = Hooks::new(space.db.clone());
        hooks.bind_space(Arc::downgrade(&space));

        assert!(BrainHook::try_start_maintenance(&hooks, 20).await.is_none());

        space
            .conversations
            .save_extension("brain_processed".to_string(), 21_u64.into())
            .await
            .unwrap();
        let daydream = BrainHook::try_start_maintenance(&hooks, 21).await.unwrap();
        wait_until_idle(&space).await;
        assert_eq!(space.maintenance_for_test().get_processed_at().daydream, 21);

        space
            .conversations
            .save_extension("brain_processed".to_string(), 42_u64.into())
            .await
            .unwrap();
        let quick = BrainHook::try_start_maintenance(&hooks, 42).await.unwrap();
        wait_until_idle(&space).await;
        assert!(quick > daydream);
        assert_eq!(space.maintenance_for_test().get_processed_at().quick, 42);

        space
            .conversations
            .save_extension("brain_processed".to_string(), 168_u64.into())
            .await
            .unwrap();
        let full = BrainHook::try_start_maintenance(&hooks, 168).await.unwrap();
        wait_until_idle(&space).await;
        assert!(full > quick);
        assert_eq!(space.maintenance_for_test().get_processed_at().full, 168);
    }

    /// M4 acceptance: an exported OKF bundle plus its manifest replays into
    /// an empty space with every document checksum intact.
    #[cfg(feature = "wiki")]
    #[tokio::test]
    async fn wiki_export_bundle_replays_into_empty_space() {
        use crate::wiki::{WikiBundleEntry, WikiCommitInput, WikiImportInput};

        let app = test_app_state("wiki_replay_src");
        let source = create_loaded_space(&app, "wiki_replay_source").await;
        for (title, body) in [
            ("部署指南", "# 部署指南\n\n回滚使用上一版本快照。\n"),
            ("安全政策", "# 安全政策\n\n密钥必须存放在 KMS。\n"),
        ] {
            let mut input = WikiCommitInput {
                title: title.to_string(),
                content: body.to_string(),
                ..Default::default()
            };
            input.namespace = Some("kb".to_string());
            source
                .wiki
                .commit("op".to_string(), input, unix_ms())
                .await
                .unwrap();
        }
        let export = source
            .wiki
            .export_bundle("op".to_string(), Some("kb".to_string()), unix_ms())
            .await
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(
            &export
                .entries
                .iter()
                .find(|e| e.path == "manifest.json")
                .unwrap()
                .content,
        )
        .unwrap();

        // Replay into a brand-new space.
        let replay_app = test_app_state("wiki_replay_dst");
        let target = create_loaded_space(&replay_app, "wiki_replay_target").await;
        let entries: Vec<WikiBundleEntry> = export
            .entries
            .iter()
            .filter(|e| e.path.ends_with(".md"))
            .cloned()
            .collect();
        let imported = target
            .wiki
            .import_bundle(
                "op".to_string(),
                WikiImportInput {
                    entries,
                    namespace: Some("kb".to_string()),
                },
                unix_ms(),
            )
            .await
            .unwrap();
        assert_eq!(imported.created, export.docs);

        // Every replayed document matches the manifest checksum: the bundle
        // is a faithful backup.
        for doc in manifest["docs"].as_array().unwrap() {
            let path = doc["path"].as_str().unwrap();
            let checksum = doc["checksum"].as_str().unwrap();
            let restored = imported
                .docs
                .iter()
                .find(|d| d.path == path)
                .unwrap_or_else(|| panic!("missing {path}"));
            let info = target.wiki.get_doc(restored.doc_id).await.unwrap();
            assert_eq!(info.current_checksum, checksum, "checksum drift for {path}");
        }

        // SpaceInfo exposes the M4 wiki metrics.
        let info = target.get_info();
        assert_eq!(info.wiki_docs, export.docs);
        assert!(info.wiki_versions >= export.docs);
    }

    /// Replays scripted completion responses in order; used to drive the
    /// wiki digest extraction deterministically.
    #[cfg(feature = "wiki")]
    #[derive(Debug)]
    struct ScriptedCompleter(std::sync::Mutex<std::collections::VecDeque<String>>);

    #[cfg(feature = "wiki")]
    impl CompletionFeaturesDyn for ScriptedCompleter {
        fn model_name(&self) -> String {
            "scripted-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let next = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"facts": []}"#.to_string());
            Box::pin(async move {
                Ok(AgentOutput {
                    content: next,
                    ..Default::default()
                })
            })
        }
    }

    #[cfg(feature = "wiki")]
    #[tokio::test]
    async fn wiki_digest_extracts_supersedes_and_verifies() {
        use crate::wiki::WikiCommitInput;

        let extraction_v1 = serde_json::json!({
            "concepts": [
                {"type": "Organization", "name": "Acme", "attributes": {"description": "发布政策的组织"}}
            ],
            "facts": [
                {
                    "subject": {"type": "Organization", "name": "Acme"},
                    "predicate": "publishes",
                    "object": {"type": "Policy", "name": "安全政策"},
                    "confidence": 0.95,
                    "anchor": "安全政策-0"
                },
                {
                    "subject": {"type": "Policy", "name": "安全政策"},
                    "predicate": "requires",
                    "object": {"type": "Procedure", "name": "密钥轮换"},
                    "confidence": 0.9,
                    "anchor": "no-such-anchor"
                }
            ]
        })
        .to_string();
        let extraction_v2 = serde_json::json!({
            "facts": [
                {
                    "subject": {"type": "Organization", "name": "Acme"},
                    "predicate": "publishes",
                    "object": {"type": "Policy", "name": "安全政策"},
                    "confidence": 0.95,
                    "anchor": "安全政策-0"
                },
                {
                    "subject": {"type": "Policy", "name": "安全政策"},
                    "predicate": "requires",
                    "object": {"type": "Procedure", "name": "双因素认证"},
                    "confidence": 0.9,
                    "anchor": "安全政策-0"
                }
            ]
        })
        .to_string();

        let models = Models::default();
        models.set_model(Model::with_completer(Arc::new(ScriptedCompleter(
            std::sync::Mutex::new([extraction_v1, extraction_v2].into_iter().collect()),
        ))));
        let app = test_app_state_with_models("wiki_digest_app", Arc::new(models));
        let space = create_loaded_space(&app, "wiki_digest_space").await;

        // RecallAgent exposes the wiki evidence tools to its LLM loop.
        {
            use anda_core::Agent;
            let deps = space.recall.tool_dependencies();
            assert!(deps.contains(&"wiki_search".to_string()));
            assert!(deps.contains(&"wiki_read".to_string()));
        }

        // Digest is opt-in: disabled spaces refuse to run.
        let err = space.run_wiki_digest(SELF_USER_ID).await.unwrap_err();
        assert!(err.to_string().contains("disabled"));
        space
            .update(
                crate::types::UpdateSpaceInput {
                    wiki_digest: Some(true),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        assert!(space.wiki_digest_enabled());

        let v1 = space
            .wiki
            .commit(
                "tester".to_string(),
                WikiCommitInput {
                    title: "安全政策".to_string(),
                    content: "# 安全政策\n\n所有系统必须启用密钥轮换。\n".to_string(),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();

        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 1);
        assert_eq!(report.facts, 2);
        assert_eq!(report.superseded, 0);
        assert!(report.citations_checked >= 2);
        assert_eq!(report.citations_invalid, 0);

        // The Space's Schema Environment grew to hold the extracted
        // vocabulary — in KIP 2.0 the digest cannot mint a type on the way in,
        // so `Organization` existing at all is the host having decided it does.
        let types = space
            .execute_kip_readonly(kip::request("LIST TYPES LIMIT 500"))
            .await
            .unwrap();
        assert!(
            kip::ok_result(&types)
                .unwrap()
                .to_string()
                .contains("Organization"),
            "{types:?}"
        );

        // §5.6/§64.2: the digest minted the `$self` Person, so the Space now
        // designates one — and the primer every agent reads reports it as a
        // different thing from the authenticated Principal.
        let primer = space
            .execute_kip_readonly(kip::request("DESCRIBE PRIMER"))
            .await
            .unwrap();
        let primer = kip::ok_result(&primer).unwrap();
        let designated = primer
            .pointer("/cognitive_identity/self_concept/id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            designated.starts_with("C-"),
            "cognitive_identity: {}",
            primer["cognitive_identity"]
        );
        assert!(
            primer.pointer("/execution_context/principal/id").is_some(),
            "the primer distinguishes the Principal from $self: {primer}"
        );

        // The graph holds the concepts, and the claim about them carries its
        // provenance as Evidence rather than as metadata on the link.
        let acme = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Organization", key: "Acme"} }"#,
            ))
            .await
            .unwrap();
        let acme: Vec<String> =
            serde_json::from_value(kip::ok_result(&acme).cloned().unwrap()).unwrap();
        assert_eq!(acme.len(), 1, "{acme:?}");

        // The claim cites its Evidence…
        let cited = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.evidence) WHERE {
  ?s CONCEPT {type: "Organization", key: "Acme"}
  ?o CONCEPT {type: "Policy", key: "安全政策"}
  ?p (?s, "publishes", ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
            ))
            .await
            .unwrap();
        let cited_text = kip::ok_result(&cited).unwrap().to_string();
        // §13.2: a citation is `{"id": "E-…", "role": "support"}` — the role is
        // what makes it a citation rather than a bare pointer, so assert on it.
        assert!(cited_text.contains(r#""role""#), "citations: {cited_text}");
        assert!(cited_text.contains("\"E-"), "citations: {cited_text}");

        // …and the Evidence carries the passage it was read from.
        let evidence = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?e.payload) WHERE { ?e EVIDENCE {evidence_class: "document"} }"#,
            ))
            .await
            .unwrap();
        let evidence_text = kip::ok_result(&evidence).unwrap().to_string();
        assert!(
            evidence_text.contains("wiki://"),
            "evidence: {evidence_text}"
        );
        assert!(
            evidence_text.contains("wiki_digest@v1"),
            "evidence: {evidence_text}"
        );
        assert!(
            evidence_text.contains(&format!("@{}", v1.version.id)),
            "the citation should pin version {}: {evidence_text}",
            v1.version.id
        );

        // Digest ledger event recorded with both facts.
        let events = space
            .wiki
            .list_events(Some("DigestExtracted".to_string()), None, None, Some(10))
            .await
            .unwrap();
        assert_eq!(events.events.len(), 1);

        // No pending versions: the next run is a no-op (cursor advanced).
        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 0);

        // Revision drops the 密钥轮换 requirement; digesting it must mark the
        // stale proposition superseded while the surviving fact stays live.
        let v2 = space
            .wiki
            .commit(
                "tester".to_string(),
                WikiCommitInput {
                    doc_id: Some(v1.doc.id),
                    parent_version: Some(v1.version.id),
                    title: "安全政策".to_string(),
                    content: "# 安全政策\n\n所有系统必须启用双因素认证。\n".to_string(),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 1);
        assert_eq!(report.superseded, 1);

        // The dropped fact: the digest withdrew its own claim, and the
        // Proposition itself survives untouched. KIP 1.x flagged the link
        // `superseded`, which spoke for every actor at once; a retraction says
        // only that *this* reader stopped saying it.
        let stale = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.lifecycle.status) WHERE {
  ?s CONCEPT {type: "Policy", key: "安全政策"}
  ?o CONCEPT {type: "Procedure", key: "密钥轮换"}
  ?p (?s, "requires", ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
            ))
            .await
            .unwrap();
        let statuses: Vec<String> =
            serde_json::from_value(kip::ok_result(&stale).cloned().unwrap()).unwrap();
        assert_eq!(statuses, vec!["retracted".to_string()], "{stale:?}");

        // The surviving fact stays believed.
        let live = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?a.lifecycle.status) WHERE {
  ?s CONCEPT {type: "Organization", key: "Acme"}
  ?o CONCEPT {type: "Policy", key: "安全政策"}
  ?p (?s, "publishes", ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
            ))
            .await
            .unwrap();
        let statuses: Vec<String> =
            serde_json::from_value(kip::ok_result(&live).cloned().unwrap()).unwrap();
        assert_eq!(statuses, vec!["active".to_string()], "{live:?}");
        let _ = v2;

        // Labeled documents never reach the graph: the Cognitive Nexus has
        // no ACL, so digesting them would leak restricted facts to any Read
        // principal (launch review P1-3).
        space
            .wiki
            .commit(
                "tester".to_string(),
                WikiCommitInput {
                    title: "受限预案".to_string(),
                    content: "# 受限预案\n\n机密事实：夜航坐标由 Acme 维护。\n".to_string(),
                    acl_label: Some("secret".to_string()),
                    ..Default::default()
                },
                unix_ms(),
            )
            .await
            .unwrap();
        let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
        assert_eq!(report.digested, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.facts, 0);
    }
}

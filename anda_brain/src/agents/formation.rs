use anda_core::{
    Agent, AgentContext, AgentOutput, BoxError, CompletionRequest, Document, Documents, Message,
    Resource, StateFeatures, Tool, estimate_tokens,
};
use anda_db::{
    collection::Collection,
    schema::{DocumentId, Json, Map},
};
use anda_engine::{
    context::{AgentCtx, CompletionRunner},
    extension::note::{NoteTool, load_notes, load_notes_from_legacy},
    local_date_hour,
    memory::{Conversation, ConversationRef, ConversationStatus, MemoryManagement},
    unix_ms,
};
use parking_lot::RwLock;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{BrainHook, PERSON_BY_KEY, RunnerFlow, RunnerHost, drive_runner_loop, first_row};
use crate::types::FormationInput;

const REVIEW_INSTRUCTIONS: &str = include_str!("../../assets/BrainFormationReview.md");
// A cost heuristic for one semantic omission check, not a completeness guarantee.
// KIP validates writes, but cannot tell which source facts the model overlooked.
const REVIEW_MIN_INPUT_TOKENS: usize = 10_000;

fn review_prompt(conversation_id: DocumentId, message_count: usize) -> String {
    let captured = message_count.min(crate::kip::MAX_INGESTED_MESSAGES);
    let bindings = if captured == 0 {
        "No input-message Evidence bindings are available.".to_string()
    } else {
        format!(
            "Host write-time Evidence bindings :msg1 through :msg{captured} refer to input messages {} through {message_count} (1-based). Earlier messages have no automatic Evidence binding. Read committed Evidence by ids from write receipts or Assertion/Activity links; :msgN is not a read parameter.",
            message_count - captured + 1
        )
    };
    format!(
        "{REVIEW_INSTRUCTIONS}\n\nHost review scope: formation conversation {conversation_id}, {message_count} input messages. {bindings}"
    )
}

// The runner guardrails are shared with maintenance; see
// `RUNNER_MAX_MODEL_TURNS` in agents.rs. Tests keep the historical name.
#[cfg(test)]
use super::RUNNER_MAX_MODEL_TURNS as FORMATION_MAX_MODEL_TURNS;

/// Resets the AtomicU64 to 0 on drop (panic guard for processing_conversation).
struct ProcessingGuard(Option<Arc<AtomicU64>>);
impl ProcessingGuard {
    fn disarm(mut self) {
        self.0.take();
    }
}
impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            value.store(0, Ordering::SeqCst);
        }
    }
}

#[derive(Clone)]
pub struct FormationAgent {
    product_control: Option<Arc<crate::product::control::Control>>,
    prompt: Arc<str>,
    memory: Arc<MemoryManagement>,
    /// The collection backing `memory`'s conversations. `MemoryManagement`
    /// wraps document access only, so the `brain_processed` watermark — a
    /// collection extension — is read and written through this handle.
    conversations: Arc<Collection>,
    processing_conversation: Arc<AtomicU64>,
    processing_gate: Arc<super::ProcessingGate>,
    hook: Arc<dyn BrainHook>,
    history: Arc<RwLock<VecDeque<Document>>>,
    max_input_tokens: usize,
    clock: Arc<crate::runtime::BusinessClock>,
    tasks: crate::runtime::RuntimeTasks,
}

impl FormationAgent {
    pub(crate) fn with_product_control(
        mut self,
        control: Arc<crate::product::control::Control>,
    ) -> Self {
        self.product_control = Some(control);
        self
    }
    pub const NAME: &'static str = "formation_memory";
    pub fn new(
        memory: Arc<MemoryManagement>,
        conversations: Arc<Collection>,
        hook: Arc<dyn BrainHook>,
        max_input_tokens: usize,
    ) -> Self {
        Self {
            product_control: None,
            prompt: super::prompts::active_prompt(super::prompts::PromptTarget::Formation),
            clock: Arc::new(crate::runtime::BusinessClock::default()),
            tasks: crate::runtime::RuntimeTasks::default(),
            max_input_tokens,
            memory,
            conversations,
            processing_conversation: Arc::new(AtomicU64::new(0)),
            processing_gate: Arc::new(super::ProcessingGate::default()),
            history: Arc::new(RwLock::new(VecDeque::new())),
            hook,
        }
    }

    /// Backfills the completed-conversation ring after a restart, mirroring
    /// recall/maintenance `init` — without it the `history_formation` context
    /// block stays empty until the next conversation completes. Formation
    /// conversations live in the shared memory store under their ingesting
    /// user, so there is no single-user list to query; walk backwards from
    /// the newest conversation and keep the latest completed formation ones.
    /// The scan is bounded: this is best-effort context, not recovery state.
    pub(crate) fn with_prompt(mut self, prompt: Arc<str>) -> Self {
        self.prompt = prompt;
        self
    }

    pub(crate) fn with_clock(mut self, clock: Arc<crate::runtime::BusinessClock>) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) fn with_tasks(mut self, tasks: crate::runtime::RuntimeTasks) -> Self {
        self.tasks = tasks;
        self
    }

    pub(crate) fn with_processing_gate(mut self, gate: Arc<super::ProcessingGate>) -> Self {
        self.processing_conversation = gate.formation.clone();
        self.processing_gate = gate;
        self
    }

    #[cfg(feature = "experiments")]
    pub(crate) fn clear_history(&self) {
        self.history.write().clear();
    }

    pub async fn init(&self) -> Result<(), BoxError> {
        // Matches the `push_completed_history` cap in `drive_runner_loop`.
        const HISTORY_LEN: usize = 2;
        const SCAN_LIMIT: u64 = 32;

        // Collected newest-first; the runtime ring runs oldest -> newest.
        let mut newest: Vec<Document> = Vec::with_capacity(HISTORY_LEN);
        let mut id = self.memory.max_conversation_id();
        let mut scanned = 0u64;
        while id > 0 && scanned < SCAN_LIMIT && newest.len() < HISTORY_LEN {
            scanned += 1;
            if let Ok(conv) = self.memory.get_conversation(id).await
                && conv._id
                    > self
                        .conversations
                        .get_extension_as::<u64>("history_boundary")
                        .unwrap_or(0)
                && conv.status == ConversationStatus::Completed
                && conv
                    .label
                    .as_deref()
                    .is_none_or(|label| label == "formation")
            {
                newest.push(Document::from(conv));
            }
            id -= 1;
        }
        newest.reverse();
        *self.history.write() = newest.into();
        Ok(())
    }

    pub fn is_processing(&self) -> bool {
        self.processing_conversation.load(Ordering::SeqCst) != 0
    }

    pub(crate) fn processing_id(&self) -> u64 {
        self.processing_conversation.load(Ordering::SeqCst)
    }

    pub fn get_processed(&self) -> Option<DocumentId> {
        self.conversations
            .get_extension_as::<DocumentId>("brain_processed")
    }

    /// Sets the formation watermark without processing anything, so a test can
    /// stage "this Space has formed memory" without an LLM turn.
    #[cfg(test)]
    pub(crate) async fn set_processed_for_test(&self, id: DocumentId) {
        self.conversations
            .save_extension("brain_processed".to_string(), id.into())
            .await
            .unwrap();
    }

    /// Resolves the Person Concept for a counterparty, creating it once.
    ///
    /// The counterparty handle is the Concept's `key` — immutable Space-local
    /// identity — while `name` is a mutable display label. Matching on the key
    /// is what makes this idempotent: a name-only match would mint a second
    /// Person for anyone who renamed themselves, and a `key` is identity within
    /// its type, so `type` is not optional decoration here.
    ///
    /// A Person is semantic cognition. It is not a Principal and cannot
    /// authenticate as one — which is why nothing about this write says
    /// anything about what the counterparty may do.
    pub async fn get_or_init_counterparty(
        &self,
        counterparty: String,
        name: Option<String>,
    ) -> Result<Json, BoxError> {
        let preserve_name = name.is_none();
        if preserve_name {
            let existing = first_row(
                self.memory
                    .query(
                        PERSON_BY_KEY,
                        Some(crate::kip::param("key", counterparty.clone())),
                    )
                    .await?,
            );
            if !existing.is_null() {
                return Ok(existing);
            }
        }
        let parameters = Map::from_iter([
            ("key".to_string(), Json::from(counterparty.clone())),
            (
                "name".to_string(),
                Json::from(name.unwrap_or_else(|| counterparty.clone())),
            ),
        ]);
        let command = if preserve_name {
            // Creation can race another caller. A key collision is read back
            // below; an UPSERT would overwrite the winner's display name.
            r#"CREATE CONCEPT ?person { TYPE "Person" NAME :name SET FIELDS { key: :key } }"#
        } else {
            r#"UPSERT CONCEPT ?person {
  MATCH { type: "Person", key: :key }
  SET FIELDS { name: :name }
}"#
        };
        if let Err(error) = self.memory.execute(command, Some(parameters)).await
            && !(preserve_name && error.code == anda_kip::KipErrorCode::IdentityConflict)
        {
            return Err(error.into());
        }

        // Read back rather than returning the write receipt: callers want the
        // Person as it now stands, which on a match is not what this call sent.
        Ok(first_row(
            self.memory
                .query(
                    PERSON_BY_KEY,
                    Some(Map::from_iter([(
                        "key".to_string(),
                        Json::from(counterparty),
                    )])),
                )
                .await?,
        ))
    }

    pub async fn start_process(
        &self,
        ctx: AgentCtx,
        conversation: DocumentId,
    ) -> Result<(), BoxError> {
        let current = self.processing_conversation.load(Ordering::SeqCst);
        if current != 0 {
            return Err(format!(
                "FormationAgent is already processing conversation {}",
                current
            )
            .into());
        }
        if self.hook.is_maintenance_processing() {
            return Err(
                "MaintenanceAgent is processing, formation will resume when maintenance completes"
                    .into(),
            );
        }

        // Find the next valid pending conversation starting from the given ID (inclusive)
        let conv = self
            .find_next_submitted(conversation.saturating_sub(1))
            .await?
            .ok_or_else(|| {
                format!(
                    "No pending formation conversation found starting from {}",
                    conversation
                )
            })?;

        self.try_process(ctx, conv);
        Ok(())
    }

    pub fn try_process(&self, ctx: AgentCtx, conversation: Conversation) {
        let admission = self.processing_gate.admission.lock();
        if self.processing_gate.maintenance.load(Ordering::SeqCst)
            || self.hook.is_maintenance_processing()
        {
            return;
        }
        if self
            .processing_conversation
            .compare_exchange(0, conversation._id, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log::info!(
                target: "brain",
                "FormationAgent is already processing conversation {}, cannot process conversation {}",
                self.processing_conversation.load(Ordering::SeqCst),
                conversation._id
            );
            return;
        }

        let agent = self.clone();
        let pc = self.processing_conversation.clone();
        let guard = ProcessingGuard(Some(pc));
        drop(admission);
        self.tasks.spawn(async move {
            // Guard is captured before spawn so cancellation before polling also releases the slot.
            agent.process_loop(ctx, conversation).await;
            // Normal exit: process_loop already manages the atomic properly,
            // so defuse the guard to avoid clobbering a valid value.
            guard.disarm();
        });
    }

    async fn process_loop(&self, ctx: AgentCtx, mut conversation: Conversation) {
        loop {
            let conv_id = conversation._id;

            self.process_one(&ctx, &mut conversation).await;
            self.hook
                .on_conversation_end(Self::NAME, &conversation)
                .await;
            if conversation.status == ConversationStatus::Failed {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await; // 避免快速失败循环
                // 重试一次
                self.process_one(&ctx, &mut conversation).await;
                self.hook
                    .on_conversation_end(Self::NAME, &conversation)
                    .await;
            }

            if !matches!(
                conversation.status,
                ConversationStatus::Completed | ConversationStatus::Cancelled
            ) {
                log::error!(
                    target: "brain",
                    "Conversation {} ended with status {:?}, not marking as processed",
                    conv_id,
                    conversation.status
                );
                // 上游异常，重置 processing 状态以允许外部干预或后续请求自动触发
                self.processing_conversation.store(0, Ordering::SeqCst);
                break;
            }

            // The marker is a high-water mark: reprocessing an older
            // conversation (e.g. via restart_formation) must not rewind it.
            let processed = self.get_processed().unwrap_or_default().max(conv_id);
            self.conversations
                .save_extension("brain_processed".to_string(), processed.into())
                .await
                .ok();

            if let Some(id) = self.hook.try_start_maintenance(conv_id).await {
                log::info!(
                    target: "brain",
                    "Triggered maintenance for conversation {}, new maintenance conversation {}",
                    conv_id,
                    id
                );

                // The maintenance claim transferred the writer slot before
                // spawning its worker. Do not clear a newer formation owner.
                break; // 交由 maintenance agent 处理后续流程，退出循环
            }

            // 查找下一个待处理的 conversation
            match self.find_next_submitted(conv_id).await {
                Ok(Some(next_conv)) => {
                    if self
                        .processing_conversation
                        .compare_exchange(
                            conv_id,
                            next_conv._id,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        conversation = next_conv;
                        continue;
                    }
                    // CAS 失败说明其他线程已接管，退出
                    break;
                }
                Ok(None) => {
                    self.processing_conversation.store(0, Ordering::SeqCst);
                    // 双重检查：store(0) 前可能有新 conversation 到达但 try_process CAS 失败
                    if let Ok(Some(next_conv)) = self.find_next_submitted(conv_id).await
                        && {
                            let _admission = self.processing_gate.admission.lock();
                            !self.processing_gate.maintenance.load(Ordering::SeqCst)
                                && !self.hook.is_maintenance_processing()
                                && self
                                    .processing_conversation
                                    .compare_exchange(
                                        0,
                                        next_conv._id,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                        }
                    {
                        conversation = next_conv;
                        continue;
                    }
                    break;
                }
                Err(error) => {
                    log::error!(target: "brain", "formation queue read failed: {error}");
                    self.processing_conversation.store(0, Ordering::SeqCst);
                    break;
                }
            }
        }
    }

    async fn find_next_submitted(&self, after_id: u64) -> Result<Option<Conversation>, BoxError> {
        use anda_db::query::{Filter, Fv, RangeQuery};
        let mut cursor = after_id;
        loop {
            let ids = self
                .conversations
                .query_ids(
                    Filter::And(vec![
                        Box::new(Filter::Field((
                            "_id".into(),
                            RangeQuery::Gt(Fv::U64(cursor)),
                        ))),
                        Box::new(Filter::Field((
                            "status".into(),
                            RangeQuery::Include(
                                [
                                    ConversationStatus::Submitted,
                                    ConversationStatus::Working,
                                    ConversationStatus::Idle,
                                    ConversationStatus::Failed,
                                ]
                                .into_iter()
                                .map(|status| Fv::Text(status.to_string()))
                                .collect(),
                            ),
                        ))),
                    ]),
                    Some(64),
                )
                .await?;
            if ids.is_empty() {
                return Ok(None);
            }
            for id in ids {
                cursor = id;
                let conv = self.memory.get_conversation(id).await?;
                if !matches!(
                    conv.status,
                    ConversationStatus::Completed | ConversationStatus::Cancelled
                ) && conv
                    .label
                    .as_deref()
                    .is_none_or(|label| label == "formation")
                {
                    return Ok(Some(conv));
                }
            }
        }
    }

    async fn mark_conversation_failed(&self, conversation: &mut Conversation, reason: String) {
        super::mark_conversation_failed(
            |id, changes| self.memory.update_conversation(id, changes),
            "formation",
            conversation,
            reason,
        )
        .await;
    }

    /// Persists the current full conversation snapshot. A formation is
    /// retried, so a stale `failed_reason` is cleared on the way through.
    async fn persist_conversation_snapshot(&self, conversation: &Conversation) {
        super::persist_conversation_snapshot(
            |id, changes| self.memory.update_conversation(id, changes),
            "formation",
            conversation,
            true,
        )
        .await;
    }

    async fn process_one(&self, ctx: &AgentCtx, conversation: &mut Conversation) {
        if let Some(control) = &self.product_control {
            use crate::product::{SourceAdmissionError, SourceIdentity};
            let admission = SourceIdentity::for_conversation(conversation)
                .map_err(|_| SourceAdmissionError::Suppressed)
                .and_then(|source| control.admit_source(&source));
            match admission {
                Ok(epoch) => {
                    ctx.base.set_state(epoch);
                }
                Err(error) => {
                    conversation.status = match error {
                        SourceAdmissionError::Busy => ConversationStatus::Submitted,
                        SourceAdmissionError::Suppressed => ConversationStatus::Cancelled,
                    };
                    conversation.failed_reason = Some(error.to_string());
                    self.persist_conversation_snapshot(conversation).await;
                    return;
                }
            }
        }
        // A Memory Interface revision waits for its predecessors; one that
        // failed blocks it for good, and that is reported, not skipped
        // (MI §5.1).
        let intent = crate::memory_interface::MemoryIntent::of(conversation).map(Arc::new);
        if let Some(intent) = &intent
            && let Some(blocker) = self.hook.memory_predecessor_failed(intent).await
        {
            conversation.status = ConversationStatus::Cancelled;
            conversation.failed_reason = Some(format!("predecessor_failed:{blocker}"));
            self.persist_conversation_snapshot(conversation).await;
            return;
        }
        let trace = crate::memory_interface::FormationTrace::resume(conversation);
        ctx.base
            .set_state(crate::memory_interface::IntentState(intent.clone()));
        ctx.base.set_state(trace.clone());
        let prompt = match conversation
            .messages
            .first()
            .and_then(|v| serde_json::from_value::<Message>(v.clone()).ok())
            .and_then(|v| v.text())
        {
            Some(p) => p,
            None => {
                self.mark_conversation_failed(conversation, "No prompt found".to_string())
                    .await;
                return;
            }
        };

        // Markdown/raw-text submissions use the same host-owned Evidence path
        // as structured conversations. Keep their exact text as one message.
        let input =
            serde_json::from_str::<FormationInput>(&prompt).unwrap_or_else(|_| FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec![prompt.clone().into()],
                    ..Default::default()
                }],
                context: None,
                timestamp: None,
            });
        let counterparty = input
            .context
            .as_ref()
            .and_then(|input_ctx| input_ctx.counterparty.clone());

        let now_ms = unix_ms();
        // The context sources are independent; fetch them concurrently (same
        // pattern as recall's context assembly).
        let (counterparty_info, primer, notes) = tokio::join!(
            async {
                match counterparty {
                    Some(counterparty) => {
                        self.get_or_init_counterparty(counterparty, None).await.ok()
                    }
                    None => None,
                }
            },
            async { self.memory.describe_primer().await.unwrap_or_default() },
            async {
                match load_notes(ctx).await {
                    Some(n) => n,
                    None => load_notes_from_legacy(ctx).await.unwrap_or_default(),
                }
            },
        );

        // The observation this pass was called on, for the runtime to mint as
        // Evidence (Spec §71.1). Set unconditionally — see [`Observation`] —
        // and before the completion, so every `execute_kip` the model makes
        // inherits it.
        //
        // A thread/channel is provenance, not the identity of an observation.
        // Each durable conversation has its own key, stable across retries,
        // so a later submission from the same source cannot cite old bytes.
        let observation = crate::kip::observation_ingest(
            &input.messages,
            &crate::kip::observation_timestamp(input.timestamp.as_deref(), conversation.created_at),
            &format!("formation:conversation:{}", conversation._id),
            counterparty_info
                .as_ref()
                .and_then(|person| person.get("id"))
                .and_then(Json::as_str),
        )
        .map(|mut ingest| {
            // A scoped observation's Evidence carries its scope too, so the
            // raw source stays in its task (Profile §20.3).
            if let Some(intent) = &intent {
                intent.scope_evidence(&mut ingest);
            }
            ingest
        })
        .map(Arc::new);
        // Each claim's `at` is when its source said it (Spec §13.2), so the
        // model reads the captured times rather than guessing them.
        let captured = observation
            .as_deref()
            .map(crate::kip::observation_manifest)
            .unwrap_or_else(|| "none".into());
        ctx.base.set_state(super::Observation(observation));

        // add history conversations to provide more context for recall
        let chat_history: Vec<Document> = if self
            .product_control
            .as_ref()
            .is_some_and(|control| control.epoch() > 0)
        {
            vec![]
        } else {
            self.history.read().iter().cloned().collect()
        };

        let chat_history = if chat_history.is_empty() {
            vec![]
        } else {
            vec![Message {
                role: "user".into(),
                content: vec![
                    Documents::new("history_formation".to_string(), chat_history)
                        .to_string()
                        .into(),
                ],
                name: Some("$system".into()),
                timestamp: Some(now_ms),
                ..Default::default()
            }]
        };
        let review_prompt = (estimate_tokens(&prompt) >= REVIEW_MIN_INPUT_TOKENS)
            .then(|| review_prompt(conversation._id, input.messages.len()));
        let mut runner = ctx.clone().completion_iter(
            CompletionRequest {
                instructions: format!(
                    "{}\n\n---\n\n# `DESCRIBE PRIMER` Result:\n{}\n\n---\n\n# Your Notes:\n{}\n\n# Counterparty Profile:\n{}\n\n# Captured Evidence:\n{}\n\n# Memory Interface Intent:\n{}\n\n# Current Datetime: {}",
                    super::prompts::system_prompt(super::prompts::PromptTarget::Formation, &self.prompt),
                    primer,
                    serde_json::to_string(&notes.items).unwrap_or_default(),
                    serde_json::to_string(&counterparty_info).unwrap_or_default(),
                    captured,
                    intent
                        .as_deref()
                        .map(crate::memory_interface::MemoryIntent::directive)
                        .unwrap_or_else(|| "none (an ordinary Formation submission)".into()),
                    local_date_hour(self.clock.now_ms()).unwrap_or_default()
                ),
                prompt,
                chat_history,
                tools: ctx.tool_definitions(Some(&self.tool_dependencies())),
                tool_choice_required: true,
                ..Default::default()
            },
            vec![],
        );
        runner.set_unbound(true);

        let mut host = FormationRunnerHost {
            agent: self,
            review_prompt,
            trace,
        };
        drive_runner_loop(&mut host, &mut runner, conversation).await;
    }
}

/// Formation's seams of the shared runner loop (`drive_runner_loop`): the
/// review pass for large inputs and the Failed-retry `failed_reason` reset.
struct FormationRunnerHost<'a> {
    agent: &'a FormationAgent,
    /// Taken once at the first successful idle boundary. The follow-up shares
    /// the runner's turn/time budgets and survives a compaction handoff.
    review_prompt: Option<String>,
    /// What the pass has committed, persisted with each snapshot so a
    /// receipt's disposition survives a restart.
    trace: crate::memory_interface::FormationTrace,
}

impl RunnerHost for FormationRunnerHost<'_> {
    fn label(&self) -> &'static str {
        "formation"
    }

    fn history(&self) -> &RwLock<VecDeque<Document>> {
        &self.agent.history
    }

    async fn persist_snapshot(&self, conversation: &Conversation) {
        self.agent.persist_conversation_snapshot(conversation).await;
    }

    async fn mark_failed(&self, conversation: &mut Conversation, reason: String) {
        self.agent
            .mark_conversation_failed(conversation, reason)
            .await;
    }

    fn turn_is_done(&self, runner: &CompletionRunner) -> bool {
        runner.is_done() || runner.is_idle() && self.review_prompt.is_none()
    }

    fn on_turn_success(&self, conversation: &mut Conversation) {
        // Clears a previous attempt's failure so the process_loop Failed
        // retry converges to a clean Completed snapshot.
        conversation.failed_reason = None;
        self.trace.persist_into(conversation);
    }

    fn after_turn(&mut self, runner: &mut CompletionRunner, is_done: bool) -> RunnerFlow {
        if runner.is_idle()
            && let Some(prompt) = self.review_prompt.take()
        {
            // Keep the write receipts, bound ids and readbacks the review needs.
            // The shared loop compacts when necessary; pruning all tool results
            // here would discard the very evidence this follow-up must inspect.
            runner.follow_up(prompt);
            return RunnerFlow::Continue;
        }

        if is_done {
            RunnerFlow::Break
        } else {
            RunnerFlow::Continue
        }
    }
}

impl Agent<AgentCtx> for FormationAgent {
    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        "Receives conversation messages and encodes them into structured memory within the Cognitive Nexus via KIP.".to_string()
    }

    fn tool_dependencies(&self) -> Vec<String> {
        vec![
            self.memory.name(),
            NoteTool::NAME.to_string(),
            crate::vocabulary::DeclareSymbolsTool::NAME.to_string(),
            crate::cognitive::MemoryRuntimeTool::NAME.to_string(),
            crate::kip_reference::KipReferenceTool::NAME.to_string(),
        ]
    }

    // 接收来自外部的 FormationInput，创建一个新的 Conversation，并启动处理流程。
    async fn run(
        &self,
        ctx: AgentCtx,
        prompt: String, // FormationInput serialized as JSON string
        _resources: Vec<Resource>,
    ) -> Result<AgentOutput, BoxError> {
        let caller = ctx.caller();
        let now_ms = unix_ms();
        let token_count = estimate_tokens(&prompt);
        if token_count > self.max_input_tokens {
            return Err(format!(
                "Input too large: {} tokens (estimated), max allowed is {} tokens",
                token_count, self.max_input_tokens
            )
            .into());
        }

        let mut conversation = Conversation {
            user: *caller,
            messages: vec![json!(Message {
                role: "user".into(),
                content: vec![prompt.into()],
                ..Default::default()
            })],
            period: now_ms / 3600 / 1000,
            created_at: now_ms,
            updated_at: now_ms,
            label: Some("formation".to_string()),
            extra: Some(json!(ctx.meta().extra)),
            ..Default::default()
        };

        let id = self
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await?;
        conversation._id = id;
        let res = AgentOutput {
            conversation: Some(id),
            ..Default::default()
        };

        let is_idle = self.processing_conversation.load(Ordering::SeqCst) == 0;
        if is_idle {
            if self.hook.is_maintenance_processing() {
                log::info!(
                    target: "brain",
                    conversation = id;
                    "Formation queued while maintenance is processing"
                );
            } else {
                // A missing marker means nothing was processed yet; resume from
                // the beginning to catch conversations queued before this one.
                let prev_id = self.get_processed().unwrap_or_default();
                if prev_id + 1 < id {
                    // Resume from the last processed conversation to catch any missed ones
                    if let Some(conv) = self.find_next_submitted(prev_id).await? {
                        self.try_process(ctx, conv);
                    }
                } else {
                    self.try_process(ctx, conversation);
                }
            }
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::{FormationAgent, ProcessingGuard};
    use crate::{
        agents::SELF_USER_ID,
        space::AppState,
        testkit::{
            app_state_core, create_loaded_space, models_with_completer,
            models_with_configured_completer,
        },
        types::{FormationInput, InputContext, MaintenanceInput, MaintenanceScope},
    };
    use anda_core::{
        Agent, AgentOutput, BoxError, BoxPinFut, CompletionRequest, ContentPart, Message, ToolCall,
        Usage,
    };
    use anda_engine::{
        context::{AgentCtx, COMPACTION_PROMPT},
        memory::{Conversation, ConversationRef, ConversationStatus},
        model::{CompletionFeaturesDyn, Models},
        unix_ms,
    };
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use tokio::time::{Duration, sleep};

    #[derive(Debug)]
    struct SuccessCompleter;

    impl CompletionFeaturesDyn for SuccessCompleter {
        fn model_name(&self) -> String {
            "success-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "formation done".to_string(),
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

    #[derive(Debug)]
    struct FailedReasonCompleter;

    impl CompletionFeaturesDyn for FailedReasonCompleter {
        fn model_name(&self) -> String {
            "failed-reason-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    failed_reason: Some("formation failed".to_string()),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["formation failure".to_string().into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct RetryCompleter {
        calls: Arc<AtomicU64>,
    }

    impl CompletionFeaturesDyn for RetryCompleter {
        fn model_name(&self) -> String {
            "retry-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    Ok(AgentOutput {
                        failed_reason: Some("transient formation failure".to_string()),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec!["retry later".to_string().into()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                } else {
                    Ok(AgentOutput {
                        content: "formation retried".to_string(),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec![format!("recovered: {}", req.prompt).into()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                }
            })
        }
    }

    #[derive(Debug)]
    struct ErrorCompleter;

    impl CompletionFeaturesDyn for ErrorCompleter {
        fn model_name(&self) -> String {
            "error-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move { Err("model error".into()) })
        }
    }

    /// Emits an endless stream of tool calls (a non-converging model), and a
    /// summary for compaction handoff requests so the runner keeps looping.
    #[derive(Debug)]
    struct ToolLoopCompleter;

    impl CompletionFeaturesDyn for ToolLoopCompleter {
        fn model_name(&self) -> String {
            "formation-tool-loop-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let usage = Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    requests: 1,
                    ..Default::default()
                };
                if req.tools.is_empty() {
                    // Compaction handoff turn: tools are cleared for the
                    // summarization request.
                    return Ok(AgentOutput {
                        content: "handoff summary".to_string(),
                        usage,
                        ..Default::default()
                    });
                }
                Ok(AgentOutput {
                    tool_calls: vec![ToolCall {
                        name: "execute_kip".to_string(),
                        // A command the engine refuses, so the tool answers with
                        // an error and the model is asked again: the point of
                        // this fixture is a loop that never converges.
                        args: serde_json::json!({"command": "NOT A VALID KIP COMMAND"}),
                        result: None,
                        call_id: Some("loop".to_string()),
                        remote_id: None,
                    }],
                    usage,
                    ..Default::default()
                })
            })
        }
    }

    /// Emits `tool_turns` tool-call turns, then a final content turn.
    #[derive(Debug)]
    struct CountedToolThenDoneCompleter {
        calls: Arc<AtomicU64>,
        tool_turns: u64,
    }

    impl CompletionFeaturesDyn for CountedToolThenDoneCompleter {
        fn model_name(&self) -> String {
            "formation-counted-tool-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let calls = self.calls.clone();
            let tool_turns = self.tool_turns;
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let usage = Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    requests: 1,
                    ..Default::default()
                };
                if call < tool_turns {
                    return Ok(AgentOutput {
                        tool_calls: vec![ToolCall {
                            name: "execute_kip".to_string(),
                            args: serde_json::json!({"command": "DESCRIBE PRIMER"}),
                            result: None,
                            call_id: Some(format!("call-{call}")),
                            remote_id: None,
                        }],
                        usage,
                        ..Default::default()
                    });
                }
                Ok(AgentOutput {
                    content: "formation done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["formation done".to_string().into()],
                        ..Default::default()
                    }],
                    usage,
                    ..Default::default()
                })
            })
        }
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

    /// Models provider history around a real write and a review readback, so
    /// removing tool receipts cannot silently turn the review into guesswork.
    #[derive(Debug)]
    struct ReviewCompleter {
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
        fail_on_call: Option<usize>,
    }

    impl CompletionFeaturesDyn for ReviewCompleter {
        fn model_name(&self) -> String {
            "formation-review-test-model".into()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let call = {
                let mut requests = self.requests.lock().unwrap();
                let call = requests.len();
                requests.push(req.clone());
                call
            };
            let fail = self.fail_on_call == Some(call);
            Box::pin(async move {
                let usage = Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    requests: 1,
                    ..Default::default()
                };
                if fail {
                    return Ok(AgentOutput {
                        failed_reason: Some("review fixture failure".into()),
                        usage,
                        ..Default::default()
                    });
                }

                let command = if call == 0 {
                    Some(
                        r#"CREATE ACTIVITY ?receipt {
                        SET FIELDS {activity_class: "extraction", status: "completed"}
                        SET STRUCTURAL {("inputs", :msg1)}
                    }"#
                        .to_string(),
                    )
                } else if request_text(&req).contains(super::REVIEW_INSTRUCTIONS) {
                    let evidence_id = req
                        .raw_history
                        .iter()
                        .filter_map(|item| item["content"].as_str())
                        .filter_map(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                        .find_map(|receipt| {
                            receipt["results"][0]["result"]["changes"]
                                .as_array()?
                                .iter()
                                .filter_map(|change| change["id"].as_str())
                                .find(|id| id.starts_with("E-"))
                                .map(str::to_string)
                        })
                        .expect("review needs the preserved write receipt's Evidence id");
                    Some(format!(
                        "FIND(?e) WHERE {{ ?e EVIDENCE {{id: \"{evidence_id}\"}} }} LIMIT 1"
                    ))
                } else {
                    None
                };
                let tool_calls: Vec<ToolCall> = command
                    .map(|command| ToolCall {
                        name: "execute_kip".into(),
                        args: json!({"command": command}),
                        call_id: Some(format!("review-{call}")),
                        result: None,
                        remote_id: None,
                    })
                    .into_iter()
                    .collect();
                let content = if call < 2 {
                    "formation done"
                } else {
                    "review done"
                };
                let mut chat_history = Vec::new();
                let mut raw_history = Vec::new();
                if !req.prompt.is_empty() {
                    chat_history.push(Message {
                        role: "user".into(),
                        content: vec![req.prompt.clone().into()],
                        ..Default::default()
                    });
                    raw_history.push(json!({"role": "user", "content": req.prompt}));
                }
                for part in &req.content {
                    if let ContentPart::ToolOutput {
                        output, call_id, ..
                    } = part
                    {
                        raw_history.push(json!({
                            "role": "tool", "tool_call_id": call_id,
                            "content": output.to_string()
                        }));
                    }
                }
                if !req.content.is_empty() {
                    chat_history.push(Message {
                        role: req.role.clone().unwrap_or_else(|| "tool".into()),
                        content: req.content,
                        ..Default::default()
                    });
                }
                let assistant = Message {
                    role: "assistant".into(),
                    content: if tool_calls.is_empty() {
                        vec![content.to_string().into()]
                    } else {
                        tool_calls
                            .iter()
                            .map(|tool| ContentPart::ToolCall {
                                name: tool.name.clone(),
                                args: tool.args.clone(),
                                call_id: tool.call_id.clone(),
                            })
                            .collect()
                    },
                    ..Default::default()
                };
                raw_history.push(if let Some(tool) = tool_calls.first() {
                    json!({"role": "assistant", "tool_calls": [{
                        "id": tool.call_id, "type": "function",
                        "function": {"name": tool.name, "arguments": tool.args.to_string()}
                    }]})
                } else {
                    json!({"role": "assistant", "content": content})
                });
                chat_history.push(assistant);
                Ok(AgentOutput {
                    content: content.into(),
                    tool_calls,
                    chat_history,
                    raw_history,
                    usage,
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct CompactionCompleter {
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl CompletionFeaturesDyn for CompactionCompleter {
        fn model_name(&self) -> String {
            "formation-compaction-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let requests = self.requests.clone();
            Box::pin(async move {
                let text = request_text(&req);
                requests.lock().unwrap().push(text.clone());
                if text.contains(COMPACTION_PROMPT.trim()) {
                    return Ok(AgentOutput {
                        content: "handoff summary".to_string(),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec!["summary turn".to_string().into()],
                            ..Default::default()
                        }],
                        usage: Usage {
                            input_tokens: 8,
                            output_tokens: 2,
                            requests: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                }

                if text.contains("handoff summary") {
                    return Ok(AgentOutput {
                        content: "review complete".to_string(),
                        chat_history: vec![Message {
                            role: "assistant".to_string(),
                            content: vec!["reviewed after compaction".to_string().into()],
                            ..Default::default()
                        }],
                        usage: Usage {
                            input_tokens: 16,
                            output_tokens: 4,
                            requests: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                }

                Ok(AgentOutput {
                    content: "formation draft".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["draft before compaction".to_string().into()],
                        ..Default::default()
                    }],
                    usage: Usage {
                        input_tokens: 900,
                        output_tokens: 4,
                        requests: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            })
        }
    }

    fn part_text(part: &ContentPart) -> Option<String> {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text } => Some(text.clone()),
            ContentPart::ToolOutput { output, .. } | ContentPart::ToolCall { args: output, .. } => {
                Some(output.to_string())
            }
            _ => None,
        }
    }

    fn request_text(req: &CompletionRequest) -> String {
        let mut text = Vec::new();
        if !req.prompt.is_empty() {
            text.push(req.prompt.clone());
        }
        text.extend(req.content.iter().filter_map(part_text));
        for message in &req.chat_history {
            text.extend(message.content.iter().filter_map(part_text));
        }
        text.join("\n")
    }

    fn test_app_state(name: &str) -> AppState {
        app_state_core(name, Arc::new(Models::default()), vec![], "test", 0)
    }

    fn test_app_state_with_completer<C>(name: &str, completer: C) -> AppState
    where
        C: CompletionFeaturesDyn,
    {
        app_state_core(name, models_with_completer(completer), vec![], "test", 0)
    }

    fn formation_prompt(counterparty: Option<&str>) -> String {
        formation_prompt_with_text("remember this preference", counterparty)
    }

    fn formation_prompt_with_text(text: &str, counterparty: Option<&str>) -> String {
        serde_json::to_string(&FormationInput {
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![text.to_string().into()],
                ..Default::default()
            }],
            context: counterparty.map(|counterparty| InputContext {
                counterparty: Some(counterparty.to_string()),
                ..Default::default()
            }),
            timestamp: None,
        })
        .unwrap()
    }

    #[derive(Debug, Default)]
    struct EvidenceCompleter {
        calls: AtomicU64,
        results: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl CompletionFeaturesDyn for EvidenceCompleter {
        fn model_name(&self) -> String {
            "formation-evidence-regression".into()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let write = self.calls.fetch_add(1, Ordering::SeqCst).is_multiple_of(2);
            let results = self.results.clone();
            Box::pin(async move {
                if write {
                    return Ok(AgentOutput {
                        tool_calls: vec![ToolCall {
                            name: "execute_kip".into(),
                            args: json!({"command": r#"CREATE ACTIVITY ?receipt {
                                SET FIELDS {activity_class: "extraction", status: "completed"}
                                SET STRUCTURAL {("inputs", :msg1)}
                            }"#}),
                            result: None,
                            call_id: Some("capture".into()),
                            remote_id: None,
                        }],
                        ..Default::default()
                    });
                }
                for part in req.content {
                    if let ContentPart::ToolOutput { output, .. } = part {
                        results.lock().unwrap().push(output);
                    }
                }
                Ok(AgentOutput {
                    content: "captured".into(),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn formation_captures_compatible_timestamps_and_raw_text_with_stable_retry_evidence() {
        let received_at = 1_789_862_400_123;
        let said = "  Keep this original text.\n原文不变。  ";
        for (index, timestamp, expected, raw) in [
            (
                0,
                Some("2026-09-20T00:00:00Z"),
                "2026-09-20T00:00:00.000Z".to_string(),
                false,
            ),
            (
                1,
                Some(" 2026-09-20T08:00:00.123456+08:00 "),
                "2026-09-20T00:00:00.123Z".to_string(),
                false,
            ),
            (
                2,
                Some("not a timestamp"),
                crate::kip::timestamp(received_at),
                false,
            ),
            (3, None, crate::kip::timestamp(received_at), false),
            (4, None, crate::kip::timestamp(received_at), true),
        ] {
            let model = EvidenceCompleter::default();
            let results = model.results.clone();
            let name = format!("formation_capture_{index}");
            let app = test_app_state_with_completer(&name, model);
            let space = create_loaded_space(&app, &name).await;
            let prompt = if raw {
                said.to_string()
            } else {
                serde_json::to_string(&FormationInput {
                    messages: vec![Message {
                        role: "user".into(),
                        content: vec![said.to_string().into()],
                        ..Default::default()
                    }],
                    context: None,
                    timestamp: timestamp.map(str::to_string),
                })
                .unwrap()
            };
            let mut original = stored_conversation(
                &space,
                vec![json!(Message {
                    role: "user".into(),
                    content: vec![prompt.into()],
                    ..Default::default()
                })],
            )
            .await;
            original.created_at = received_at;
            let ctx = space
                .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
                .unwrap();
            for _ in 0..2 {
                let mut conversation = original.clone();
                space.formation.process_one(&ctx, &mut conversation).await;
                assert_eq!(conversation.status, ConversationStatus::Completed);
            }
            let results = results.lock().unwrap().clone();
            assert_eq!(results.len(), 2);
            assert!(
                results.iter().all(|r| r["status"] == "succeeded"),
                "{results:?}"
            );
            let evidence = space
                .memory
                .query("FIND(?e) WHERE {?e EVIDENCE {}} LIMIT 10", None)
                .await
                .unwrap();
            let rows = evidence.as_array().unwrap();
            assert_eq!(rows.len(), 1, "a retry must retain the original Evidence");
            assert_eq!(rows[0]["observed_at"], expected);
            assert_eq!(rows[0]["payload"]["inline"]["content"][0]["text"], said);
            space.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn counterparty_lookup_preserves_display_name_and_explicit_updates_still_work() {
        let app = test_app_state("counterparty_name");
        let space = create_loaded_space(&app, "counterparty_name").await;
        let agent = &space.formation;
        let initial = agent
            .get_or_init_counterparty("alice".into(), Some("Alice Smith".into()))
            .await
            .unwrap();
        let found = agent
            .get_or_init_counterparty("alice".into(), None)
            .await
            .unwrap();
        assert_eq!(found, initial, "lookup must not mutate the existing Person");
        let renamed = agent
            .get_or_init_counterparty("alice".into(), Some("Alice Jones".into()))
            .await
            .unwrap();
        assert_eq!(renamed["id"], initial["id"]);
        assert_eq!(renamed["name"], "Alice Jones");
        let (a, b) = tokio::join!(
            agent.get_or_init_counterparty("new_person".into(), None),
            agent.get_or_init_counterparty("new_person".into(), None),
        );
        assert_eq!(a.unwrap()["id"], b.unwrap()["id"]);
        space.close().await.unwrap();
    }

    async fn stored_conversation(
        space: &crate::space::Space,
        messages: Vec<serde_json::Value>,
    ) -> Conversation {
        let now = unix_ms();
        let mut conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            messages,
            label: Some("formation".to_string()),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        let id = space
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
        conversation._id = id;
        conversation
    }

    #[test]
    fn processing_guard_resets_conversation_id_on_drop() {
        let processing = Arc::new(AtomicU64::new(42));

        {
            let _guard = ProcessingGuard(Some(processing.clone()));
            assert_eq!(processing.load(Ordering::SeqCst), 42);
        }

        assert_eq!(processing.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn formation_agent_name_matches_registered_agent_name() {
        assert_eq!(FormationAgent::NAME, "formation_memory");
    }

    #[tokio::test]
    async fn formation_agent_trait_metadata_matches_runtime_registration() {
        let app = test_app_state("formation_trait_metadata");
        let space = create_loaded_space(&app, "formation_trait_metadata").await;

        assert_eq!(
            Agent::<AgentCtx>::name(space.formation.as_ref()),
            FormationAgent::NAME
        );
        assert!(
            Agent::<AgentCtx>::description(space.formation.as_ref()).contains("structured memory")
        );
        let tools = Agent::<AgentCtx>::tool_dependencies(space.formation.as_ref());
        assert!(tools.iter().any(|name| name == "execute_kip"));
        assert!(tools.iter().any(|name| name == "kip_reference"));
        assert!(tools.iter().any(|name| name == "note"));
    }

    #[tokio::test]
    async fn find_next_submitted_skips_terminal_and_non_formation_conversations() {
        let app = test_app_state("formation_find_next");
        let space = create_loaded_space(&app, "formation_find_next").await;
        let now = unix_ms();

        for conversation in [
            Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("formation".to_string()),
                created_at: now,
                updated_at: now,
                ..Default::default()
            },
            Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Submitted,
                label: Some("recall".to_string()),
                created_at: now + 1,
                updated_at: now + 1,
                ..Default::default()
            },
            Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Cancelled,
                label: Some("formation".to_string()),
                created_at: now + 2,
                updated_at: now + 2,
                ..Default::default()
            },
        ] {
            space
                .memory
                .add_conversation(ConversationRef::from(&conversation))
                .await
                .unwrap();
        }

        let pending = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            label: Some("formation".to_string()),
            created_at: now + 3,
            updated_at: now + 3,
            ..Default::default()
        };
        let pending_id = space
            .memory
            .add_conversation(ConversationRef::from(&pending))
            .await
            .unwrap();

        let found = space
            .formation
            .find_next_submitted(0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found._id, pending_id);
        assert!(
            space
                .formation
                .find_next_submitted(pending_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn mark_conversation_failed_persists_status_and_reason() {
        let app = test_app_state("formation_mark_failed");
        let space = create_loaded_space(&app, "formation_mark_failed").await;
        let now = unix_ms();
        let mut conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Submitted,
            label: Some("formation".to_string()),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        let id = space
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
        conversation._id = id;

        space
            .formation
            .mark_conversation_failed(&mut conversation, "boom".to_string())
            .await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert_eq!(conversation.failed_reason.as_deref(), Some("boom"));
        let stored = space.memory.get_conversation(id).await.unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(stored.failed_reason.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn start_process_rejects_busy_and_maintenance_states() {
        let app = test_app_state("formation_start_guards");
        let space = create_loaded_space(&app, "formation_start_guards").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();

        space
            .formation
            .processing_conversation
            .store(42, Ordering::SeqCst);
        let err = space
            .formation
            .start_process(ctx.clone(), 1)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("already processing conversation 42")
        );
        space
            .formation
            .processing_conversation
            .store(0, Ordering::SeqCst);

        let app = test_app_state_with_completer("formation_maintenance_guard", SlowCompleter);
        let space = create_loaded_space(&app, "formation_maintenance_guard").await;
        let maintenance = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(maintenance.conversation.is_some());

        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let err = space.formation.start_process(ctx, 1).await.unwrap_err();
        assert!(err.to_string().contains("MaintenanceAgent is processing"));

        for _ in 0..100 {
            if !space.is_processing() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("maintenance did not finish");
    }

    #[tokio::test]
    async fn start_process_finds_pending_conversation_and_dispatches_worker() {
        let app = test_app_state_with_completer("formation_start_success", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_start_success").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .start_process(ctx, pending._id)
            .await
            .unwrap();
        for _ in 0..100 {
            if !space.formation.is_processing() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(space.formation.get_processed(), Some(pending._id));
        assert_eq!(
            space
                .memory
                .get_conversation(pending._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );
    }

    #[tokio::test]
    async fn run_queues_while_maintenance_runs_and_resumes_from_processed_gap() {
        let app = test_app_state_with_completer("formation_run_resume_gap", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_run_resume_gap").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        space
            .conversations
            .save_extension("brain_processed".to_string(), 0_u64.into())
            .await
            .unwrap();
        let missed = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        let output = Agent::<AgentCtx>::run(
            space.formation.as_ref(),
            ctx,
            formation_prompt(Some("resume-gap-user")),
            vec![],
        )
        .await
        .unwrap();
        let queued_id = output.conversation.unwrap();

        for _ in 0..100 {
            if !space.formation.is_processing() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(space.formation.get_processed(), Some(queued_id));
        assert_eq!(
            space
                .memory
                .get_conversation(missed._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );

        let app = test_app_state_with_completer("formation_run_maintenance_queue", SlowCompleter);
        let space = create_loaded_space(&app, "formation_run_maintenance_queue").await;
        let maintenance = space
            .maintenance(
                SELF_USER_ID,
                MaintenanceInput {
                    scope: MaintenanceScope::Quick,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(maintenance.conversation.is_some());
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let output = Agent::<AgentCtx>::run(
            space.formation.as_ref(),
            ctx,
            formation_prompt(None),
            vec![],
        )
        .await
        .unwrap();
        assert!(output.conversation.is_some());
        assert!(!space.formation.is_processing());

        for _ in 0..100 {
            if !space.is_processing() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("maintenance did not finish");
    }

    #[tokio::test]
    async fn try_process_returns_when_another_conversation_owns_the_guard() {
        let app = test_app_state("formation_try_process_guard");
        let space = create_loaded_space(&app, "formation_try_process_guard").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .processing_conversation
            .store(conversation._id + 10, Ordering::SeqCst);
        space.formation.try_process(ctx, conversation.clone());

        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            conversation._id + 10
        );
    }

    #[tokio::test]
    async fn process_one_marks_missing_prompt_and_completion_errors() {
        let app = test_app_state("formation_no_prompt");
        let space = create_loaded_space(&app, "formation_no_prompt").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut no_prompt = stored_conversation(&space, vec![]).await;

        space.formation.process_one(&ctx, &mut no_prompt).await;

        assert_eq!(no_prompt.status, ConversationStatus::Failed);
        assert_eq!(no_prompt.failed_reason.as_deref(), Some("No prompt found"));

        let app = test_app_state_with_completer("formation_model_error", ErrorCompleter);
        let space = create_loaded_space(&app, "formation_model_error").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(Some("counterparty-error")).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert!(
            conversation
                .failed_reason
                .as_deref()
                .unwrap_or_default()
                .contains("CompletionRunner error")
        );
    }

    #[tokio::test]
    async fn process_one_persists_model_failed_reason() {
        let app = test_app_state_with_completer("formation_failed_reason", FailedReasonCompleter);
        let space = create_loaded_space(&app, "formation_failed_reason").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert_eq!(
            conversation.failed_reason.as_deref(),
            Some("formation failed")
        );
        let stored = space
            .memory
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(stored.failed_reason.as_deref(), Some("formation failed"));
    }

    #[tokio::test]
    async fn process_one_fails_tool_loop_at_model_turn_limit() {
        let app = test_app_state_with_completer("formation_turn_limit", ToolLoopCompleter);
        let space = create_loaded_space(&app, "formation_turn_limit").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Failed);
        assert!(
            conversation
                .failed_reason
                .as_deref()
                .unwrap_or_default()
                .contains(&format!(
                    "exceeded model turn limit of {}",
                    super::FORMATION_MAX_MODEL_TURNS
                )),
            "failed_reason: {:?}",
            conversation.failed_reason
        );
        assert_eq!(
            stored_status(&space, conversation._id).await,
            ConversationStatus::Failed
        );
    }

    /// The persisted status of one conversation, allowing the write to become
    /// visible.
    ///
    /// A conversation this long is rewritten on every throttled snapshot, and a
    /// read issued in the same instant as the final write can still see the
    /// previous one. The assertion is about what the agent persisted, not about
    /// how fast the store settles.
    async fn stored_status(space: &crate::space::Space, id: u64) -> ConversationStatus {
        for _ in 0..50 {
            let stored = space.memory.get_conversation(id).await.unwrap();
            if stored.status != ConversationStatus::Working {
                return stored.status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        space.memory.get_conversation(id).await.unwrap().status
    }

    #[tokio::test]
    async fn process_one_throttles_intermediate_persistence_until_terminal() {
        let app = test_app_state_with_completer(
            "formation_persist_throttle",
            CountedToolThenDoneCompleter {
                calls: Arc::new(AtomicU64::new(0)),
                // Fewer working turns than PERSIST_EVERY_N_TURNS, so only the
                // terminal turn triggers a write.
                tool_turns: 3,
            },
        );
        let space = create_loaded_space(&app, "formation_persist_throttle").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        let updates_before = space.conversations.stats().update_count;
        space.formation.process_one(&ctx, &mut conversation).await;
        let updates_after = space.conversations.stats().update_count;

        assert_eq!(conversation.status, ConversationStatus::Completed);
        // 4 model turns total: 3 intermediate Working turns are throttled and
        // only the terminal Completed turn is written.
        assert_eq!(updates_after - updates_before, 1);
        let stored = space
            .memory
            .get_conversation(conversation._id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
        // The terminal write carries the full accumulated usage (4 model
        // turns x 10 input tokens; `requests` also counts tool executions),
        // so throttled turns are not lost from the stored snapshot.
        assert_eq!(stored.usage.input_tokens, 40);
    }

    #[tokio::test]
    async fn run_rejects_oversized_formation_input_before_persisting() {
        let app = test_app_state("formation_input_too_large");
        let space = create_loaded_space(&app, "formation_input_too_large").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let prompt = "x ".repeat(1_000_000);

        let err = Agent::<AgentCtx>::run(space.formation.as_ref(), ctx, prompt, vec![])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Input too large"));
        assert_eq!(space.conversations.len(), 0);
    }

    #[tokio::test]
    async fn process_loop_processes_submitted_formation_queue_sequentially() {
        let app = test_app_state_with_completer("formation_process_loop_queue", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_process_loop_queue").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let first = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;
        let second = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(Some("queue-user")).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .processing_conversation
            .store(first._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, first).await;

        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(space.formation.get_processed(), Some(second._id));
        assert_eq!(
            space
                .memory
                .get_conversation(second._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn process_loop_retries_failed_conversation_once_and_clears_failure_reason() {
        let calls = Arc::new(AtomicU64::new(0));
        let app = test_app_state_with_completer(
            "formation_process_loop_retry",
            RetryCompleter {
                calls: calls.clone(),
            },
        );
        let space = create_loaded_space(&app, "formation_process_loop_retry").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;

        space
            .formation
            .processing_conversation
            .store(pending._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, pending.clone()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(space.formation.get_processed(), Some(pending._id));
        let stored = space.memory.get_conversation(pending._id).await.unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
        assert_eq!(stored.failed_reason, None);
    }

    #[tokio::test]
    async fn process_loop_keeps_processed_marker_as_high_water_mark() {
        let app = test_app_state_with_completer("formation_marker_high_water", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_marker_high_water").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;
        space
            .conversations
            .save_extension("brain_processed".to_string(), 9_u64.into())
            .await
            .unwrap();

        space
            .formation
            .processing_conversation
            .store(pending._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, pending.clone()).await;

        // Reprocessing an older conversation must not rewind the marker.
        assert_eq!(space.formation.get_processed(), Some(9));
        assert_eq!(
            space
                .memory
                .get_conversation(pending._id)
                .await
                .unwrap()
                .status,
            ConversationStatus::Completed
        );
    }

    #[tokio::test]
    async fn process_loop_triggers_scheduled_maintenance_at_threshold() {
        let app =
            test_app_state_with_completer("formation_process_loop_maintenance", SuccessCompleter);
        let space = create_loaded_space(&app, "formation_process_loop_maintenance").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();

        for _ in 0..20 {
            let completed = Conversation {
                user: SELF_USER_ID,
                status: ConversationStatus::Completed,
                label: Some("formation".to_string()),
                created_at: unix_ms(),
                updated_at: unix_ms(),
                ..Default::default()
            };
            space
                .memory
                .add_conversation(ConversationRef::from(&completed))
                .await
                .unwrap();
        }
        let pending = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt(None).into()],
                ..Default::default()
            })],
        )
        .await;
        assert_eq!(pending._id, 21);

        space
            .formation
            .processing_conversation
            .store(pending._id, Ordering::SeqCst);
        space.formation.process_loop(ctx, pending).await;

        assert_eq!(
            space
                .formation
                .processing_conversation
                .load(Ordering::SeqCst),
            0
        );
        for _ in 0..100 {
            if !space.is_processing() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(space.maintenance_for_test().get_processed_at().daydream, 21);
    }

    #[tokio::test]
    async fn process_one_reviews_once_at_threshold_with_receipts_and_source_bindings() {
        for tokens in [
            super::REVIEW_MIN_INPUT_TOKENS - 1,
            super::REVIEW_MIN_INPUT_TOKENS,
        ] {
            let name = format!("formation_review_{tokens}");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let app = test_app_state_with_completer(
                &name,
                ReviewCompleter {
                    requests: requests.clone(),
                    fail_on_call: None,
                },
            );
            let space = create_loaded_space(&app, &name).await;
            let ctx = space
                .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
                .unwrap();
            let prompt = "x".repeat(tokens * 4);
            assert_eq!(anda_core::estimate_tokens(&prompt), tokens);
            let mut conversation = stored_conversation(
                &space,
                vec![json!(Message {
                    role: "user".into(),
                    content: vec![prompt.clone().into()],
                    ..Default::default()
                })],
            )
            .await;

            space.formation.process_one(&ctx, &mut conversation).await;

            let expected_calls = if tokens >= super::REVIEW_MIN_INPUT_TOKENS {
                4
            } else {
                2
            };
            assert_eq!(conversation.status, ConversationStatus::Completed);
            assert_eq!(conversation.usage.input_tokens, expected_calls as u64 * 10);
            assert_eq!(space.formation.history.read().len(), 1);
            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests.len(), expected_calls);
            let receipt = requests[1]
                .content
                .iter()
                .find_map(|part| match part {
                    ContentPart::ToolOutput { output, .. } => Some(output),
                    _ => None,
                })
                .unwrap();
            assert_eq!(receipt["status"], "succeeded");
            assert!(receipt["results"][0]["result"]["handles"]["receipt"].is_string());
            if expected_calls == 4 {
                assert_eq!(
                    request_text(&requests[2])
                        .matches(super::REVIEW_INSTRUCTIONS)
                        .count(),
                    1
                );
                assert!(
                    requests[2].raw_history.iter().any(|item| {
                        item["role"] == "tool"
                            && item["content"].as_str() == Some(receipt.to_string().as_str())
                    }),
                    "review must retain the actual write receipt"
                );
                assert!(
                    requests[2]
                        .raw_history
                        .iter()
                        .any(|item| item["content"] == prompt)
                );
                let readback = requests[3]
                    .content
                    .iter()
                    .find_map(|part| match part {
                        ContentPart::ToolOutput { output, .. } => Some(output),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(readback["status"], "succeeded", "{readback:#}");
                assert!(
                    readback["results"][0]["result"][0]["payload"]["inline"]["content"][0]["text"]
                        == prompt,
                    "source readback: {}",
                    readback.to_string().chars().take(2000).collect::<String>()
                );
                assert_eq!(
                    serde_json::to_string(&conversation.messages)
                        .unwrap()
                        .matches("Host review scope:")
                        .count(),
                    1
                );
            }
            let stored = space
                .memory
                .get_conversation(conversation._id)
                .await
                .unwrap();
            assert_eq!(stored.status, ConversationStatus::Completed);
            assert_eq!(json!(stored.usage), json!(conversation.usage));
            space.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn process_one_does_not_complete_a_failed_initial_or_review_pass() {
        for fail_on_call in [0, 2] {
            let name = format!("formation_review_failure_{fail_on_call}");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let app = test_app_state_with_completer(
                &name,
                ReviewCompleter {
                    requests: requests.clone(),
                    fail_on_call: Some(fail_on_call),
                },
            );
            let space = create_loaded_space(&app, &name).await;
            let ctx = space
                .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
                .unwrap();
            let mut conversation = stored_conversation(
                &space,
                vec![json!(Message {
                    role: "user".into(),
                    content: vec!["x".repeat(40_000).into()],
                    ..Default::default()
                })],
            )
            .await;

            space.formation.process_one(&ctx, &mut conversation).await;

            assert_eq!(requests.lock().unwrap().len(), fail_on_call + 1);
            assert_eq!(conversation.status, ConversationStatus::Failed);
            assert_eq!(
                conversation.failed_reason.as_deref(),
                Some("review fixture failure")
            );
            assert_eq!(
                conversation.usage.input_tokens,
                (fail_on_call as u64 + 1) * 10
            );
            assert!(space.formation.history.read().is_empty());
            assert_eq!(
                stored_status(&space, conversation._id).await,
                ConversationStatus::Failed
            );
            space.close().await.unwrap();
        }
    }

    #[test]
    fn review_scope_identifies_the_actual_captured_message_window() {
        let prompt = super::review_prompt(42, 20);
        assert!(prompt.contains("formation conversation 42, 20 input messages"));
        assert!(prompt.contains(":msg1 through :msg16 refer to input messages 5 through 20"));
        assert!(super::review_prompt(43, 0).contains("No input-message Evidence bindings"));
    }

    #[tokio::test]
    async fn process_one_compacts_before_large_prompt_review_handoff() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let models = models_with_configured_completer(
            CompactionCompleter {
                requests: requests.clone(),
            },
            |model| model.context_window = 1000,
        );
        let app = app_state_core(
            "formation_compacts_before_review",
            models,
            vec![],
            "test",
            0,
        );
        let space = create_loaded_space(&app, "formation_compacts_before_review").await;
        let ctx = space
            .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
            .unwrap();
        // ~44k chars ≈ 11k estimated tokens, above the 10k-token review threshold
        let large_text = "x".repeat(44_000);
        let mut conversation = stored_conversation(
            &space,
            vec![json!(Message {
                role: "user".to_string(),
                content: vec![formation_prompt_with_text(&large_text, None).into()],
                ..Default::default()
            })],
        )
        .await;

        space.formation.process_one(&ctx, &mut conversation).await;

        assert_eq!(conversation.status, ConversationStatus::Completed);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "{requests:#?}");
        assert!(requests[1].contains(COMPACTION_PROMPT.trim()));
        assert!(requests[2].contains("handoff summary"));
        assert!(requests[2].contains(super::REVIEW_INSTRUCTIONS));
        assert!(requests[2].contains(&format!(
            "formation conversation {}, 1 input messages",
            conversation._id
        )));
        assert!(
            !requests[1].contains(super::REVIEW_INSTRUCTIONS),
            "queued review must survive rather than be folded into the handoff"
        );

        let messages = serde_json::to_string(&conversation.messages).unwrap();
        assert!(messages.contains("draft before compaction"));
        assert!(messages.contains("handoff summary"));
        assert!(messages.contains("reviewed after compaction"));
    }
}

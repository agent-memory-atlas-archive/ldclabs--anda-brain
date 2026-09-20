use anda_core::{
    Agent, AgentContext, AgentOutput, BoxError, CompletionRequest, Document, Documents,
    FunctionDefinition, Json, Message, ModelEffort, Resource, StateFeatures, Tool, ToolOutput,
    estimate_tokens,
};
use anda_db::collection::Collection;
use anda_engine::{
    context::{AgentCtx, BaseCtx},
    extension::note::{load_notes, load_notes_from_legacy},
    local_date_hour,
    memory::{
        Conversation, ConversationRef, ConversationStatus, Conversations, KipArgs,
        MemoryManagement, MemoryReadonly, READONLY_FUNCTION_DEFINITION,
    },
    unix_ms,
};
use parking_lot::RwLock;
use serde_json::json;
use std::sync::atomic::AtomicU64;
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::time::timeout;

use anda_kip::{KipError, KipErrorCode, Response};

use crate::kip;

use super::{
    BrainHook, SELF_USER_ID, append_runner_history, compact_runner_if_needed,
    push_completed_history,
};
use crate::types::{MemoryPolicy, RecallInput};
mod budgeted;
#[cfg(feature = "wiki")]
use crate::wiki::{WikiReadTool, WikiSearchTool};

const RECALL_CONTEXT_TIMEOUT: Duration = Duration::from_secs(5);
const RECALL_TOTAL_TIMEOUT: Duration = Duration::from_secs(180);
/// Fallback model-turn cap, used when the space has no policy of its own.
/// Equal to `MemoryPolicy::default_recall_max_rounds`, so an unset policy is
/// not a behavior change.
const RECALL_MAX_MODEL_TURNS: usize = 7;
const RECALL_HISTORY_LIMIT: usize = 1;
pub const READONLY_KIP_TIMEOUT: Duration = Duration::from_secs(15);

pub static FUNCTION_DEFINITION: LazyLock<FunctionDefinition> = LazyLock::new(|| {
    serde_json::from_value(json!({
        "name": "recall_memory",
        "description": "Recall information from the assistant's long-term memory (the Cognitive Nexus owned by $self). Use only for information that is not already present in the active conversation. Do not call for facts just mentioned, just submitted to formation, or otherwise available in current context; formation is asynchronous and fresh memories may take a minute or more to become searchable.",
        "parameters": {
            "type": "object",
            "properties": {
            "budget": {
                "type": ["object", "null"],
                "description": "Optional hard-budget memory packet mode. The host returns selected authorized items and coverage, not a free-form answer. A Space policy may enforce tighter limits. Null preserves the policy/default behavior.",
                "properties": {
                    "tokenizer": {"type":"string","enum":["o200k_base@tiktoken-rs-0.12.0"]},
                    "max_tokens": {"type":"integer","minimum":1,"maximum":65536},
                    "context_tokens": {"type":"integer","minimum":1,"maximum":131072}
                },
                "required": ["tokenizer","max_tokens","context_tokens"],
                "additionalProperties": false
            },
            "query": {
                "type": "string",
                "description": "A natural language question about older or out-of-context memory. Be specific and include the subject, timeframe, and topic when known. Examples: 'What do we know about the current user's communication preferences?', 'What happened in our last discussion about Project Aurora?', 'Who are the members of the engineering team?'"
            },
            "context": {
                "type": [
                    "object",
                    "null"
                ],
                "description": "Optional current conversational context used only to disambiguate the query within $self's memory. Pass an object, not a JSON string. It does not change the memory owner.",
                "properties": {
                "counterparty": {
                    "type": [
                        "string",
                        "null"
                    ],
                    "description": "Preferred. Durable identifier of the current external person or organization interacting with the business agent. Useful for resolving implicit references such as 'the current user', 'they', or omitted subjects."
                },
                "agent": {
                    "type": [
                        "string",
                        "null"
                    ],
                    "description": "The identifier of the calling business agent, if applicable. Useful for provenance or caller-specific queries, but it does not change whose memory is searched."
                },
                "source": {
                    "type": [
                        "string",
                        "null"
                    ],
                    "description": "Identifier of the current source, thread, channel, or app context. Useful when the query refers to a previous discussion in the same place."
                },
                "topic": {
                    "type": [
                        "string",
                        "null"
                    ],
                    "description": "The topic of the current conversation, to help disambiguate the query."
                }
                },
                "required": [
                    "counterparty",
                    "agent",
                    "source",
                    "topic"
                ],
                "additionalProperties": false
            }
            },
            "required": ["query", "context", "budget"],
            "additionalProperties": false
        },
        "strict": true
        })).unwrap()
});

#[derive(Clone)]
pub struct TimedMemoryReadonly {
    memory: Arc<MemoryManagement>,
    timeout: Duration,
    clock: Arc<crate::runtime::BusinessClock>,
    attention: Option<Arc<crate::attention::AttentionRuntime>>,
}

impl TimedMemoryReadonly {
    pub fn new(memory: Arc<MemoryManagement>) -> Self {
        Self {
            memory,
            timeout: READONLY_KIP_TIMEOUT,
            clock: Arc::new(crate::runtime::BusinessClock::default()),
            attention: None,
        }
    }
    pub(crate) fn with_clock(mut self, clock: Arc<crate::runtime::BusinessClock>) -> Self {
        self.clock = clock;
        self
    }
    pub(crate) fn with_attention(
        mut self,
        attention: Arc<crate::attention::AttentionRuntime>,
    ) -> Self {
        self.attention = Some(attention);
        self
    }
}

impl Tool<BaseCtx> for TimedMemoryReadonly {
    type Args = KipArgs;
    type Output = Response;

    fn name(&self) -> String {
        MemoryReadonly::NAME.to_string()
    }

    fn description(&self) -> String {
        READONLY_FUNCTION_DEFINITION.description.clone()
    }

    fn definition(&self) -> FunctionDefinition {
        // The definition `anda_kip` ships with the protocol it describes, so
        // the tool the model is shown and the envelope the engine executes stay
        // in step across protocol revisions.
        READONLY_FUNCTION_DEFINITION.clone()
    }

    async fn call(
        &self,
        _ctx: BaseCtx,
        args: Self::Args,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Self::Output>, BoxError> {
        let mut request = match args.into_readonly_request() {
            Ok(request) => request,
            Err(err) => return Ok(error_output(Response::from(err))),
        };
        self.clock.bind_read(&mut request)?;
        let nexus = self.memory.nexus();
        let res = match timeout(
            self.timeout,
            kip::execute_readonly_request(nexus.as_ref(), &request),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => Response::failed(KipError::new(
                KipErrorCode::ExecutionTimeout,
                format!(
                    "read-only KIP execution timed out after {} seconds; memory is busy, retry later",
                    self.timeout.as_secs()
                ),
            )),
        };

        if let Some(attention) = &self.attention {
            attention.notice_read(&request, &res);
        }
        Ok(error_output(res))
    }
}

/// Wraps a KIP response as a tool output.
///
/// Anything short of `succeeded` is flagged as an error, `partial` included: a
/// batch where one operation failed is not a clean result, and the
/// per-operation detail the model needs to tell which is already in the
/// payload.
fn error_output(res: Response) -> ToolOutput<Response> {
    let is_error = (!kip::succeeded(&res)).then_some(true);
    let mut output = ToolOutput::new(res);
    output.is_error = is_error;
    output
}

/// Reads the owning space's current [`MemoryPolicy`].
///
/// A closure rather than a stored snapshot: `update_space` can change the
/// policy while the agent is alive, and a cap read once at construction is a
/// cap that quietly ignores the operator who raised it.
pub type MemoryPolicyReader = Arc<dyn Fn() -> MemoryPolicy + Send + Sync>;

#[derive(Clone)]
pub struct RecallAgent {
    utility: Option<std::sync::Weak<crate::consequence::utility::UtilityRuntime>>,
    receipts: Option<Arc<crate::recall_receipt::RecallReceipts>>,
    prompt: Arc<str>,
    pub conversations: Conversations,
    /// The collection backing `conversations`. `Conversations` wraps document
    /// access only, so cursor paging in `Space::list_conversations` goes
    /// through this handle.
    pub conversations_collection: Arc<Collection>,
    memory: Arc<MemoryManagement>,
    hook: Arc<dyn BrainHook>,
    history: Arc<RwLock<VecDeque<Document>>>,
    max_input_tokens: usize,
    policy: MemoryPolicyReader,
    clock: Arc<crate::runtime::BusinessClock>,
}

impl RecallAgent {
    pub const NAME: &'static str = "recall_memory";
    pub fn new(
        memory: Arc<MemoryManagement>,
        conversations: Conversations,
        conversations_collection: Arc<Collection>,
        hook: Arc<dyn BrainHook>,
        max_input_tokens: usize,
        policy: MemoryPolicyReader,
    ) -> Self {
        Self {
            prompt: super::prompts::active_prompt(super::prompts::PromptTarget::Recall),
            receipts: None,
            utility: None,
            clock: Arc::new(crate::runtime::BusinessClock::default()),
            conversations,
            conversations_collection,
            memory,
            hook,
            history: Arc::new(RwLock::new(VecDeque::new())),
            max_input_tokens,
            policy,
        }
    }

    /// Retained for caller compatibility. Primers are now read fresh because
    /// trust, identity and authorization can change without a schema publish.
    pub fn with_schema_generation(self, _generation: Arc<AtomicU64>) -> Self {
        self
    }
    pub(crate) fn with_receipts(
        mut self,
        receipts: Arc<crate::recall_receipt::RecallReceipts>,
    ) -> Self {
        self.receipts = Some(receipts);
        self
    }
    pub(crate) fn with_utility(
        mut self,
        utility: std::sync::Weak<crate::consequence::utility::UtilityRuntime>,
    ) -> Self {
        self.utility = Some(utility);
        self
    }

    /// The model-turn cap for one recall run.
    ///
    /// `recall_max_rounds` was declared as a policy knob and read by nothing;
    /// its default happens to equal the compiled fallback, so the gap was
    /// invisible until an operator raised it and nothing changed.
    ///
    /// `MemoryPolicy::validate` holds the field in `[1, 50]`, so a zero can
    /// only arrive from a stored policy that predates the check. Treating it
    /// as unset rather than as "no turns at all" keeps such a space answering
    /// recalls instead of failing every one of them instantly.
    fn max_model_turns(&self) -> usize {
        match (self.policy)().recall_max_rounds as usize {
            0 => RECALL_MAX_MODEL_TURNS,
            rounds => rounds,
        }
    }

    pub(crate) fn with_prompt(mut self, prompt: Arc<str>) -> Self {
        self.prompt = prompt;
        self
    }

    pub(crate) fn with_clock(mut self, clock: Arc<crate::runtime::BusinessClock>) -> Self {
        self.clock = clock;
        self
    }

    #[cfg(feature = "experiments")]
    pub(crate) fn clear_history(&self) {
        self.history.write().clear();
    }

    pub async fn init(&self) -> Result<(), BoxError> {
        let (conversations, _) = self
            .conversations
            .list_conversations_by_user(&SELF_USER_ID, None, Some(3))
            .await?;
        // Only completed conversations belong in the model context, matching
        // the runtime push_completed_history behavior. The list is newest
        // first while the runtime queue runs oldest -> newest, so reverse it;
        // otherwise the next push_back would evict the newest entry first.
        let mut history: Vec<Conversation> = conversations
            .into_iter()
            .filter(|c| {
                c.status == ConversationStatus::Completed
                    && c._id
                        > self
                            .conversations_collection
                            .get_extension_as::<u64>("history_boundary")
                            .unwrap_or(0)
            })
            .take(RECALL_HISTORY_LIMIT)
            .collect();
        history.reverse();
        *self.history.write() = history.into_iter().map(Document::from).collect();
        Ok(())
    }

    /// The Person Concept a counterparty handle keys, or `Json::Null`.
    ///
    /// The handle is the Concept's `key` — immutable Space-local identity —
    /// which is what Formation writes it under. Reading by `name` would resolve
    /// through a mutable display label and could match more than one Person.
    pub async fn get_counterparty(&self, counterparty: &str) -> Result<Json, BoxError> {
        let found = self
            .memory
            .query(
                super::PERSON_BY_KEY,
                Some(serde_json::Map::from_iter([(
                    "key".to_string(),
                    Json::from(counterparty),
                )])),
            )
            .await?;
        Ok(super::first_row(found))
    }

    async fn get_counterparty_with_timeout(&self, counterparty: Option<String>) -> Option<Json> {
        let counterparty = counterparty?;

        match timeout(RECALL_CONTEXT_TIMEOUT, self.get_counterparty(&counterparty)).await {
            Ok(Ok(info)) => Some(info),
            Ok(Err(err)) => {
                log::debug!(
                    target: "brain",
                    counterparty;
                    "recall counterparty profile not available: {err:?}"
                );
                None
            }
            Err(_) => {
                log::warn!(
                    target: "brain",
                    counterparty;
                    "recall counterparty profile lookup timed out"
                );
                None
            }
        }
    }

    async fn describe_primer_fresh(&self) -> Json {
        // Governance, trust and identity can change without a vocabulary publish.
        // A fresh primer carries the live control basis; a TTL cannot validate it.
        match timeout(RECALL_CONTEXT_TIMEOUT, self.memory.describe_primer()).await {
            Ok(Ok(primer)) => primer,
            Ok(Err(err)) => {
                log::warn!(target: "brain", "recall primer not available: {err:?}");
                Json::default()
            }
            Err(_) => {
                log::warn!(target: "brain", "recall primer lookup timed out");
                Json::default()
            }
        }
    }

    async fn load_recall_notes(ctx: &AgentCtx) -> Json {
        let notes = match load_notes(ctx).await {
            Some(n) => n,
            None => load_notes_from_legacy(ctx).await.unwrap_or_default(),
        };
        serde_json::to_value(notes.items).unwrap_or_default()
    }

    async fn persist_conversation(&self, conversation: &Conversation) {
        match conversation.to_changes() {
            Ok(changes) => {
                let _ = self
                    .conversations
                    .update_conversation(conversation._id, changes)
                    .await;
            }
            Err(err) => {
                log::error!(
                    target: "brain",
                    "Failed to serialize recall conversation {} changes: {:?}",
                    conversation._id,
                    err
                );
            }
        }
    }

    /// Reads the canonical budget recorded by budget mode. Unbudgeted raw or
    /// structured prompts return `None`.
    pub(crate) async fn conversation_budget(
        &self,
        conversation: u64,
    ) -> Result<Option<crate::recall_budget::RecallBudget>, BoxError> {
        let conversation = self.conversations.get_conversation(conversation).await?;
        let first = conversation
            .messages
            .first()
            .ok_or("recall conversation has no input message")?;
        let message: Message = serde_json::from_value(first.clone())?;
        let prompt = message
            .content
            .iter()
            .find_map(|part| match part {
                anda_core::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .ok_or("recall conversation has no text input")?;
        Ok(RecallInput::parse_prompt(prompt)?.and_then(|input| input.budget))
    }

    async fn failed_output(
        &self,
        mut conversation: Conversation,
        reason: String,
        last_output: Option<AgentOutput>,
    ) -> AgentOutput {
        conversation.status = ConversationStatus::Failed;
        conversation.failed_reason = Some(reason.clone());
        conversation.updated_at = unix_ms();
        self.persist_conversation(&conversation).await;
        self.hook
            .on_conversation_end(Self::NAME, &conversation)
            .await;

        log::warn!(target: "brain", "recall failed: {reason}");

        let mut output = last_output.unwrap_or_default();
        output.conversation = Some(conversation._id);
        if let Some(receipts) = &self.receipts
            && let Err(error) = receipts
                .issue_legacy(
                    conversation._id,
                    crate::assess::split_recall_meta(&output.content).0,
                    &conversation.messages,
                )
                .await
        {
            log::warn!(target:"brain","failed Recall delivery receipt unavailable: {error}");
        }
        let doc = Document::from(conversation.clone());
        output.failed_reason = Some(format!("{reason}\n\n{doc}"));
        output
    }
}

fn recall_time_remaining(started_at: u64) -> Option<Duration> {
    let elapsed = Duration::from_millis(unix_ms().saturating_sub(started_at));
    RECALL_TOTAL_TIMEOUT.checked_sub(elapsed)
}

/// Terminal failure modes of the recall runner loop, converted into a reason
/// string or propagated error at the single post-loop exit in [`Agent::run`].
enum RecallFailure {
    TurnLimit,
    Timeout,
    Runner(BoxError),
}

/// Implementation of the [`Agent`] trait for RecallAgent.
impl Agent<AgentCtx> for RecallAgent {
    /// Returns the agent's name identifier
    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    /// Returns a description of the agent's purpose and capabilities.
    fn description(&self) -> String {
        FUNCTION_DEFINITION.description.clone()
    }

    fn definition(&self) -> FunctionDefinition {
        FUNCTION_DEFINITION.clone()
    }

    /// Returns a list of tool names that this agent depends on
    fn tool_dependencies(&self) -> Vec<String> {
        #[allow(unused_mut)]
        let mut tools = vec![
            MemoryReadonly::NAME.to_string(),
            crate::kip_reference::KipReferenceTool::NAME.to_string(),
        ];
        #[cfg(feature = "learning")]
        tools.push(crate::learning::recall::ProcedureStatusTool::NAME.to_string());
        #[cfg(feature = "wiki")]
        tools.extend([
            WikiSearchTool::NAME.to_string(),
            WikiReadTool::NAME.to_string(),
        ]);
        tools
    }

    async fn run(
        &self,
        ctx: AgentCtx,
        prompt: String, // RecallInput serialized as JSON string
        _resources: Vec<Resource>,
    ) -> Result<AgentOutput, BoxError> {
        let budget_input = RecallInput::parse_prompt(&prompt)?;
        if let Some(budget) = crate::recall_budget::RecallBudget::resolve(
            (self.policy)().recall_budget.as_ref(),
            budget_input
                .as_ref()
                .and_then(|input| input.budget.as_ref()),
        )? {
            return self.run_budgeted(ctx, prompt, budget_input, budget).await;
        }
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

        let parsed_input = serde_json::from_str::<RecallInput>(&prompt).ok();
        let mut conversation = Conversation {
            user: *caller,
            messages: vec![serde_json::json!(Message {
                role: "user".into(),
                content: vec![prompt.clone().into()],
                timestamp: Some(now_ms),
                ..Default::default()
            })],
            status: ConversationStatus::Working,
            period: now_ms / 3600 / 1000,
            created_at: now_ms,
            updated_at: now_ms,
            label: Some("recall".to_string()),
            ..Default::default()
        };

        let id = self
            .conversations
            .add_conversation(ConversationRef::from(&conversation))
            .await?;
        conversation._id = id;

        let counterparty = parsed_input
            .as_ref()
            .and_then(|input| input.context.as_ref())
            .and_then(|ctx| ctx.counterparty.clone());

        let (counterparty_info, primer, notes) = tokio::join!(
            self.get_counterparty_with_timeout(counterparty),
            self.describe_primer_fresh(),
            Self::load_recall_notes(&ctx),
        );

        // add bounded history conversations to provide context without bloating
        // every recall request.
        let chat_history: Vec<Document> = { self.history.read().iter().cloned().collect() };

        let chat_history = if chat_history.is_empty() {
            vec![]
        } else {
            vec![Message {
                role: "user".into(),
                content: vec![
                    Documents::new("history_recall".to_string(), chat_history)
                        .to_string()
                        .into(),
                ],
                name: Some("$system".into()),
                timestamp: Some(now_ms),
                ..Default::default()
            }]
        };

        let mut runner = ctx.clone().completion_iter(
            CompletionRequest {
                instructions: format!(
                    "{}\n\n---\n\n{}\n\n---\n\n# `DESCRIBE PRIMER` Result:\n{}\n\n---\n\n# Your Notes:\n{}\n\n# Counterparty profile:\n{}\n\n# Current Datetime: {}",
                    super::prompts::mode_reference(super::prompts::PromptTarget::Recall),
                    self.prompt,
                    primer,
                    serde_json::to_string(&notes).unwrap_or_default(),
                    serde_json::to_string(&counterparty_info).unwrap_or_default(),
                    local_date_hour(self.clock.now_ms()).unwrap_or_default()
                ),
                prompt,
                chat_history,
                tools: ctx.tool_definitions(Some(&self.tool_dependencies())),
                tool_choice_required: true,
                effort: Some(ModelEffort::Medium),
                ..Default::default()
            },
            vec![],
        );

        let max_model_turns = self.max_model_turns();
        let started_at = now_ms;
        let mut replace_initial_input = true;
        let mut persisted_runner_history_len = 0;
        let mut last_output: Option<AgentOutput> = None;
        let mut total_model_turns = 0usize;
        let mut accounted_runner_turns = 0usize;
        let mut unpersisted_turns = 0usize;
        // Every failure exits through the labeled break so usage backfill and
        // the failure handling live at exactly one place below the loop.
        let failure: Option<RecallFailure> = 'run: {
            loop {
                if total_model_turns >= max_model_turns {
                    break 'run Some(RecallFailure::TurnLimit);
                }

                let Some(remaining) = recall_time_remaining(started_at) else {
                    break 'run Some(RecallFailure::Timeout);
                };

                match timeout(remaining, compact_runner_if_needed(&mut runner)).await {
                    Ok(Ok(true)) => {
                        // A compaction that lands exactly on the turn limit is
                        // caught by the check at the top of the next iteration.
                        total_model_turns = total_model_turns.saturating_add(1);
                        accounted_runner_turns = runner.turns();
                        persisted_runner_history_len = 0;
                        replace_initial_input = false;
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(err)) => break 'run Some(RecallFailure::Runner(err)),
                    Err(_) => break 'run Some(RecallFailure::Timeout),
                }

                let Some(remaining) = recall_time_remaining(started_at) else {
                    break 'run Some(RecallFailure::Timeout);
                };

                match timeout(remaining, runner.next()).await {
                    Err(_) => break 'run Some(RecallFailure::Timeout),
                    Ok(Ok(None)) => break 'run None,
                    Ok(Ok(Some(mut output))) => {
                        let runner_turns = runner.turns();
                        total_model_turns = total_model_turns
                            .saturating_add(runner_turns.saturating_sub(accounted_runner_turns));
                        accounted_runner_turns = runner_turns;

                        let is_done = runner.is_done();
                        append_runner_history(
                            &mut conversation,
                            &output.chat_history,
                            &mut persisted_runner_history_len,
                            &mut replace_initial_input,
                        );
                        conversation.status = if output.failed_reason.is_some() {
                            ConversationStatus::Failed
                        } else if is_done {
                            ConversationStatus::Completed
                        } else {
                            ConversationStatus::Working
                        };
                        conversation.usage = output.usage.clone();
                        conversation.updated_at = unix_ms();

                        if let Some(ref failed_reason) = output.failed_reason {
                            conversation.failed_reason = Some(failed_reason.clone());
                        } else {
                            conversation.failed_reason = None;
                            push_completed_history(
                                &self.history,
                                &conversation,
                                RECALL_HISTORY_LIMIT,
                            );
                        }

                        // Persisting rewrites the full message array (O(turns^2)
                        // over a session), so intermediate Working turns are
                        // throttled; terminal statuses always persist. See
                        // PERSIST_EVERY_N_TURNS.
                        unpersisted_turns = unpersisted_turns.saturating_add(1);
                        if conversation.status != ConversationStatus::Working
                            || unpersisted_turns >= super::PERSIST_EVERY_N_TURNS
                        {
                            self.persist_conversation(&conversation).await;
                            unpersisted_turns = 0;
                        }
                        output.conversation = Some(conversation._id);
                        last_output = Some(output);

                        if conversation.status == ConversationStatus::Failed
                            || conversation.status == ConversationStatus::Completed
                        {
                            break 'run None;
                        }
                    }
                    Ok(Err(err)) => break 'run Some(RecallFailure::Runner(err)),
                }
            }
        };

        // Single failure exit. The usage snapshot happens after the error
        // occurred: failure can strike after usage was accumulated but before
        // it was copied from a runner output (e.g. right after a compaction
        // handoff), so the runner total is synced here on every failure path
        // to avoid undercounting token accounting.
        if let Some(failure) = failure {
            conversation.usage = runner.total_usage().clone();
            return match failure {
                RecallFailure::TurnLimit => {
                    let reason = format!("recall exceeded model turn limit of {max_model_turns}");
                    Ok(self.failed_output(conversation, reason, last_output).await)
                }
                RecallFailure::Timeout => {
                    let reason = format!(
                        "recall timed out after {} seconds",
                        RECALL_TOTAL_TIMEOUT.as_secs()
                    );
                    Ok(self.failed_output(conversation, reason, last_output).await)
                }
                RecallFailure::Runner(err) => {
                    conversation.status = ConversationStatus::Failed;
                    conversation.failed_reason = Some(err.to_string());
                    conversation.updated_at = unix_ms();
                    self.persist_conversation(&conversation).await;
                    self.hook
                        .on_conversation_end(Self::NAME, &conversation)
                        .await;
                    Err(err)
                }
            };
        }

        // Terminal exits above always persist (any non-Working status forces
        // a write) and failure paths return early after persisting, so only a
        // Working exit — the runner returning `Ok(None)` — can still hold
        // turns skipped by the throttle.
        if unpersisted_turns > 0 && conversation.status == ConversationStatus::Working {
            self.persist_conversation(&conversation).await;
        }

        self.hook
            .on_conversation_end(Self::NAME, &conversation)
            .await;
        let output = last_output.ok_or("completion runner returned no output")?;
        if let Some(receipts) = &self.receipts {
            receipts
                .issue_legacy(
                    conversation._id,
                    crate::assess::split_recall_meta(&output.content).0,
                    &conversation.messages,
                )
                .await?;
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::{FUNCTION_DEFINITION, READONLY_KIP_TIMEOUT, RecallAgent};
    use crate::{
        agents::SELF_USER_ID,
        space::AppState,
        testkit::{app_state_core, create_loaded_space, models_with_configured_completer},
        types::{InputContext, RecallInput},
    };
    use anda_core::{
        Agent, AgentOutput, BoxError, BoxPinFut, CompletionRequest, Message, ToolCall, Usage,
    };
    use anda_engine::{
        context::AgentCtx,
        memory::ConversationStatus,
        model::{CompletionFeaturesDyn, Model},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    #[derive(Debug)]
    struct FinalCompleter;

    impl CompletionFeaturesDyn for FinalCompleter {
        fn model_name(&self) -> String {
            "recall-final-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "answer".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("answered: {}", req.prompt).into()],
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
            "recall-failed-reason-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    failed_reason: Some("recall failed".to_string()),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec!["recall failure".to_string().into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct ErrorCompleter;

    impl CompletionFeaturesDyn for ErrorCompleter {
        fn model_name(&self) -> String {
            "recall-error-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move { Err("model error".into()) })
        }
    }

    #[derive(Debug)]
    struct EmptyHistoryCompleter;

    impl CompletionFeaturesDyn for EmptyHistoryCompleter {
        fn model_name(&self) -> String {
            "recall-empty-history-test-model".to_string()
        }

        fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "no output".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    #[derive(Debug)]
    struct CompactingToolLoopCompleter;

    impl CompletionFeaturesDyn for CompactingToolLoopCompleter {
        fn model_name(&self) -> String {
            "recall-compacting-tool-loop-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                let usage = Usage {
                    input_tokens: 100_000,
                    output_tokens: 1,
                    cached_tokens: 0,
                    requests: 1,
                };

                if req.tools.is_empty() {
                    return Ok(AgentOutput {
                        content: "compacted recall handoff".to_string(),
                        usage,
                        ..Default::default()
                    });
                }

                Ok(AgentOutput {
                    tool_calls: vec![ToolCall {
                        name: "execute_kip_readonly".to_string(),
                        args: serde_json::json!({"command": "DESCRIBE PRIMER"}),
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

    /// First turn emits a tool call whose usage forces compaction on the next
    /// loop iteration (with `context_window = 1`), the compaction handoff
    /// succeeds, and the following model turn errors out.
    #[derive(Debug)]
    struct CompactionThenErrorCompleter {
        calls: Arc<AtomicU64>,
    }

    impl CompletionFeaturesDyn for CompactionThenErrorCompleter {
        fn model_name(&self) -> String {
            "recall-compaction-then-error-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    return Ok(AgentOutput {
                        tool_calls: vec![ToolCall {
                            name: "execute_kip_readonly".to_string(),
                            args: serde_json::json!({"command": "DESCRIBE PRIMER"}),
                            result: None,
                            call_id: Some("t0".to_string()),
                            remote_id: None,
                        }],
                        usage: Usage {
                            input_tokens: 100_000,
                            output_tokens: 1,
                            cached_tokens: 0,
                            requests: 1,
                        },
                        ..Default::default()
                    });
                }
                if req.tools.is_empty() {
                    // Compaction handoff turn: tools are cleared for the
                    // summarization request.
                    return Ok(AgentOutput {
                        content: "compacted recall handoff".to_string(),
                        usage: Usage {
                            input_tokens: 5_000,
                            output_tokens: 1,
                            cached_tokens: 0,
                            requests: 1,
                        },
                        ..Default::default()
                    });
                }
                Err("model down".into())
            })
        }
    }

    fn test_app_state_with_completer<C>(name: &str, completer: C) -> AppState
    where
        C: CompletionFeaturesDyn,
    {
        test_app_state_with_configured_completer(name, completer, |_| {})
    }

    fn test_app_state_with_configured_completer<C, F>(
        name: &str,
        completer: C,
        configure: F,
    ) -> AppState
    where
        C: CompletionFeaturesDyn,
        F: FnOnce(&mut Model),
    {
        app_state_core(
            name,
            models_with_configured_completer(completer, configure),
            vec![],
            "test",
            0,
        )
    }

    fn recall_prompt(query: &str, counterparty: Option<&str>) -> String {
        serde_json::to_string(&RecallInput {
            budget: None,
            query: query.to_string(),
            context: counterparty.map(|counterparty| InputContext {
                counterparty: Some(counterparty.to_string()),
                ..Default::default()
            }),
        })
        .unwrap()
    }

    #[test]
    fn recall_function_definition_matches_agent_contract() {
        assert_eq!(RecallAgent::NAME, "recall_memory");
        assert_eq!(FUNCTION_DEFINITION.name, RecallAgent::NAME);
        assert_eq!(FUNCTION_DEFINITION.strict, Some(true));
        assert_eq!(
            FUNCTION_DEFINITION
                .parameters
                .pointer("/properties/query/type")
                .and_then(|v| v.as_str()),
            Some("string")
        );
        assert_eq!(
            FUNCTION_DEFINITION
                .parameters
                .pointer("/required")
                .and_then(|v| v.as_array())
                .map(|values| values.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
            Some(vec!["query", "context", "budget"])
        );
    }

    #[test]
    fn readonly_kip_timeout_stays_bounded() {
        assert_eq!(READONLY_KIP_TIMEOUT.as_secs(), 15);
    }

    #[tokio::test]
    async fn recall_agent_trait_metadata_matches_function_definition() {
        let app = test_app_state_with_completer("recall_trait_metadata", FinalCompleter);
        let space = create_loaded_space(&app, "recall_trait_metadata").await;

        assert_eq!(
            Agent::<AgentCtx>::name(space.recall.as_ref()),
            RecallAgent::NAME
        );
        assert_eq!(
            Agent::<AgentCtx>::description(space.recall.as_ref()),
            FUNCTION_DEFINITION.description
        );
        assert_eq!(
            Agent::<AgentCtx>::definition(space.recall.as_ref()).name,
            RecallAgent::NAME
        );
        let tools = Agent::<AgentCtx>::tool_dependencies(space.recall.as_ref());
        #[allow(unused_mut)]
        let mut expected = vec![
            "execute_kip_readonly".to_string(),
            "kip_reference".to_string(),
        ];
        #[cfg(feature = "learning")]
        expected.push("check_procedure_status".to_string());
        #[cfg(feature = "wiki")]
        expected.extend(["wiki_search".to_string(), "wiki_read".to_string()]);
        assert_eq!(tools, expected);
    }

    #[tokio::test]
    async fn recall_run_uses_history_and_tolerates_missing_counterparty_profile() {
        let app = test_app_state_with_completer("recall_history", FinalCompleter);
        let space = create_loaded_space(&app, "recall_history").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();

        let first = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx.clone(),
            recall_prompt("what is remembered?", None),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(first.conversation, Some(1));

        let second = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("what about this missing person?", Some("missing-person")),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(second.conversation, Some(2));

        let stored = space
            .get_conversation(Some("recall".to_string()), 2)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
    }

    #[tokio::test]
    async fn recall_run_persists_model_failed_reason_and_model_errors() {
        let app = test_app_state_with_completer("recall_failed_reason", FailedReasonCompleter);
        let space = create_loaded_space(&app, "recall_failed_reason").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();
        let output = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("fail this recall", None),
            vec![],
        )
        .await
        .unwrap();
        let conversation_id = output.conversation.unwrap();
        let stored = space
            .get_conversation(Some("recall".to_string()), conversation_id)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(stored.failed_reason.as_deref(), Some("recall failed"));

        let app = test_app_state_with_completer("recall_model_error", ErrorCompleter);
        let space = create_loaded_space(&app, "recall_model_error").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();
        let err = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("error this recall", None),
            vec![],
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("model error"));

        let stored = space
            .get_conversation(Some("recall".to_string()), 1)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert!(
            stored
                .failed_reason
                .as_deref()
                .unwrap()
                .contains("model error")
        );
    }

    #[tokio::test]
    async fn recall_run_preserves_input_when_chat_history_is_empty() {
        let app = test_app_state_with_completer("recall_empty_history", EmptyHistoryCompleter);
        let space = create_loaded_space(&app, "recall_empty_history").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();

        let output = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("anything stored?", None),
            vec![],
        )
        .await
        .unwrap();

        let stored = space
            .get_conversation(Some("recall".to_string()), output.conversation.unwrap())
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Completed);
        // The anomalous empty model output must not erase the original input.
        assert_eq!(stored.messages.len(), 1);
    }

    #[tokio::test]
    async fn recall_run_rejects_oversized_input_before_persisting() {
        let app = test_app_state_with_completer("recall_input_too_large", FinalCompleter);
        let space = create_loaded_space(&app, "recall_input_too_large").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();
        let prompt = "x ".repeat(1_000_000);

        let err = Agent::<AgentCtx>::run(space.recall.as_ref(), ctx, prompt, vec![])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Input too large"));
        assert_eq!(space.recall.conversations_collection.len(), 0);
    }

    #[tokio::test]
    async fn recall_failure_after_compaction_backfills_runner_usage() {
        let app = test_app_state_with_configured_completer(
            "recall_usage_backfill",
            CompactionThenErrorCompleter {
                calls: Arc::new(AtomicU64::new(0)),
            },
            |model| {
                model.context_window = 1;
            },
        );
        let space = create_loaded_space(&app, "recall_usage_backfill").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();

        let err = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("fail after compaction", None),
            vec![],
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("model down"));

        let stored = space
            .get_conversation(Some("recall".to_string()), 1)
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        // The failing turn came right after a successful compaction handoff.
        // Without backfilling from runner.total_usage() the stored usage would
        // miss the 5_000-token handoff turn and record only 100_000.
        assert_eq!(stored.usage.input_tokens, 105_000);
    }

    #[tokio::test]
    async fn recall_run_enforces_total_model_turn_limit_across_compaction() {
        let app = test_app_state_with_configured_completer(
            "recall_total_turn_limit",
            CompactingToolLoopCompleter,
            |model| {
                model.context_window = 1;
            },
        );
        let space = create_loaded_space(&app, "recall_total_turn_limit").await;
        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();

        let output = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("loop until the guardrail stops it", None),
            vec![],
        )
        .await
        .unwrap();

        let failed_reason = output.failed_reason.as_deref().unwrap_or_default();
        assert!(failed_reason.contains("recall exceeded model turn limit of 7"));

        let stored = space
            .get_conversation(Some("recall".to_string()), output.conversation.unwrap())
            .await
            .unwrap();
        assert_eq!(stored.status, ConversationStatus::Failed);
        assert_eq!(
            stored.failed_reason.as_deref(),
            Some("recall exceeded model turn limit of 7")
        );
    }

    /// `recall_max_rounds` was a declared policy knob nothing read: its
    /// default equals the compiled fallback, so raising it changed nothing
    /// and the gap was invisible. Same space, same looping model, one
    /// `update_space` apart.
    #[tokio::test]
    async fn recall_turn_limit_follows_the_space_policy() {
        use crate::types::{MemoryPolicy, UpdateSpaceInput};

        let app = test_app_state_with_configured_completer(
            "recall_policy_turn_limit",
            CompactingToolLoopCompleter,
            |model| {
                model.context_window = 1;
            },
        );
        let space = create_loaded_space(&app, "recall_policy_turn_limit").await;

        space
            .update(
                UpdateSpaceInput {
                    memory_policy: Some(MemoryPolicy {
                        recall_max_rounds: 3,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                anda_engine::unix_ms(),
            )
            .await
            .unwrap();

        let ctx = space.ctx_for_test(SELF_USER_ID, RecallAgent::NAME).unwrap();
        let output = Agent::<AgentCtx>::run(
            space.recall.as_ref(),
            ctx,
            recall_prompt("loop until the guardrail stops it", None),
            vec![],
        )
        .await
        .unwrap();

        let failed_reason = output.failed_reason.as_deref().unwrap_or_default();
        assert!(
            failed_reason.contains("recall exceeded model turn limit of 3"),
            "{failed_reason}"
        );
    }
}

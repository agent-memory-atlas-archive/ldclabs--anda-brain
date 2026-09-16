//! Explicit budget mode: the model selects authorized items; the host owns
//! reads, priority, current procedure checks and both serialization boundaries.
//! Each planning pass is a fresh request with the admitted memory snapshot.
//! Opaque provider history and a generic runner's automatic tool loop never
//! carry unbudgeted material into another model invocation.
use super::*;
use crate::recall_budget::{self as budget, Channel, Coverage, MemoryItem, Priority, RecallBudget};
use anda_core::{ContentPart, ToolInput, Usage};
use anda_kip::{Command, KipValue, MetaCommand, Request, Scalar};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

const MAX_CALLS: usize = 16;
const MAX_CALLS_PER_TURN: usize = 4;
const MAX_ROWS: usize = 32;
const MAX_BYTES: usize = 1024 * 1024;
const SELECT: &str = "select_recall_items";
const PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";
const ALL_CHANNELS: [Channel; 7] = [
    Channel::Primer,
    Channel::Notes,
    Channel::Counterparty,
    Channel::History,
    Channel::Kip,
    Channel::Wiki,
    Channel::Procedures,
];

#[derive(Default)]
struct Material {
    next_id: usize,
    failure: Option<&'static str>,
    items: Vec<MemoryItem>,
    coverage: Coverage,
    critical_missing: bool,
    skills: BTreeSet<String>,
    /// Slot -> exact Skill reference, supplied by the actual procedure tool.
    procedures: BTreeMap<String, String>,
}
impl Material {
    fn mark(&mut self, channel: Channel, partial: bool) {
        if !self.coverage.queried.contains(&channel) {
            self.coverage.queried.push(channel);
        }
        if partial && !self.coverage.partial.contains(&channel) {
            self.coverage.partial.push(channel);
        }
    }
    fn omit(&mut self, channel: Channel) {
        self.mark(channel, true);
        if !self.coverage.omitted.contains(&channel) {
            self.coverage.omitted.push(channel);
        }
    }
    fn add(
        &mut self,
        channel: Channel,
        priority: Priority,
        content: Json,
    ) -> Result<Option<String>, BoxError> {
        self.mark(channel, false);
        if content.is_null()
            || content.as_array().is_some_and(Vec::is_empty)
            || content.as_object().is_some_and(|v| v.is_empty())
        {
            return Ok(None);
        }
        if self.items.len() >= budget::MAX_ITEMS || serde_json::to_vec(&content)?.len() > MAX_BYTES
        {
            self.omit(channel);
            self.critical_missing |= matches!(priority, Priority::Required | Priority::Warning);
            return Ok(None);
        }
        let id = format!("m{}", self.next_id);
        self.next_id += 1;
        self.items.push(MemoryItem {
            id: id.clone(),
            channel,
            priority,
            content,
        });
        Ok(Some(id))
    }
    fn coverage(&self) -> Coverage {
        let mut coverage = self.coverage.clone();
        coverage.unchecked = ALL_CHANNELS
            .into_iter()
            .filter(|c| !coverage.queried.contains(c))
            .collect();
        coverage
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    selected_ids: Vec<String>,
}

fn selector() -> FunctionDefinition {
    serde_json::from_value(json!({"name":SELECT,"description":"Finish Recall by selecting existing memory item IDs. The host retains all required constraints and warnings regardless of this selection. Do not generate new content or invent IDs.","strict":true,
        "parameters":{"type":"object","properties":{"selected_ids":{"type":"array","items":{"type":"string"}}},"required":["selected_ids"],"additionalProperties":false}})).unwrap()
}

impl RecallAgent {
    pub(super) async fn run_budgeted(
        &self,
        ctx: AgentCtx,
        prompt: String,
        parsed: Option<RecallInput>,
        limits: RecallBudget,
    ) -> Result<AgentOutput, BoxError> {
        limits.validate()?;
        // A clone with its own extension map, even when a host reuses ctx.
        let mut ctx = ctx;
        ctx.base = ctx.base.with_caller(*ctx.caller());
        let now = unix_ms();
        let query = parsed
            .as_ref()
            .map(|i| i.query.clone())
            .unwrap_or_else(|| prompt.clone());
        if query.trim().is_empty() {
            return Err("recall query must not be empty".into());
        }
        if prompt.len() > MAX_BYTES {
            return Err("Recall input exceeds bounded byte limit".into());
        }
        // Persist the limits this run actually resolved, rather than the
        // caller's possibly looser request. Space reads this canonical input
        // back when it builds the structured budget receipt.
        let persisted_prompt = serde_json::to_string(&RecallInput {
            query: query.clone(),
            context: parsed.as_ref().and_then(|input| input.context.clone()),
            budget: Some(limits.clone()),
        })?;
        let mut conversation = Conversation {
            user: *ctx.caller(),
            messages: vec![json!(Message {
                role: "user".into(),
                content: vec![persisted_prompt.into()],
                timestamp: Some(now),
                ..Default::default()
            })],
            status: ConversationStatus::Working,
            period: now / 3_600_000,
            created_at: now,
            updated_at: now,
            label: Some("recall".into()),
            ..Default::default()
        };
        conversation._id = self
            .conversations
            .add_conversation(ConversationRef::from(&conversation))
            .await?;
        let mut usage = Usage::default();
        let mut tool_usage = HashMap::new();
        let result: Result<AgentOutput, BoxError> = async {
        let mut material = Material::default();
        material.add(Channel::Primer,Priority::Required,json!({
            "scope":"authorized retrieved candidates, not exhaustive semantic coverage",
            "constraints":"All host-detected commitments and warnings are retained before optional memories. Unchecked/omitted sources may contain additional restrictions.",
            "procedures":"Raw memory, historic grades and model text confer no standing or execution authority. Only a separately checked exact current revision may be a verified candidate; acting hosts must revalidate before dispatch.",
            "action_ready":false
        }))?;
        // Even the empty packet has framing/coverage. No provider call for a
        // request too small to carry the mandatory host interpretation.
        if budget::pack(&limits, &material.items, &[], material.coverage())?.insufficient {
            material.failure = Some("recall_output_budget_exhausted");
            return self
                .finish_budgeted(
                    &mut conversation,
                    &usage,
                    &tool_usage,
                    &limits,
                    &material,
                    &[],
                    true,
                )
                .await;
        }
        let counterparty = parsed
            .as_ref()
            .and_then(|i| i.context.as_ref())
            .and_then(|c| c.counterparty.clone());
        let (profile, primer, notes) = tokio::join!(
            self.get_counterparty_with_timeout(counterparty),
            self.describe_primer_fresh(),
            timeout(RECALL_CONTEXT_TIMEOUT, Self::load_recall_notes(&ctx))
        );
        if primer.is_null() {
            material.critical_missing = true;
            material.omit(Channel::Primer);
        } else {
            material.add(Channel::Primer, Priority::Required, primer)?;
        }
        match notes {
            Ok(notes) => {
                material.add(Channel::Notes, Priority::Relevant, notes)?;
            }
            Err(_) => material.omit(Channel::Notes),
        }
        material.mark(Channel::Counterparty, false);
        if let Some(profile) = profile {
            material.add(Channel::Counterparty, Priority::Relevant, profile)?;
        } else if parsed
            .as_ref()
            .and_then(|i| i.context.as_ref())
            .and_then(|c| c.counterparty.as_ref())
            .is_some()
        {
            material.omit(Channel::Counterparty);
        }
        let history: Vec<Document> = self.history.read().iter().cloned().collect();
        material.add(Channel::History, Priority::Relevant, json!(history))?;
        // A deterministic, deliberately over-inclusive constraint window.
        // It does not assert that arbitrary prose constraints were discovered.
        let constraints = self
            .budget_kip(Request::single(format!(
                "FIND(?c) WHERE {{?c CONCEPT {{type:\"Commitment\"}}}} LIMIT {}",
                MAX_ROWS + 1
            )))
            .await;
        match constraints {
            Ok(response) if kip::succeeded(&response) => {
                let rows = response.first_result().and_then(Json::as_array);
                if let Some(rows) = rows {
                    material.mark(Channel::Kip, rows.len() > MAX_ROWS);
                    if rows.len() > MAX_ROWS {
                        material.critical_missing = true;
                    }
                    for row in rows.iter().take(MAX_ROWS) {
                        material.add(Channel::Kip, Priority::Required, row.clone())?;
                    }
                } else {
                    material.critical_missing = true;
                    material.omit(Channel::Kip);
                }
            }
            _ => {
                material.critical_missing = true;
                material.omit(Channel::Kip);
            }
        }
        if material.critical_missing
            || budget::pack(&limits, &material.items, &[], material.coverage())?.insufficient
        {
            material.failure = Some(if material.critical_missing {
                "recall_required_read_incomplete"
            } else {
                "recall_output_budget_exhausted"
            });
            return self
                .finish_budgeted(
                    &mut conversation,
                    &usage,
                    &tool_usage,
                    &limits,
                    &material,
                    &[],
                    true,
                )
                .await;
        }
        let names = self.tool_dependencies();
        let mut tools = ctx.tool_definitions(Some(&names));
        tools.push(selector());
        let instructions = format!(
            "{}\n\n{}\n\n# Host budget mode\nYou are selecting an authorized memory packet for another agent. Read only through the listed tools, then call {SELECT} with existing memory item IDs. The KIP tool supports only KQL and SEARCH in this mode (up to 4 operations, LIMIT at most 32); other META commands are unavailable and the current Primer is already supplied. Wiki search uses at most 8 hits without neighbor expansion. At most 4 tool calls per pass and 16 per Recall are allowed. You cannot add content or choose priorities. No free-form answer, grades or completeness claims will be delivered. Preserve native uncertainty, conflicts and warnings. Unchecked channels are unknown, not absent. Each pass receives the entire currently admitted snapshot; previous provider history is intentionally not replayed.",
            super::super::prompts::mode_reference(super::super::prompts::PromptTarget::Recall),
            self.prompt
        );
        let template = CompletionRequest {
            instructions,
            tools,
            tool_choice_required: true,
            max_output_tokens: Some(2048),
            effort: Some(ModelEffort::Medium),
            ..Default::default()
        };
        // Resolve once without invoking the generic runner's automatic tools.
        let model = ctx
            .clone()
            .completion_iter(template.clone(), vec![])
            .model()
            .clone();
        let mut calls = 0;
        let mut selected = Vec::new();
        let mut failed = false;
        let mut finished = false;
        let mut observations = Vec::<Json>::new();
        let mut context_spent = 0usize;
        for _ in 0..self.max_model_turns().min(8) {
            let mut request = template.clone();
            // Optional items can be evicted, never a required item/warning.
            let mut visible = material.items.clone();
            loop {
                request.prompt=json!({"query":query.as_str(),"context":parsed.as_ref().and_then(|i|i.context.as_ref()),"memory_items":visible,"observations":observations,"coverage":material.coverage()}).to_string();
                let encoded = normalized_request(&request)?;
                let tokens = if encoded.len() <= 4 * MAX_BYTES {
                    budget::count(&encoded)?
                } else {
                    usize::MAX
                };
                if tokens <= (limits.context_tokens as usize).saturating_sub(context_spent) {
                    context_spent += tokens;
                    break;
                }
                let remove = visible
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| !matches!(i.priority, Priority::Required | Priority::Warning))
                    .max_by_key(|(_, i)| (i.priority, i.id.clone()))
                    .map(|(index, _)| index);
                let Some(index) = remove else {
                    failed = true;
                    material.failure = Some("recall_context_budget_exhausted");
                    break;
                };
                let removed = visible.remove(index);
                material.omit(removed.channel);
                // Also remove it from the final candidate pool: selection
                // cannot reference a record the planner was denied here.
                material.items.retain(|i| i.id != removed.id);
            }
            if failed || material.critical_missing {
                failed = true;
                break;
            }
            let Some(remaining) = recall_time_remaining(now) else {
                failed = true;
                material.failure = Some("recall_deadline_reached");
                break;
            };
            let cancel = ctx.cancellation_token();
            let response = tokio::select! {
                _=cancel.cancelled()=>None,
                result=timeout(remaining,model.completion(request))=>result.ok().and_then(Result::ok),
            };
            let Some(output) = response else {
                failed = true;
                material.failure = Some("recall_model_unavailable");
                break;
            };
            usage.accumulate(&output.usage);
            // Provider-native thought/history/artifacts never become inputs or
            // outputs of this mode. Persist only bounded public tool receipts.
            if output.failed_reason.is_some() {
                failed = true;
                material.failure = Some("recall_model_unavailable");
                break;
            }
            if output.tool_calls.is_empty() {
                if output.content.len() <= 16_384
                    && let Ok(choice) = serde_json::from_str::<Selection>(&output.content)
                {
                    selected = choice.selected_ids;
                    finished = true;
                } else {
                    failed = true;
                }
                break;
            }
            if output.tool_calls.len() > MAX_CALLS_PER_TURN
                || calls + output.tool_calls.len() > MAX_CALLS
            {
                failed = true;
                break;
            }
            for tool in output.tool_calls {
                calls += 1;
                if serde_json::to_vec(&tool.args)?.len() > 16_384 {
                    failed = true;
                    break;
                }
                if tool.name == SELECT {
                    if let Ok(choice) = serde_json::from_value::<Selection>(tool.args) {
                        selected = choice.selected_ids;
                        finished = true;
                    } else {
                        failed = true;
                    }
                    break;
                }
                if !names.contains(&tool.name) {
                    failed = true;
                    break;
                }
                if tool.name == crate::kip_reference::KipReferenceTool::NAME {
                    // Protocol text is planning context, never a retrieved memory
                    // item or evidence that a memory channel was covered. These
                    // observations pass through the cumulative token admission
                    // check before the next model call, and count as tool calls.
                    let response = self
                        .budget_tool_before_deadline(&ctx, &tool.name, tool.args, now)
                        .await;
                    let (value, is_error) = match response {
                        Ok((value, measured, is_error)) => {
                            usage.accumulate(&measured);
                            tool_usage.entry(tool.name.clone()).or_insert_with(Usage::default).accumulate(&measured);
                            (value, is_error)
                        }
                        Err(_) => (json!({"status":"unavailable","hint":"Use document=index, or section=index for exact headings, with offset=0."}), true),
                    };
                    observations.push(json!({"tool":tool.name,"reference":value,"is_error":is_error}));
                    conversation.messages.push(json!(Message {
                        role: "tool".into(),
                        content: vec![ContentPart::ToolOutput {
                            name: tool.name,
                            output: value,
                            is_error: is_error.then_some(true),
                            call_id: tool.call_id,
                            remote_id: None,
                        }],
                        ..Default::default()
                    }));
                    continue;
                }
                let channel = match tool.name.as_str() {
                    "execute_kip_readonly" => Channel::Kip,
                    "wiki_read" | "wiki_search" => Channel::Wiki,
                    "check_procedure_status" => Channel::Procedures,
                    _ => {
                        failed = true;
                        break;
                    }
                };
                let response = self
                    .budget_tool_before_deadline(&ctx, &tool.name, tool.args.clone(), now)
                    .await;
                material.mark(channel, false);
                match response {
                    Ok((value, measured, tool_error)) => {
                        usage.accumulate(&measured);
                        tool_usage
                            .entry(tool.name.clone())
                            .or_insert_with(Usage::default)
                            .accumulate(&measured);
                        if tool_error {
                            material.omit(channel);
                        }
                        let priority = if tool_error {
                            Priority::Warning
                        } else if channel == Channel::Procedures {
                            if value["recommendation_allowed"] == true {
                                Priority::VerifiedProcedure
                            } else {
                                Priority::Warning
                            }
                        } else if contains_program(&value) {
                            Priority::UnprovenProcedure
                        } else {
                            Priority::Relevant
                        };
                        discover_skills(&value, &mut material.skills);
                        if let Some(id) = material.add(channel, priority, value.clone())? {
                            if channel == Channel::Procedures
                                && let Some(skill) = tool.args["skill_ref"].as_str()
                            {
                                material.procedures.insert(id.clone(), skill.into());
                            }
                            observations
                                .push(json!({"tool":tool.name,"item_id":id,"partial":tool_error}));
                            conversation.messages.push(json!(Message {
                                role: "tool".into(),
                                content: vec![ContentPart::ToolOutput {
                                    name: tool.name.clone(),
                                    output: value,
                                    is_error: tool_error.then_some(true),
                                    call_id: tool.call_id.clone(),
                                    remote_id: None,
                                }],
                                ..Default::default()
                            }));
                        }
                    }
                    Err(_) => {
                        material.omit(channel);
                        material.add(channel,Priority::Warning,json!({"tool":tool.name,"status":"unavailable","meaning":"not evidence of absence or complete coverage"}))?;
                        observations.push(json!({"tool":tool.name,"status":"unavailable"}));
                    }
                }
                // A full result window is not an exhaustive search. A bounded
                // projection/lookup still cannot attest semantic coverage.
                if matches!(channel, Channel::Kip | Channel::Wiki) {
                    material.mark(channel, true);
                }
            }
            if failed || finished {
                break;
            }
        }
        // No free-form fallback on an exhausted planner. Required items and
        // explicit uncertainty are retained by pack; generic answers aren't.
        failed |= !finished;
        if failed && material.failure.is_none() {
            material.failure = Some("recall_planner_incomplete");
        }
        selected.retain(|id| material.items.iter().any(|item| &item.id == id));
        if !failed {
            self.refresh_budget_procedures(&ctx, &mut material, &mut usage, &mut tool_usage, now)
                .await?;
        }
        self.finish_budgeted(
            &mut conversation,
            &usage,
            &tool_usage,
            &limits,
            &material,
            &selected,
            failed,
        )
        .await
        }
        .await;
        if let Err(error) = &result {
            conversation.status = ConversationStatus::Failed;
            conversation.failed_reason = Some(format!("budgeted recall failed: {error}"));
            conversation.usage = usage;
            conversation.updated_at = unix_ms();
            self.persist_conversation(&conversation).await;
            self.hook
                .on_conversation_end(Self::NAME, &conversation)
                .await;
        }
        result
    }

    async fn budget_kip(&self, mut request: Request) -> Result<Response, BoxError> {
        self.clock.bind_read(&mut request)?;
        Ok(timeout(
            READONLY_KIP_TIMEOUT,
            kip::execute_readonly_request(self.memory.nexus().as_ref(), &request),
        )
        .await
        .map_err(|_| "bounded KIP read timed out")?)
    }

    async fn budget_tool(
        &self,
        ctx: &AgentCtx,
        name: &str,
        mut args: Json,
    ) -> Result<(Json, Usage, bool), BoxError> {
        if name == "execute_kip_readonly" {
            let args: KipArgs = serde_json::from_value(args)?;
            let mut request = args.into_readonly_request()?;
            if request.operations.len() > 4 {
                return Err("bounded Recall allows at most four KIP operations per read".into());
            }
            let parsed = request.parse_operations()?;
            let global = request.parameters.clone();
            for (op, mut cmd) in request.operations.iter_mut().zip(parsed) {
                match &mut cmd {
                    Command::Kql(query)=>query.limit=Some(capped_limit(query.limit.as_ref(),op.parameters.as_ref(),global.as_ref())?),
                    Command::Meta(MetaCommand::Search(search))=>search.limit=Some(capped_limit(search.limit.as_ref(),op.parameters.as_ref(),global.as_ref())?),
                    Command::Meta(_)=>return Err("bounded Recall supports KQL and SEARCH; broad metadata expansion is unavailable".into()),
                    Command::Kml(_)=>return Err("Recall is read-only".into()),
                }
                op.command = None;
                op.ast = Some(cmd);
            }
            let result = self.budget_kip(request).await?;
            let error = !kip::succeeded(&result);
            return Ok((
                serde_json::to_value(result)?,
                Usage {
                    requests: 1,
                    ..Default::default()
                },
                error,
            ));
        }
        if name == "wiki_search" {
            args["top_k"] = json!(args["top_k"].as_u64().unwrap_or(8).min(8));
            args["expand"] = json!(0);
        }
        let output = timeout(
            READONLY_KIP_TIMEOUT,
            ctx.tool_call(ToolInput {
                name: name.into(),
                args,
                resources: vec![],
                meta: None,
            }),
        )
        .await
        .map_err(|_| "bounded Recall tool timed out")??
        .0;
        Ok((output.output, output.usage, output.is_error == Some(true)))
    }

    async fn budget_tool_before_deadline(
        &self,
        ctx: &AgentCtx,
        name: &str,
        args: Json,
        started_at: u64,
    ) -> Result<(Json, Usage, bool), BoxError> {
        let remaining = recall_time_remaining(started_at).ok_or("Recall read deadline reached")?;
        let cancel = ctx.cancellation_token();
        tokio::select! {
            _=cancel.cancelled()=>Err("Recall read cancelled".into()),
            result=timeout(remaining,self.budget_tool(ctx,name,args))=>result.map_err(|_|"Recall read deadline reached")?,
        }
    }

    async fn refresh_budget_procedures(
        &self,
        ctx: &AgentCtx,
        material: &mut Material,
        usage: &mut Usage,
        tool_usage: &mut HashMap<String, Usage>,
        started_at: u64,
    ) -> Result<(), BoxError> {
        if !self
            .tool_dependencies()
            .iter()
            .any(|n| n == "check_procedure_status")
        {
            return Ok(());
        }
        let mut skills = material.skills.clone();
        skills.extend(material.procedures.values().cloned());
        if skills.len() > 8 {
            material.critical_missing = true;
            material.failure = Some("recall_procedure_window_incomplete");
            material.omit(Channel::Procedures);
            return Ok(());
        }
        // Drop all old status objects. No earlier true flag may survive refresh.
        material
            .items
            .retain(|i| !material.procedures.contains_key(&i.id));
        for skill in skills {
            let result = self
                .budget_tool_before_deadline(
                    ctx,
                    "check_procedure_status",
                    json!({"skill_ref":skill}),
                    started_at,
                )
                .await;
            let (content, priority) = match result {
                Ok((mut value, cost, false)) => {
                    usage.accumulate(&cost);
                    tool_usage
                        .entry("check_procedure_status".into())
                        .or_default()
                        .accumulate(&cost);
                    let valid = value["recommendation_allowed"] == true
                        && value["context_expires_at_ms"]
                            .as_u64()
                            .is_some_and(|t| t > unix_ms())
                        && value["review_due_at"].as_str().is_some_and(|t| {
                            t > crate::kip::timestamp(self.clock.now_ms()).as_str()
                        });
                    if valid {
                        (value, Priority::VerifiedProcedure)
                    } else {
                        value["recommendation_allowed"] = json!(false);
                        value["host_delivery_warning"] = json!(
                            "current applicability was not established or expired before delivery"
                        );
                        (value, Priority::Warning)
                    }
                }
                _ => (
                    json!({"skill_ref":skill,"recommendation_allowed":false,"reason":"current procedure verification unavailable"}),
                    Priority::Warning,
                ),
            };
            // The current check is mandatory even if the model omitted it.
            material.add(
                Channel::Procedures,
                if priority == Priority::VerifiedProcedure {
                    Priority::Required
                } else {
                    priority
                },
                content,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_budgeted(
        &self,
        conversation: &mut Conversation,
        usage: &Usage,
        tools_usage: &HashMap<String, Usage>,
        limits: &RecallBudget,
        material: &Material,
        selected: &[String],
        failed: bool,
    ) -> Result<AgentOutput, BoxError> {
        let mut items = material.items.clone();
        expire_procedure_checks(&mut items, self.clock.now_ms(), false);
        let mut packet = if failed || material.critical_missing {
            budget::insufficient(limits, material.coverage())?
        } else {
            budget::pack(limits, &items, selected, material.coverage())?
        };
        // If any permit expires during serialization, downgrade every positive
        // check and serialize once more; the second packet contains no permits.
        if !failed
            && !material.critical_missing
            && expire_procedure_checks(&mut items, self.clock.now_ms(), false)
        {
            expire_procedure_checks(&mut items, self.clock.now_ms(), true);
            packet = budget::pack(limits, &items, selected, material.coverage())?;
        }
        conversation.status = if packet.insufficient {
            ConversationStatus::Failed
        } else {
            ConversationStatus::Completed
        };
        let failure = packet.insufficient.then(|| {
            material
                .failure
                .unwrap_or("recall_output_budget_exhausted")
                .to_string()
        });
        conversation.failed_reason = failure.clone();
        conversation.usage = usage.clone();
        conversation.updated_at = unix_ms();
        conversation.messages.push(json!(Message {
            role: "assistant".into(),
            content: vec![packet.content.clone().into()],
            ..Default::default()
        }));
        self.persist_conversation(conversation).await;
        push_completed_history(&self.history, conversation, RECALL_HISTORY_LIMIT);
        self.hook
            .on_conversation_end(Self::NAME, conversation)
            .await;
        // This whitelist is the delivery seam, including direct agent calls.
        // No diagnostic conversation, reasoning, tool args, citations or
        // provider artifact can bypass the semantic memory packet budget.
        Ok(AgentOutput {
            content: packet.content,
            usage: usage.clone(),
            tools_usage: tools_usage.clone(),
            conversation: Some(conversation._id),
            failed_reason: failure,
            ..Default::default()
        })
    }
}

fn capped_limit(
    limit: Option<&Scalar>,
    local: Option<&serde_json::Map<String, Json>>,
    global: Option<&serde_json::Map<String, Json>>,
) -> Result<Scalar, BoxError> {
    let requested = match limit {
        None => MAX_ROWS as u64,
        Some(Scalar::Literal(KipValue::Number(n))) => n
            .as_u64()
            .ok_or("Recall LIMIT must be a nonnegative integer")?,
        Some(Scalar::Param(name)) => local
            .and_then(|p| p.get(name))
            .or_else(|| global.and_then(|p| p.get(name)))
            .and_then(Json::as_u64)
            .ok_or("Recall LIMIT parameter must resolve to a nonnegative integer")?,
        _ => return Err("Recall LIMIT must be a nonnegative integer".into()),
    };
    Ok(Scalar::Literal(KipValue::Number(
        requested.min(MAX_ROWS as u64).into(),
    )))
}

fn expire_procedure_checks(items: &mut [MemoryItem], business_now: u64, all: bool) -> bool {
    let mut changed = false;
    for item in items {
        if item.channel == Channel::Procedures
            && item.content["recommendation_allowed"] == true
            && (all
                || item.content["context_expires_at_ms"]
                    .as_u64()
                    .is_none_or(|t| t <= unix_ms())
                || item.content["review_due_at"]
                    .as_str()
                    .is_none_or(|t| t <= crate::kip::timestamp(business_now).as_str()))
        {
            item.content["recommendation_allowed"] = json!(false);
            item.content["host_delivery_warning"] =
                json!("procedure check expired before delivery; revalidation required");
            item.priority = Priority::Warning;
            changed = true;
        }
    }
    changed
}

fn contains_program(value: &Json) -> bool {
    let mut stack = vec![value];
    let mut nodes = 0;
    while let Some(value) = stack.pop() {
        nodes += 1;
        if nodes > 4096 {
            return true;
        }
        if value["schema_ref"] == format!("{PROFILE}Skill")
            || value["schema_ref"] == format!("{PROFILE}SkillRevision")
        {
            return true;
        }
        match value {
            Json::Object(map) => stack.extend(map.values()),
            Json::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    false
}

/// Exact versioned serialization of every data-bearing field used by this
/// fresh-request mode. Provider-specific message templates/billing are not
/// inferred from it. Hidden history/documents cannot bypass this contract.
fn normalized_request(request: &CompletionRequest) -> Result<String, BoxError> {
    if !request.chat_history.is_empty()
        || !request.raw_history.is_empty()
        || !request.documents.is_empty()
        || !request.content.is_empty()
        || request.output_schema.is_some()
    {
        return Err("budgeted Recall does not accept additional request material".into());
    }
    Ok(
        json!({"format":"anda-brain-recall-input/1","instructions":request.instructions,
        "prompt":request.prompt,"tools":request.tools,"max_output_tokens":request.max_output_tokens,
        "tool_choice_required":request.tool_choice_required,"effort":request.effort})
        .to_string(),
    )
}
fn discover_skills(value: &Json, into: &mut BTreeSet<String>) {
    let mut stack = vec![value];
    let mut nodes = 0;
    while let Some(value) = stack.pop() {
        nodes += 1;
        if nodes > 4096 || into.len() > 8 {
            break;
        }
        if value["schema_ref"] == format!("{PROFILE}Skill")
            && let Some(id) = value["id"].as_str()
        {
            into.insert(id.into());
        }
        match value {
            Json::Object(map) => stack.extend(map.values()),
            Json::Array(items) => stack.extend(items),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;

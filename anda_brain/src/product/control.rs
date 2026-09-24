//! Durable source suppression and write fences for product memory changes.
use anda_core::{BoxError, FunctionDefinition, Resource, Tool, ToolOutput};
use anda_db::database::AndaDB;
use anda_engine::{
    context::BaseCtx,
    extension::note::{NoteArgs, NoteOutput, NoteTool},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

pub const SOURCE_KEY: &str = "memory_product_source";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    pub key: String,
    #[serde(default)]
    pub parents: Vec<String>,
}
impl SourceIdentity {
    pub fn validate(&self) -> Result<(), BoxError> {
        if self.parents.len() > 16
            || std::iter::once(&self.key)
                .chain(self.parents.iter())
                .any(|key| key.is_empty() || key.len() > 512 || key.chars().any(char::is_control))
        {
            return Err("invalid memory source identity".into());
        }
        Ok(())
    }
    pub(crate) fn keys(&self) -> BTreeSet<String> {
        std::iter::once(self.key.clone())
            .chain(self.parents.iter().cloned())
            .collect()
    }
    pub(crate) fn for_conversation(
        conversation: &anda_engine::memory::Conversation,
    ) -> Result<Self, BoxError> {
        let source = match conversation.extra.as_ref().and_then(|v| v.get(SOURCE_KEY)) {
            Some(value) => serde_json::from_value(value.clone())?,
            None => Self {
                key: format!("formation:{}", conversation._id),
                parents: vec![],
            },
        };
        source.validate()?;
        Ok(source)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ControlState {
    pub version: u32,
    pub epoch: u64,
    /// Earliest snapshot that can be continued after the last managed change.
    #[serde(default)]
    pub read_floor: u64,
    pub suppressed: BTreeSet<String>,
    pub pending: Option<String>,
}
impl Default for ControlState {
    fn default() -> Self {
        Self {
            version: 1,
            epoch: 0,
            read_floor: 0,
            suppressed: BTreeSet::new(),
            pending: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProcessingEpoch(pub u64);

pub(crate) struct Control {
    pub gate: tokio::sync::Mutex<()>,
    pub journal: crate::journal::Journal,
    state: parking_lot::RwLock<ControlState>,
    pub tasks: crate::runtime::DurableTasks,
}
impl Control {
    pub async fn connect(db: Arc<AndaDB>) -> Result<Arc<Self>, BoxError> {
        let journal = crate::journal::Journal::new(db.object_store(), "memory-product/v1".into());
        let state = match journal.read::<ControlState>("state").await? {
            Some(state) => state.value,
            None => {
                let state = ControlState::default();
                journal.create("state", &state).await?;
                state
            }
        };
        if state.version != 1 {
            return Err("unsupported memory product state version".into());
        }
        Ok(Arc::new(Self {
            gate: tokio::sync::Mutex::new(()),
            journal,
            state: parking_lot::RwLock::new(state),
            tasks: Default::default(),
        }))
    }
    pub fn epoch(&self) -> u64 {
        self.state.read().epoch
    }
    pub fn snapshot(&self) -> ControlState {
        self.state.read().clone()
    }
    pub fn source_allowed(&self, source: &SourceIdentity) -> bool {
        self.admit_source(source).is_ok()
    }
    pub fn admit_source(
        &self,
        source: &SourceIdentity,
    ) -> Result<ProcessingEpoch, SourceAdmissionError> {
        let state = self.state.read();
        if state.pending.is_some() {
            return Err(SourceAdmissionError::Busy);
        }
        if !source.keys().is_disjoint(&state.suppressed) {
            return Err(SourceAdmissionError::Suppressed);
        }
        Ok(ProcessingEpoch(state.epoch))
    }
    pub fn current_request(&self, request: &anda_kip::Request) -> Result<(), BoxError> {
        current_request(request, self.state.read().read_floor)
    }
    pub fn available(&self) -> bool {
        self.state.read().pending.is_none()
    }
    /// Call with gate held. Publish only after conditional durable write/readback.
    pub async fn save(&self, state: ControlState) -> Result<(), BoxError> {
        let old = self
            .journal
            .read::<ControlState>("state")
            .await?
            .ok_or("memory control state missing")?;
        self.journal
            .put("state", &state, object_store::PutMode::Update(old.version))
            .await?;
        *self.state.write() = state;
        Ok(())
    }
    pub fn check(&self, ctx: &BaseCtx) -> Result<(), BoxError> {
        let state = self.state.read();
        if state.pending.is_some() {
            return Err("memory change is reconciling; retry after it finishes".into());
        }
        if ctx
            .get_state::<ProcessingEpoch>()
            .map_or(0, |epoch| epoch.0)
            != state.epoch
        {
            return Err("memory changed during processing; rebuild context before writing".into());
        }
        Ok(())
    }
}

pub(crate) struct ControlledNotes {
    inner: NoteTool,
    control: Arc<Control>,
}
impl ControlledNotes {
    pub fn new(control: Arc<Control>) -> Self {
        Self {
            inner: NoteTool::new(),
            control,
        }
    }
}
impl Tool<BaseCtx> for ControlledNotes {
    type Args = NoteArgs;
    type Output = NoteOutput;
    fn name(&self) -> String {
        self.inner.name()
    }
    fn description(&self) -> String {
        self.inner.description()
    }
    fn definition(&self) -> FunctionDefinition {
        self.inner.definition()
    }
    async fn call(
        &self,
        ctx: BaseCtx,
        args: NoteArgs,
        resources: Vec<Resource>,
    ) -> Result<ToolOutput<NoteOutput>, BoxError> {
        let _gate = self.control.gate.lock().await;
        self.control.check(&ctx)?;
        self.inner.call(ctx, args, resources).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceAdmissionError {
    Suppressed,
    Busy,
}
impl std::fmt::Display for SourceAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Suppressed => "source_suppressed",
            Self::Busy => "memory_change_pending",
        })
    }
}
impl std::error::Error for SourceAdmissionError {}

/// After a managed memory change, automatic agents may read current active
/// graph state only. Technical owner audit APIs remain separate from model tools.
fn current_request(request: &anda_kip::Request, read_floor: u64) -> Result<(), BoxError> {
    use anda_kip::{Command, DescribeTarget, MetaCommand, MutationClause, WhereClause};
    fn clauses(rows: &[WhereClause]) -> bool {
        rows.iter().all(|row| match row {
            WhereClause::Concept { matcher, .. }
            | WhereClause::Assertion { matcher, .. }
            | WhereClause::Evidence { matcher, .. }
            | WhereClause::Activity { matcher, .. } => !matcher
                .keys()
                .any(|key| key == "state" || key.starts_with("state.")),
            WhereClause::Not(rows) | WhereClause::Optional(rows) | WhereClause::Union(rows) => {
                clauses(rows)
            }
            _ => true,
        })
    }
    if request
        .read
        .as_ref()
        .is_some_and(|read| read.snapshot_token.is_some() || read.extensions.is_some())
    {
        return Err("historical memory access is disabled after a managed change".into());
    }
    for (command, operation) in request
        .parse_operations()?
        .into_iter()
        .zip(&request.operations)
    {
        let current = match command {
            Command::Kql(query) => {
                query.as_of.is_none()
                    && clauses(&query.where_clauses)
                    && current_cursor(&query, request, operation, read_floor)?
            }
            Command::Meta(MetaCommand::Search(search)) => search.as_of_seq.is_none(),
            Command::Kml(statement) => statement.clauses.iter().all(|mutation| match mutation {
                MutationClause::UpsertConcept(value) => value
                    .r#match
                    .as_ref()
                    .is_none_or(|matcher| !matcher.contains_key("state")),
                MutationClause::Update(value) => value
                    .where_clauses
                    .as_ref()
                    .is_none_or(|rows| clauses(rows)),
                MutationClause::Transition(value) => value
                    .where_clauses
                    .as_ref()
                    .is_none_or(|rows| clauses(rows)),
                MutationClause::SetRetention(value) => value
                    .where_clauses
                    .as_ref()
                    .is_none_or(|rows| clauses(rows)),
                MutationClause::Purge(value) => value
                    .where_clauses
                    .as_ref()
                    .is_none_or(|rows| clauses(rows)),
                MutationClause::PurgePayload(value) => value
                    .where_clauses
                    .as_ref()
                    .is_none_or(|rows| clauses(rows)),
                MutationClause::MergeConcept(value) => value
                    .where_clauses
                    .as_ref()
                    .is_none_or(|rows| clauses(rows)),
                _ => true,
            }),
            Command::Meta(
                MetaCommand::History(_)
                | MetaCommand::Changes(_)
                | MetaCommand::ExportCapsule(_)
                | MetaCommand::Describe(
                    DescribeTarget::Transaction(_)
                    | DescribeTarget::TransactionByIdempotencyKey(_)
                    | DescribeTarget::Capsule(_),
                ),
            ) => false,
            _ => true,
        };
        if !current {
            return Err(
                "historical or inactive memory access is disabled after a managed change".into(),
            );
        }
    }
    Ok(())
}

/// Let Nexus decode its own opaque cursor and bind it to the actual query.
/// Pages started after the last change remain usable; older traversals restart.
fn current_cursor(
    query: &anda_kip::KqlQuery,
    request: &anda_kip::Request,
    operation: &anda_kip::Operation,
    read_floor: u64,
) -> Result<bool, BoxError> {
    use anda_cognitive_nexus::{
        nexus::DEFAULT_SPACE,
        store::history::{CursorFamily, PageCursor, traversal_of},
    };
    use anda_kip::{KipValue, Scalar};
    let token = match &query.cursor {
        None => return Ok(true),
        Some(Scalar::Literal(KipValue::String(token))) => Some(token.as_str()),
        Some(Scalar::Param(name)) => operation
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.get(name))
            .or_else(|| {
                request
                    .parameters
                    .as_ref()
                    .and_then(|parameters| parameters.get(name))
            })
            .and_then(serde_json::Value::as_str),
        _ => None,
    }
    .ok_or("invalid memory pagination cursor")?;
    let cursor = PageCursor::from_token(
        token,
        DEFAULT_SPACE,
        CursorFamily::Query,
        &traversal_of(
            query,
            request.parameters.as_ref(),
            operation.parameters.as_ref(),
        ),
    )?;
    Ok(cursor.snapshot_seq >= read_floor)
}

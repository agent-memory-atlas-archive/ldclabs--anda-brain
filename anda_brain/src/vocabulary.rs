//! The vocabulary this brain's memories are written against.
//!
//! A memory system meets vocabulary it was not designed with. A conversation
//! introduces `ships_to`, a wiki page introduces `Drug` and `treats`. KIP 1.x
//! let a write register those on the spot, as `$ConceptType` /
//! `$PropositionType` nodes in the same graph as the facts — which meant an
//! ordinary write could change what a type meant, and a model under prompt
//! injection could change it on purpose.
//!
//! KIP 2.0 will not do that. Authoritative Schema is an immutable, versioned
//! Package resolved through the Space's Schema Environment. What a Space may
//! add is its **draft vocabulary** (Spec §20.16): `DEFINE PREDICATE` /
//! `DEFINE CONCEPT TYPE` add one symbol to the Space-local
//! `kip://local/draft@0.0.0`, only ever adding, never changing or shadowing a
//! symbol anything else defines, under `propose_schema` — which grants nothing
//! over existing Schema. A draft symbol stays a draft until an owner holding
//! `manage_schema` promotes it onto an installed package's symbol.
//!
//! The brain adds drafts from two places: Formation's own `DEFINE`, bounded by
//! the cognition gate, and [`draft_symbols`] for names the host proposes (the
//! `declare_memory_symbols` tool and the wiki digest). Either way the host
//! caps the Space's vocabulary and queues one `review_schema` SleepTask per new
//! symbol, keyed `review_schema:<kind>:<ref>`, so Maintenance reviews what was
//! drafted and proposes promotions it cannot perform itself.
//!
//! Spaces that grew vocabulary before the draft package existed keep their
//! host package `kip://anda-brain/memory@1.0.N`: it stays in force and
//! readable, and nothing is added to it any more.
//!
//! The Space's Schema Environment is the *only* store: the vocabulary is read
//! back out of `LIST TYPES` / `LIST PREDICATES` rather than kept in a second
//! place that could disagree with it.

use anda_cognitive_nexus::CognitiveNexus;
use anda_core::{BoxError, FunctionDefinition, Resource, Tool, ToolOutput};
use anda_engine::{context::BaseCtx, memory::MemoryManagement};
use anda_kip::DefineKind;
use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, LazyLock},
};

use crate::kip;

/// The package id this brain published its vocabulary under before the
/// draft vocabulary existed. Read-only now: it stays in force for the Spaces
/// that have it, and nothing new is added to it.
pub const MEMORY_PACKAGE_ID: &str = "kip://anda-brain/memory";

/// Cap on how many symbols one Space's own vocabulary may hold: its drafts and
/// its legacy host package together.
///
/// Draft symbols are never removed, so an unbounded vocabulary would grow the
/// Schema Environment without limit. Past the cap the brain refuses the new
/// symbol rather than the whole write: a Space that has already introduced this
/// many distinct predicates is accumulating synonyms, and the answer is to
/// reuse what it has. `@ldclabs/anda-brain-worker` enforces the same number.
pub const MAX_SYMBOLS: usize = 512;

/// The most `DEFINE`s one model request may carry.
pub const MAX_DEFINES_PER_REQUEST: usize = 8;

/// Longest symbol name the brain will define.
pub const MAX_SYMBOL_CHARS: usize = 64;

/// The vocabulary of one Space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryVocabulary {
    /// Concept types in this Space's legacy host package.
    pub types: BTreeSet<String>,

    /// Predicates in this Space's legacy host package.
    pub predicates: BTreeSet<String>,

    /// Concept types this Space drafted (`kip://local/draft@0.0.0`).
    pub draft_types: BTreeSet<String>,

    /// Predicates this Space drafted.
    pub draft_predicates: BTreeSet<String>,

    /// The legacy host package's patch version in force, `0` when the Space
    /// has none.
    pub revision: u32,

    /// Symbols other active packages declare, chiefly the Cognitive Memory
    /// Profile's. Writable, but not this Space's to redefine.
    borrowed_types: BTreeSet<String>,
    borrowed_predicates: BTreeSet<String>,
}

impl MemoryVocabulary {
    /// Reads the vocabulary out of the Space's Schema Environment.
    ///
    /// The environment is the store. Keeping a second copy beside it would
    /// create exactly one bug — the two disagreeing — and no capability.
    pub async fn load(nexus: &CognitiveNexus) -> Result<Self, BoxError> {
        let mut vocabulary = Self::default();
        let mut version = String::new();
        for (command, mine, drafted, borrowed) in [
            (
                "LIST TYPES LIMIT 1000",
                &mut vocabulary.types,
                &mut vocabulary.draft_types,
                &mut vocabulary.borrowed_types,
            ),
            (
                "LIST PREDICATES LIMIT 1000",
                &mut vocabulary.predicates,
                &mut vocabulary.draft_predicates,
                &mut vocabulary.borrowed_predicates,
            ),
        ] {
            let response = anda_kip::execute_request(nexus, &kip::request(command)).await;
            if !kip::succeeded(&response) {
                return Err(format!(
                    "reading the Space's vocabulary failed: {}",
                    kip::error_message(&response)
                )
                .into());
            }
            let Some(entries) = kip::ok_result(&response).and_then(Json::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(name) = entry.get("local_name").and_then(Json::as_str) else {
                    continue;
                };
                let reference = entry
                    .get("package_ref")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                if reference == anda_kip::DRAFT_PACKAGE_REF {
                    drafted.insert(name.to_string());
                } else if let Some(declared) =
                    reference.strip_prefix(&format!("{MEMORY_PACKAGE_ID}@"))
                {
                    version = declared.to_string();
                    mine.insert(name.to_string());
                } else {
                    borrowed.insert(name.to_string());
                }
            }
        }
        vocabulary.revision = patch_of(&version).unwrap_or(0);
        Ok(vocabulary)
    }

    /// The legacy host package's exact version.
    pub fn version(&self) -> String {
        format!("1.0.{}", self.revision)
    }

    /// The legacy host package's reference — `kip://anda-brain/memory@1.0.7`.
    pub fn package_ref(&self) -> String {
        format!("{MEMORY_PACKAGE_ID}@{}", self.version())
    }

    /// Whether the Space can already resolve every one of these symbols,
    /// whoever declares them.
    #[cfg(any(test, feature = "wiki"))]
    pub fn covers<'a>(
        &self,
        types: impl IntoIterator<Item = &'a str>,
        predicates: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        types.into_iter().all(|name| self.has_type(name))
            && predicates.into_iter().all(|name| self.has_predicate(name))
    }

    fn has_type(&self, name: &str) -> bool {
        self.types.contains(name)
            || self.draft_types.contains(name)
            || self.borrowed_types.contains(name)
    }

    fn has_predicate(&self, name: &str) -> bool {
        self.predicates.contains(name)
            || self.draft_predicates.contains(name)
            || self.borrowed_predicates.contains(name)
    }

    /// How many symbols this Space's own vocabulary holds: drafts and the
    /// legacy host package together, which is what [`MAX_SYMBOLS`] caps.
    pub fn len(&self) -> usize {
        self.package_len() + self.draft_types.len() + self.draft_predicates.len()
    }

    /// How many symbols the legacy host package declares.
    pub fn package_len(&self) -> usize {
        self.types.len() + self.predicates.len()
    }

    /// Puts the Cognitive Memory Profile in force, with the legacy host
    /// package when this Space has one. The draft package is Space state the
    /// engine keeps across every activation.
    ///
    /// `install_and_activate` re-activates only when the resulting lock differs
    /// from the one already in force, so calling this on every open does not
    /// walk the environment version forward.
    pub async fn activate(&self, nexus: &CognitiveNexus) -> Result<(), BoxError> {
        let mut artifacts: Vec<(&str, String)> = vec![(
            "anda_brain",
            anda_cognitive_nexus::profiles::COGNITIVE_MEMORY.to_string(),
        )];
        if self.package_len() > 0 {
            artifacts.push(("anda_brain_memory", self.artifact()));
        }
        let artifacts: Vec<(&str, &str)> = artifacts
            .iter()
            .map(|(source, artifact)| (*source, artifact.as_str()))
            .collect();
        nexus
            .install_and_activate(&artifacts, anda_cognitive_nexus::nexus::DEFAULT_SPACE)
            .await?;
        Ok(())
    }

    /// Renders the legacy host package exactly as it was published, so
    /// re-activating it installs nothing new.
    ///
    /// Every predicate accepts any Concept on both ends and is declared
    /// `open_world` and non-`functional`: these symbols came from prose, so
    /// nothing had a basis for claiming more.
    pub fn artifact(&self) -> String {
        let package_ref = self.package_ref();
        let mut concept_types = Map::new();
        for name in &self.types {
            concept_types.insert(
                name.clone(),
                json!({
                    "ref": format!("{package_ref}/{name}"),
                    "kind": "ConceptType",
                    "description": format!(
                        "Entity type `{name}`, met while forming this Space's memory. It means \
                         whatever the sources it came from meant by it; nothing here verifies that."
                    ),
                    "attributes": {"open": true, "fields": {}},
                }),
            );
        }
        let mut predicates = Map::new();
        for name in &self.predicates {
            predicates.insert(
                name.clone(),
                json!({
                    "ref": format!("{package_ref}/{name}"),
                    "kind": "PredicateType",
                    "description": format!(
                        "Relation `{name}`, met while forming this Space's memory. A claim under \
                         it is somebody's statement, never a verified fact."
                    ),
                    "subject": {"kinds": ["Concept"]},
                    "object": {"kinds": ["Concept"]},
                    "functional": false,
                    "open_world": true,
                    "complete": false,
                }),
            );
        }

        let package = json!({
            "format": "KIP-Schema-Package",
            "format_version": "2.0-draft",
            "manifest": {
                "package_id": MEMORY_PACKAGE_ID,
                "version": self.version(),
                "package_ref": package_ref,
                "name": "Anda Brain space vocabulary",
                "description": "Concept types and predicates this Space's memory needed beyond the \
                                Cognitive Memory Profile. Deployment-local: these symbols mean what \
                                their sources meant and are not portable ontology.",
                "publisher": "urn:kip:publisher:anda-brain",
                "purpose": "deployment_extension",
                "stability": "experimental",
                "executable": false,
            },
            "dependencies": [{
                "package_id": "kip://core",
                "version": "2.0.0",
                "package_ref": "kip://core@2.0.0",
                "required": true,
            }],
            "definitions": {
                "concept_types": concept_types,
                "predicates": predicates,
            },
            "model_hints": {
                "provenance_invariant": "A symbol here was proposed by a model reading a \
                                         conversation or a document. Its presence is not evidence \
                                         that the relation it names is real.",
            },
        });
        serde_json::to_string(&package).expect("the vocabulary package serializes")
    }
}

/// A Concept type name: UpperCamelCase, alphanumeric.
///
/// Enforced here rather than left to the model, because `drug`, `Drug` and
/// `medical device` would otherwise become three types meaning one thing —
/// and, unlike a 1.x graph node, a published symbol cannot be tidied away.
pub fn is_type_name(name: &str) -> bool {
    name.len() <= MAX_SYMBOL_CHARS
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric())
        // §20.13: a package MUST NOT define a symbol that shadows a reserved
        // Core symbol name. Core exports the five element kinds and no Concept
        // types at all, so `LIST TYPES` never reports them and the borrowed set
        // cannot catch this one — a model proposing `Assertion` would pass every
        // other check here and be refused at *package installation*, which
        // takes the whole publish down and leaves the Space unable to grow its
        // vocabulary again. Refused here instead, where the answer is one
        // rejected name.
        && !anda_kip::CORE_ELEMENT_KINDS.contains(&name)
}

/// A predicate name: snake_case, starting with a lowercase letter.
pub fn is_predicate_name(name: &str) -> bool {
    name.len() <= MAX_SYMBOL_CHARS
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.ends_with('_')
}

/// The label a symbol kind carries in a `review_schema` key and in the
/// promotion API: `ConceptType` or `PredicateType` (Spec §20.16).
pub fn kind_label(kind: DefineKind) -> &'static str {
    match kind {
        DefineKind::ConceptType => "ConceptType",
        DefineKind::Predicate => "PredicateType",
    }
}

/// Whether a name has the shape this brain gives a symbol of this kind.
pub fn is_symbol_name(kind: DefineKind, name: &str) -> bool {
    match kind {
        DefineKind::ConceptType => is_type_name(name),
        DefineKind::Predicate => is_predicate_name(name),
    }
}

/// The description the host gives a symbol it drafts from a bare name.
fn host_description(kind: DefineKind, name: &str) -> String {
    match kind {
        DefineKind::ConceptType => format!(
            "Entity type `{name}`, met while forming this Space's memory. It means whatever the \
             sources it came from meant by it; nothing here verifies that."
        ),
        DefineKind::Predicate => format!(
            "Relation `{name}`, met while forming this Space's memory. A claim under it is \
             somebody's statement, never a verified fact."
        ),
    }
}

/// What [`draft_symbols`] made of the names it was given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drafted {
    /// Exact references of the symbols this call defined.
    pub defined: Vec<String>,
    /// Names refused: malformed, or past [`MAX_SYMBOLS`].
    pub rejected: Vec<String>,
}

/// Drafts the host-proposed names the Space cannot yet resolve (Spec §20.16).
///
/// A name the Space already speaks, whoever defines it, is left alone; a
/// malformed name or one past [`MAX_SYMBOLS`] is refused on its own rather than
/// failing the rest. Each new symbol is one `DEFINE` with a host description —
/// unconstrained endpoints, open world, open attributes — because these names
/// come from prose and nothing here has a basis for claiming more. Every
/// definition queues its `review_schema` SleepTask.
pub async fn draft_symbols(
    nexus: &CognitiveNexus,
    types: &[&str],
    predicates: &[&str],
) -> Result<Drafted, BoxError> {
    let vocabulary = MemoryVocabulary::load(nexus).await?;
    let mut drafted = Drafted::default();
    let mut size = vocabulary.len();
    let mut seen = BTreeSet::new();
    for (kind, names) in [
        (DefineKind::ConceptType, types),
        (DefineKind::Predicate, predicates),
    ] {
        for &name in names {
            if !seen.insert((kind_label(kind), name)) {
                continue;
            }
            let known = match kind {
                DefineKind::ConceptType => vocabulary.has_type(name),
                DefineKind::Predicate => vocabulary.has_predicate(name),
            };
            if known {
                continue;
            }
            if !is_symbol_name(kind, name) || size >= MAX_SYMBOLS {
                drafted.rejected.push(name.to_string());
                continue;
            }
            let description = host_description(kind, name);
            let command = match kind {
                DefineKind::ConceptType => "DEFINE CONCEPT TYPE :name {description: :description}",
                DefineKind::Predicate => "DEFINE PREDICATE :name {description: :description}",
            };
            let response = anda_kip::execute_request(
                nexus,
                &kip::request_with(
                    command,
                    Map::from_iter([
                        ("name".to_string(), Json::from(name)),
                        ("description".to_string(), Json::from(description.as_str())),
                    ]),
                ),
            )
            .await;
            if kip::error_of(&response).is_some_and(|error| error.code == "SchemaSymbolConflict") {
                // Defined meanwhile: the name resolves, which is all a caller
                // asked for.
                continue;
            }
            let Some(reference) = kip::ok_result(&response)
                .and_then(|result| result.get("ref"))
                .and_then(Json::as_str)
                .map(str::to_string)
            else {
                return Err(format!(
                    "defining {} `{name}` failed: {}",
                    kind_label(kind),
                    kip::error_message(&response)
                )
                .into());
            };
            size += 1;
            queue_schema_review(nexus, kind, &reference, &description).await?;
            drafted.defined.push(reference);
        }
    }
    Ok(drafted)
}

/// Queues the review of one draft symbol: a `review_schema` SleepTask keyed
/// `review_schema:<kind>:<exact ref>` (Spec §20.16, Profile §5.9), so a retry
/// or a second definition attempt resolves to the same task. Maintenance
/// reviews it and may propose a promotion; only an owner performs one.
pub(crate) async fn queue_schema_review(
    executor: &impl anda_kip::Executor,
    kind: DefineKind,
    reference: &str,
    description: &str,
) -> Result<(), BoxError> {
    let label = kind_label(kind);
    let name = reference.rsplit('/').next().unwrap_or(reference);
    let summary: String = format!("Review the draft {label} `{name}`: {description}")
        .chars()
        .take(1024)
        .collect();
    let response = anda_kip::execute_request(
        executor,
        &kip::request_with(
            r#"CREATE CONCEPT ?task {
  TYPE "SleepTask"
  CLIENT KEY :key
  NAME :name
  SET ATTRIBUTES { task_class: "review_schema", summary: :summary, status: "pending", created_at: :now, symbol_kind: :kind, symbol_ref: :ref }
}"#,
            Map::from_iter([
                (
                    "key".to_string(),
                    Json::from(format!("review_schema:{label}:{reference}")),
                ),
                (
                    "name".to_string(),
                    Json::from(format!("Review draft {label} {name}")),
                ),
                ("summary".to_string(), Json::from(summary)),
                (
                    "now".to_string(),
                    Json::from(kip::timestamp(anda_engine::unix_ms())),
                ),
                ("kind".to_string(), Json::from(label)),
                ("ref".to_string(), Json::from(reference)),
            ]),
        ),
    )
    .await;
    if kip::succeeded(&response) {
        Ok(())
    } else {
        Err(format!(
            "queueing the review of {reference} failed: {}",
            kip::error_message(&response)
        )
        .into())
    }
}

/// One `DEFINE` in a model's request, as the host queues its review.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelDefine {
    /// The operation's position in the request.
    pub index: usize,
    pub kind: DefineKind,
    pub description: String,
}

/// The `DEFINE`s a model request carries, in operation order. The Formation
/// gate has already required each to be a literal standalone operation.
pub(crate) fn model_defines(request: &anda_kip::Request) -> Vec<ModelDefine> {
    request
        .operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| {
            let anda_kip::Command::Kml(statement) = operation.parse().ok()? else {
                return None;
            };
            let [anda_kip::MutationClause::Define(define)] = statement.clauses.as_slice() else {
                return None;
            };
            let description = match define.definition.get("description") {
                Some(anda_kip::BoundValue::Value(anda_kip::KipValue::String(text))) => text.clone(),
                _ => String::new(),
            };
            Some(ModelDefine {
                index,
                kind: define.kind,
                description,
            })
        })
        .collect()
}

/// Whether the Space's vocabulary has room for `count` more symbols.
pub(crate) async fn check_define_budget(
    nexus: &CognitiveNexus,
    count: usize,
) -> Result<(), String> {
    let vocabulary = MemoryVocabulary::load(nexus)
        .await
        .map_err(|error| error.to_string())?;
    if vocabulary.len() + count > MAX_SYMBOLS {
        return Err(format!(
            "this Space's vocabulary holds {} of its {MAX_SYMBOLS} symbols; reuse an existing \
             symbol instead of defining another",
            vocabulary.len()
        ));
    }
    Ok(())
}

/// Queues a `review_schema` task for every model `DEFINE` that committed.
pub(crate) async fn review_model_defines(
    nexus: &CognitiveNexus,
    defines: &[ModelDefine],
    response: &anda_kip::Response,
) {
    for define in defines {
        let Some(reference) = response
            .results
            .get(define.index)
            .filter(|result| result.error.is_none())
            .and_then(|result| result.result.as_ref())
            .and_then(|result| result.get("ref"))
            .and_then(Json::as_str)
        else {
            continue;
        };
        if let Err(error) =
            queue_schema_review(nexus, define.kind, reference, &define.description).await
        {
            log::warn!(target: "brain", "{error}");
        }
    }
}

/// The model-facing arguments of [`DeclareSymbolsTool`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeclareSymbolsArgs {
    /// UpperCamelCase Concept type names, e.g. `Project`.
    #[serde(default)]
    pub types: Vec<String>,
    /// snake_case predicate names, e.g. `works_on`.
    #[serde(default)]
    pub predicates: Vec<String>,
}

static DECLARE_SYMBOLS_DEFINITION: LazyLock<FunctionDefinition> = LazyLock::new(|| {
    serde_json::from_value(json!({
        "name": DeclareSymbolsTool::NAME,
        "description": "Deprecated shortcut for DEFINE: drafts bare Concept type and predicate \
                        names with a generic host description, for when you cannot write one. \
                        Prefer `DEFINE CONCEPT TYPE` / `DEFINE PREDICATE` through execute_kip \
                        with a real description. KIP 2.0 resolves every symbol through the \
                        Space's Schema Environment, so a command naming an undefined type or \
                        predicate is refused with SchemaSymbolNotFound. Reuse an existing symbol \
                        wherever one fits: `LIST TYPES` and `LIST PREDICATES` show what this \
                        Space already speaks, and minting a synonym splits one memory in two. \
                        Defining a symbol says nothing about whether any claim using it is true.",
        "parameters": {
            "type": "object",
            "properties": {
                "types": {
                    "type": "array",
                    "description": "Concept type names in UpperCamelCase, letters and digits only \
                                    (e.g. Project, MedicalDevice).",
                    "items": {"type": "string"}
                },
                "predicates": {
                    "type": "array",
                    "description": "Predicate names in snake_case, lowercase letters, digits and \
                                    underscores (e.g. works_on, ships_to).",
                    "items": {"type": "string"}
                }
            },
            "required": ["types", "predicates"],
            "additionalProperties": false
        },
        "strict": true
    }))
    .unwrap()
});

/// The patch component of a `1.0.N` version string.
fn patch_of(version: &str) -> Option<u32> {
    version
        .rsplit('.')
        .next()
        .and_then(|patch| patch.parse().ok())
}

/// Lets Formation draft names it cannot describe better.
///
/// A deprecated shortcut: Formation's own `DEFINE` carries a real description
/// and is the path the contract asks for. This one keeps its signature for one
/// release so a model that still calls it gets a draft rather than an error.
/// Maintenance reviews drafts and never defines one, and Recall is read-only,
/// so neither may call it.
#[derive(Clone)]
pub struct DeclareSymbolsTool {
    memory: Arc<MemoryManagement>,
    product_control: Option<Arc<crate::product::control::Control>>,
    /// Serializes drafting, so two concurrent calls cannot both pass the cap.
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl DeclareSymbolsTool {
    /// Function name used when registering the tool.
    pub const NAME: &'static str = "declare_memory_symbols";

    /// Creates the tool over one space's memory.
    pub fn new(memory: Arc<MemoryManagement>) -> Self {
        Self {
            memory,
            product_control: None,
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) fn with_product_control(
        mut self,
        control: Arc<crate::product::control::Control>,
    ) -> Self {
        self.product_control = Some(control);
        self
    }

    /// Drafts the symbols and returns the names it refused.
    pub async fn declare(
        &self,
        args: &DeclareSymbolsArgs,
    ) -> Result<(Json, Vec<String>), BoxError> {
        let _guard = self.lock.lock().await;
        let nexus = self.memory.nexus();
        let types: Vec<&str> = args.types.iter().map(String::as_str).collect();
        let predicates: Vec<&str> = args.predicates.iter().map(String::as_str).collect();
        let drafted = draft_symbols(nexus.as_ref(), &types, &predicates).await?;
        Ok((
            json!({
                "draft_package": anda_kip::DRAFT_PACKAGE_REF,
                "defined": drafted.defined,
            }),
            drafted.rejected,
        ))
    }
}

impl std::fmt::Debug for DeclareSymbolsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclareSymbolsTool").finish_non_exhaustive()
    }
}

impl Tool<BaseCtx> for DeclareSymbolsTool {
    type Args = DeclareSymbolsArgs;
    type Output = Json;

    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn description(&self) -> String {
        DECLARE_SYMBOLS_DEFINITION.description.clone()
    }

    fn definition(&self) -> FunctionDefinition {
        DECLARE_SYMBOLS_DEFINITION.clone()
    }

    async fn call(
        &self,
        ctx: BaseCtx,
        args: Self::Args,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Self::Output>, BoxError> {
        if ctx.agent != crate::agents::FormationAgent::NAME {
            return Err(
                "only Formation drafts vocabulary; Maintenance reviews drafts and \
                        records a near-synonym as an Insight about the existing symbol"
                    .into(),
            );
        }
        let _guard = if let Some(control) = &self.product_control {
            let guard = control.gate.lock().await;
            control.check(&ctx)?;
            Some(guard)
        } else {
            None
        };
        let (vocabulary, rejected) = self.declare(&args).await?;
        // A refusal is reported, not raised: the caller can still write every
        // memory whose symbols were accepted, and telling it which names to
        // stop trying is more useful than failing the whole call.
        let mut output = ToolOutput::new(json!({
            "vocabulary": vocabulary,
            "rejected": rejected,
            "hint": if rejected.is_empty() {
                Json::Null
            } else {
                json!("a rejected name is malformed (types are UpperCamelCase, predicates are \
                       snake_case) or the Space has reached its symbol cap; reuse an existing \
                       symbol instead of renaming around the refusal")
            },
        }));
        output.is_error = (!rejected.is_empty()).then_some(true);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_without_a_host_package_names_the_first_revision() {
        let vocabulary = MemoryVocabulary::default();
        assert_eq!(vocabulary.package_ref(), "kip://anda-brain/memory@1.0.0");
        assert!(vocabulary.covers([], []));
        assert!(!vocabulary.covers(["Drug"], []));
    }

    #[test]
    fn drafts_legacy_symbols_and_borrowed_ones_all_resolve() {
        let vocabulary = MemoryVocabulary {
            types: BTreeSet::from(["Project".to_string()]),
            draft_types: BTreeSet::from(["Instrument".to_string()]),
            draft_predicates: BTreeSet::from(["mentors".to_string()]),
            borrowed_types: BTreeSet::from(["Person".to_string()]),
            borrowed_predicates: BTreeSet::from(["prefers".to_string()]),
            revision: 3,
            ..Default::default()
        };
        assert!(vocabulary.covers(["Project", "Instrument", "Person"], ["mentors", "prefers"]));
        assert!(!vocabulary.covers(["Drug"], []));
        // The cap counts this Space's own symbols, drafts included, and never
        // another package's.
        assert_eq!(vocabulary.len(), 3);
        assert_eq!(vocabulary.package_len(), 1);
        assert_eq!(vocabulary.package_ref(), "kip://anda-brain/memory@1.0.3");
    }

    #[test]
    fn symbol_names_follow_their_kind() {
        assert!(is_symbol_name(DefineKind::ConceptType, "MedicalDevice"));
        assert!(is_symbol_name(DefineKind::Predicate, "works_on"));
        for (kind, name) in [
            (DefineKind::ConceptType, "drug"),
            (DefineKind::ConceptType, "medical device"),
            // §20.13: a reserved Core name is refused for every kind.
            (DefineKind::ConceptType, "Assertion"),
            (DefineKind::Predicate, "Treats"),
            (DefineKind::Predicate, "ends_"),
        ] {
            assert!(!is_symbol_name(kind, name), "{name}");
        }
        assert!(!is_type_name(&"A".repeat(MAX_SYMBOL_CHARS + 1)));
    }

    #[test]
    fn the_legacy_artifact_declares_exactly_the_symbols_it_holds() {
        let vocabulary = MemoryVocabulary {
            types: BTreeSet::from(["Drug".to_string(), "Symptom".to_string()]),
            predicates: BTreeSet::from(["treats".to_string()]),
            // Drafts are the engine's, never part of this artifact.
            draft_types: BTreeSet::from(["Instrument".to_string()]),
            revision: 1,
            ..Default::default()
        };
        let package: Json = serde_json::from_str(&vocabulary.artifact()).unwrap();
        assert_eq!(
            package["manifest"]["package_ref"],
            "kip://anda-brain/memory@1.0.1"
        );
        let types = package["definitions"]["concept_types"].as_object().unwrap();
        assert_eq!(types.len(), 2);
        assert_eq!(
            types["Drug"]["ref"],
            "kip://anda-brain/memory@1.0.1/Drug".to_string()
        );
        let predicates = package["definitions"]["predicates"].as_object().unwrap();
        assert_eq!(predicates["treats"]["functional"], false);
        assert_eq!(predicates["treats"]["open_world"], true);
    }

    #[test]
    fn a_model_define_is_read_with_its_position_and_description() {
        let mut request = kip::request("DESCRIBE PRIMER");
        request.operations.extend(
            kip::request(
                r#"DEFINE PREDICATE "mentors" {description: "The subject mentors the object."}"#,
            )
            .operations,
        );
        request.operations.extend(
            kip::request(r#"DEFINE CONCEPT TYPE "Instrument" {description: "An instrument."}"#)
                .operations,
        );
        assert_eq!(
            model_defines(&request),
            vec![
                ModelDefine {
                    index: 1,
                    kind: DefineKind::Predicate,
                    description: "The subject mentors the object.".into(),
                },
                ModelDefine {
                    index: 2,
                    kind: DefineKind::ConceptType,
                    description: "An instrument.".into(),
                },
            ]
        );
    }
}

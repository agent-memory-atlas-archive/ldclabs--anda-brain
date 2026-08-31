//! The Schema Package this brain's own memories are written against.
//!
//! A memory system meets vocabulary it was not designed with. A conversation
//! introduces `ships_to`, a wiki page introduces `Drug` and `treats`. KIP 1.x
//! let a write register those on the spot, as `$ConceptType` /
//! `$PropositionType` nodes in the same graph as the facts — which meant an
//! ordinary write could change what a type meant, and a model under prompt
//! injection could change it on purpose.
//!
//! KIP 2.0 will not do that. Authoritative Schema is an immutable, versioned
//! Package resolved through the Space's Schema Environment, and KML cannot
//! touch it: "a language a model writes must not be a language that can change
//! who controls the Space". So new vocabulary enters through the **host**. The
//! brain keeps one package per Space, publishes a new version when a symbol is
//! genuinely new, and activates it alongside the Cognitive Memory Profile.
//!
//! The model still proposes the words — through a tool call, not through KML —
//! and the host decides, caps and versions the result. What that buys is
//! concrete: every element's `schema_ref` names an exact version forever, so a
//! memory's meaning cannot drift underneath it, and the set of things this
//! Brain can say is a reviewable artifact rather than an emergent property of
//! whatever its models happened to write.
//!
//! The Space's Schema Environment is also the *only* store: the vocabulary is
//! read back out of `LIST TYPES` / `LIST PREDICATES` rather than kept in a
//! second place that could disagree with it.

use anda_cognitive_nexus::CognitiveNexus;
use anda_core::{BoxError, FunctionDefinition, Resource, Tool, ToolOutput};
use anda_engine::{context::BaseCtx, memory::MemoryManagement};
use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, LazyLock},
};

use crate::kip;

/// The package id the brain publishes its own vocabulary under.
///
/// Deliberately outside `kip://core` and `kip://profiles`: those namespaces
/// carry meanings other engines are expected to share, and a predicate one
/// conversation happened to use is not one of them.
pub const MEMORY_PACKAGE_ID: &str = "kip://anda-brain/memory";

/// Cap on how many symbols one Space's vocabulary may hold.
///
/// The package is re-published on every extension, so an unbounded vocabulary
/// would grow both the artifact and the Schema Environment history without
/// limit. Past the cap the brain refuses the new symbol rather than the whole
/// write: a Space that has already introduced this many distinct predicates is
/// accumulating synonyms, and the answer is to reuse what it has.
pub const MAX_SYMBOLS: usize = 512;

/// Longest symbol name the brain will publish.
pub const MAX_SYMBOL_CHARS: usize = 64;

/// The vocabulary of one Space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryVocabulary {
    /// Concept types this brain published, as UpperCamelCase local names.
    pub types: BTreeSet<String>,

    /// Predicates this brain published, as snake_case local names.
    pub predicates: BTreeSet<String>,

    /// How many times the package has been published — the version's patch
    /// component. A counter rather than a content hash: a version must never go
    /// backwards, and two vocabularies differing only in arrival order are
    /// still two published artifacts in the environment's history.
    pub revision: u32,

    /// The highest revision this Space has ever *installed*, in force or not.
    ///
    /// `revision` is the one in force, which is what `package_ref()` has to
    /// name. They differ after a partial publish: `install_and_activate`
    /// installs the artifact and then activates it, and a failure between those
    /// two leaves a version installed that never came into force. Re-minting
    /// that number with a different symbol set is refused as a `DigestMismatch`
    /// — a package version identifies one canonical content forever — and the
    /// Space could then never grow its vocabulary again. Skipping the stranded
    /// number costs one integer.
    floor: u32,

    /// Symbols other active packages declare, chiefly the Cognitive Memory
    /// Profile's. Writable, but not this package's to redeclare.
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
        for (command, mine, borrowed) in [
            (
                "LIST TYPES LIMIT 1000",
                &mut vocabulary.types,
                &mut vocabulary.borrowed_types,
            ),
            (
                "LIST PREDICATES LIMIT 1000",
                &mut vocabulary.predicates,
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
                if let Some(declared) = reference.strip_prefix(&format!("{MEMORY_PACKAGE_ID}@")) {
                    version = declared.to_string();
                    mine.insert(name.to_string());
                } else {
                    borrowed.insert(name.to_string());
                }
            }
        }
        vocabulary.revision = patch_of(&version).unwrap_or(0);
        vocabulary.floor = highest_installed_revision(nexus).await?;
        Ok(vocabulary)
    }

    /// The exact version this vocabulary publishes as.
    pub fn version(&self) -> String {
        format!("1.0.{}", self.revision)
    }

    /// The package reference — `kip://anda-brain/memory@1.0.7`.
    pub fn package_ref(&self) -> String {
        format!("{MEMORY_PACKAGE_ID}@{}", self.version())
    }

    /// Whether the Space can already resolve every one of these symbols,
    /// whoever declares them.
    pub fn covers<'a>(
        &self,
        types: impl IntoIterator<Item = &'a str>,
        predicates: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        types
            .into_iter()
            .all(|name| self.types.contains(name) || self.borrowed_types.contains(name))
            && predicates.into_iter().all(|name| {
                self.predicates.contains(name) || self.borrowed_predicates.contains(name)
            })
    }

    /// Adds symbols, bumping the revision when anything was genuinely new.
    ///
    /// Returns the names that were rejected: malformed, or past [`MAX_SYMBOLS`].
    /// A symbol another active package already declares is *not* rejected and
    /// not added — redeclaring it would make every reference to it ambiguous,
    /// and the Profile's meaning is the better one anyway.
    pub fn extend<'a>(
        &mut self,
        types: impl IntoIterator<Item = &'a str>,
        predicates: impl IntoIterator<Item = &'a str>,
    ) -> Vec<String> {
        let mut rejected = Vec::new();
        let mut changed = false;
        let mut published = self.len();
        for (names, valid, mine, borrowed) in [
            (
                types.into_iter().collect::<Vec<_>>(),
                is_type_name as fn(&str) -> bool,
                &mut self.types,
                &self.borrowed_types,
            ),
            (
                predicates.into_iter().collect::<Vec<_>>(),
                is_predicate_name as fn(&str) -> bool,
                &mut self.predicates,
                &self.borrowed_predicates,
            ),
        ] {
            for name in names {
                if borrowed.contains(name) || mine.contains(name) {
                    continue;
                }
                if !valid(name) {
                    rejected.push(name.to_string());
                    continue;
                }
                // The cap is on what *this package* declares — the artifact
                // it re-publishes on every extension — so it counts types and
                // predicates together and counts nothing another package
                // declares. Reading `self.borrowed_types` here charged the
                // Profile's types against the predicate budget and let each
                // kind fill MAX_SYMBOLS on its own, so the real ceiling was
                // twice the documented one. `@ldclabs/kip-do` checks
                // `this.size`, and the two engines have to agree on a limit
                // both READMEs quote.
                if published >= MAX_SYMBOLS {
                    rejected.push(name.to_string());
                    continue;
                }
                mine.insert(name.to_string());
                published += 1;
                changed = true;
            }
        }
        if changed {
            // Past the highest number ever installed, not merely past the one
            // in force: see `floor`.
            self.revision = self.revision.max(self.floor).saturating_add(1);
            self.floor = self.revision;
        }
        rejected
    }

    /// How many symbols this package declares.
    pub fn len(&self) -> usize {
        self.types.len() + self.predicates.len()
    }

    /// Publishes this vocabulary and puts it in force alongside the Cognitive
    /// Memory Profile.
    ///
    /// `install_and_activate` re-activates only when the resulting lock differs
    /// from the one already in force, so calling this with an unchanged
    /// vocabulary does not walk the environment version forward.
    pub async fn activate(&self, nexus: &CognitiveNexus) -> Result<(), BoxError> {
        let mut artifacts: Vec<(&str, String)> = vec![(
            "anda_brain",
            anda_cognitive_nexus::profiles::COGNITIVE_MEMORY.to_string(),
        )];
        if self.len() > 0 {
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

    /// Renders the Schema Package artifact.
    ///
    /// Every predicate accepts any Concept on both ends and is declared
    /// `open_world` and non-`functional`. That is not laziness: these symbols
    /// come from prose, so nothing here has a basis for claiming a relation
    /// holds between exactly two types, or that what was written down was
    /// everything — and a schema asserting either would turn a gap in what the
    /// brain was told into a closed world.
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
        "description": "Declares Concept types and predicates this memory needs before you can \
                        write with them. KIP 2.0 resolves every symbol through the Space's Schema \
                        Environment, so a KML command naming an undeclared type or predicate is \
                        refused with SchemaSymbolNotFound — call this first, then write. Reuse an \
                        existing symbol wherever one fits: `LIST TYPES` and `LIST PREDICATES` show \
                        what this Space already speaks, and minting a synonym splits one memory in \
                        two. Declaring a symbol says nothing about whether any claim using it is \
                        true.",
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

/// The highest revision of this package the Space has ever installed.
///
/// Read from the installed set rather than from what is in force, because the
/// two differ exactly in the case this exists to survive — an artifact
/// installed by a publish that then failed to activate. The installed set holds
/// a handful of packages (Core, the Profile, this one), so enumerating it is
/// cheaper than the failure it prevents.
async fn highest_installed_revision(nexus: &CognitiveNexus) -> Result<u32, BoxError> {
    let prefix = format!("{MEMORY_PACKAGE_ID}@");
    Ok(nexus
        .store
        .installed_packages()
        .await?
        .keys()
        .filter_map(|package_ref| patch_of(package_ref.strip_prefix(&prefix)?))
        .max()
        .unwrap_or(0))
}

/// Lets Formation and Maintenance grow this Space's vocabulary.
///
/// The tool exists because KIP 2.0 deliberately took schema out of the language
/// a model writes: KML cannot declare a type, so a model that needs one has to
/// ask the host. What the host adds on top is what makes that worth doing —
/// name validation, a cap, and a version — so the set of things this Brain can
/// say stays a reviewable artifact instead of whatever its models happened to
/// emit.
///
/// Not registered for Recall, which is read-only.
#[derive(Clone)]
pub struct DeclareSymbolsTool {
    memory: Arc<MemoryManagement>,
    /// Serializes publication. Two concurrent extends would each read the same
    /// vocabulary, add their own symbol, and publish — the second overwriting
    /// the first's, which is how a declared symbol quietly stops existing.
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl DeclareSymbolsTool {
    /// Function name used when registering the tool.
    pub const NAME: &'static str = "declare_memory_symbols";

    /// Creates the tool over one space's memory.
    pub fn new(memory: Arc<MemoryManagement>) -> Self {
        Self {
            memory,
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Publishes the symbols and returns the names it refused.
    pub async fn declare(
        &self,
        args: &DeclareSymbolsArgs,
    ) -> Result<(Json, Vec<String>), BoxError> {
        let _guard = self.lock.lock().await;
        let nexus = self.memory.nexus();
        let mut vocabulary = MemoryVocabulary::load(nexus.as_ref()).await?;
        let before = vocabulary.revision;
        let rejected = vocabulary.extend(
            args.types.iter().map(String::as_str),
            args.predicates.iter().map(String::as_str),
        );
        if vocabulary.revision != before {
            vocabulary.activate(nexus.as_ref()).await?;
        }
        Ok((
            json!({
                "package_ref": vocabulary.package_ref(),
                "types": vocabulary.types,
                "predicates": vocabulary.predicates,
            }),
            rejected,
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
        _ctx: BaseCtx,
        args: Self::Args,
        _resources: Vec<Resource>,
    ) -> Result<ToolOutput<Self::Output>, BoxError> {
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
    fn an_empty_vocabulary_publishes_as_the_first_revision() {
        let vocabulary = MemoryVocabulary::default();
        assert_eq!(vocabulary.package_ref(), "kip://anda-brain/memory@1.0.0");
        assert!(vocabulary.covers([], []));
        assert!(!vocabulary.covers(["Drug"], []));
    }

    #[test]
    fn adding_a_symbol_moves_the_version_forward_and_re_adding_does_not() {
        let mut vocabulary = MemoryVocabulary::default();
        assert!(vocabulary.extend(["Drug"], ["treats"]).is_empty());
        assert_eq!(vocabulary.version(), "1.0.1");
        // A version that went nowhere would still mint a new Schema
        // Environment version on activation, invalidating every client's
        // `schema_environment_version` precondition for no change at all.
        assert!(vocabulary.extend(["Drug"], ["treats"]).is_empty());
        assert_eq!(vocabulary.version(), "1.0.1");
        assert!(vocabulary.covers(["Drug"], ["treats"]));
    }

    #[test]
    fn a_symbol_another_package_declares_is_usable_but_never_redeclared() {
        // Two active packages defining `Person` make every reference to it
        // ambiguous, and the engine refuses the whole write rather than pick.
        let mut vocabulary = MemoryVocabulary {
            borrowed_types: BTreeSet::from(["Person".to_string()]),
            borrowed_predicates: BTreeSet::from(["prefers".to_string()]),
            ..Default::default()
        };
        assert!(vocabulary.covers(["Person"], ["prefers"]));
        assert!(vocabulary.extend(["Person"], ["prefers"]).is_empty());
        assert!(vocabulary.types.is_empty());
        assert_eq!(vocabulary.revision, 0, "nothing was published");
    }

    #[test]
    fn malformed_names_are_reported_rather_than_published() {
        let mut vocabulary = MemoryVocabulary::default();
        let rejected = vocabulary.extend(["drug", "medical device", "Ok"], ["Treats", "fine_one"]);
        assert_eq!(rejected, vec!["drug", "medical device", "Treats"]);
        assert_eq!(vocabulary.types, BTreeSet::from(["Ok".to_string()]));
        assert_eq!(
            vocabulary.predicates,
            BTreeSet::from(["fine_one".to_string()])
        );
    }

    #[test]
    fn the_cap_counts_this_package_whole_and_nobody_else() {
        let mut vocabulary = MemoryVocabulary::default();
        // Another package's symbols are not this one's to be charged for.
        vocabulary
            .borrowed_types
            .extend((0..40).map(|i| format!("Borrowed{i}")));

        let types: Vec<String> = (0..MAX_SYMBOLS).map(|i| format!("T{i}")).collect();
        let rejected = vocabulary.extend(types.iter().map(String::as_str), []);
        assert!(rejected.is_empty(), "{} rejected", rejected.len());
        assert_eq!(vocabulary.len(), MAX_SYMBOLS);

        // Types and predicates share one budget: the cap is on the artifact
        // this package republishes, and it holds both.
        let rejected = vocabulary.extend(["OneMore"], ["one_more"]);
        assert_eq!(rejected, vec!["OneMore", "one_more"]);
        assert_eq!(vocabulary.len(), MAX_SYMBOLS);
    }

    #[test]
    fn a_revision_never_reuses_a_number_already_installed() {
        // The in-force package is 1.0.4, but 1.0.5 and 1.0.6 were installed by
        // publishes that never activated. Re-minting either with different
        // content is a permanent DigestMismatch.
        let mut vocabulary = MemoryVocabulary {
            revision: 4,
            floor: 6,
            ..Default::default()
        };

        assert!(vocabulary.extend(["Drug"], []).is_empty());
        assert_eq!(vocabulary.version(), "1.0.7");

        assert!(vocabulary.extend([], ["treats"]).is_empty());
        assert_eq!(vocabulary.version(), "1.0.8");
    }

    #[test]
    fn the_artifact_declares_exactly_the_symbols_it_holds() {
        let mut vocabulary = MemoryVocabulary::default();
        vocabulary.extend(["Drug", "Symptom"], ["treats"]);
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
    fn the_vocabulary_stops_widening_at_its_cap() {
        let mut vocabulary = MemoryVocabulary::default();
        let names: Vec<String> = (0..MAX_SYMBOLS + 10).map(|i| format!("Type{i}")).collect();
        let rejected = vocabulary.extend(names.iter().map(String::as_str), []);
        assert_eq!(vocabulary.len(), MAX_SYMBOLS);
        assert_eq!(rejected.len(), 10);
    }
}

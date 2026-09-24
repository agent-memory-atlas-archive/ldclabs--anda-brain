//! The write side, on the KIP 2.0 engine: the Space's Nexus is rebuilt from
//! nothing inside the same database, so no draft artifact, draft Schema
//! Environment or element version survives, while the host's own collections
//! (conversations, ledgers, wiki) and extensions stay untouched.
use anda_cognitive_nexus::{
    CognitiveNexus,
    capsule::SymbolMapping,
    governance::{AuthContext, EffectiveAuthority},
    nexus::DEFAULT_SPACE,
    schema::{PackageState, SchemaLock, SchemaPackage},
    store::{Element, planes::PlaneKey, space::JournalEntry},
    tx::{Guard, Transaction},
};
use anda_db::database::{AndaDB, DBConfig};
use anda_kip::{Executor, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use crate::{BoxError, export::Exported};

const DRAFT_PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";
const PROFILE: &str = "kip://profiles/cognitive-memory@2.0.0/";
const PROFILE_ID: &str = "kip://profiles/cognitive-memory";
const DRAFT_VOCABULARY: &str = "kip://local/draft@0.0.0/";

/// The collections a Cognitive Nexus owns. Everything else in the database
/// belongs to the host and is kept.
const NEXUS_COLLECTIONS: &[&str] = &[
    "concepts",
    "propositions",
    "assertions",
    "evidence",
    "activities",
    "spaces",
    "transactions",
    "schema_packages",
    "schema_envs",
    "element_versions",
    "kip_control_records",
    "kip_commit_log",
    "kip_exposures",
    "kip_legacy_v1",
    "gov_principals",
    "gov_principal_groups",
    "gov_actor_bindings",
    "gov_grants",
    "gov_delegations",
    "gov_policies",
    "gov_approvals",
    "gov_audit",
];

/// The option type one draft-typed Concept becomes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionType {
    #[serde(rename = "type")]
    pub name: String,
    pub description: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub dropped_collections: Vec<String>,
    pub kept_collections: Vec<String>,
    pub packages: Vec<String>,
    pub defined_types: Vec<String>,
    pub imported: BTreeMap<String, usize>,
    pub id_changes: Vec<(String, String)>,
    pub lineage_activities: Vec<String>,
    pub keys_restored: usize,
    pub archived_restored: usize,
    pub self_concept: String,
    pub warnings: Vec<String>,
}

fn rewrite(reference: &str) -> String {
    match reference.strip_prefix(DRAFT_PROFILE) {
        Some(name) => format!("{PROFILE}{name}"),
        None => reference.to_string(),
    }
}

fn rewrite_keys(map: &mut Json) {
    if let Some(object) = map.as_object_mut() {
        let entries = std::mem::take(object);
        for (key, value) in entries {
            object.insert(rewrite(&key), value);
        }
    }
}

fn number(id: &str) -> u64 {
    id.split_once('-')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(u64::MAX)
}

fn kind_rank(kind: &str) -> usize {
    [
        "concept",
        "proposition",
        "evidence",
        "assertion",
        "activity",
    ]
    .iter()
    .position(|k| *k == kind)
    .unwrap_or(usize::MAX)
}

/// The draft-typed option Concepts: the draft `Preference` type is gone, and
/// each option is a Concept typed by its kind (Profile `prefers`).
pub fn option_concepts(exported: &Exported) -> Vec<&Json> {
    exported.capsule["payload"]["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["schema_ref"].as_str() == Some(&format!("{DRAFT_PROFILE}Preference")))
        .collect()
}

/// Rewrites the exported Capsule onto the KIP 2.0 Profile: symbol references
/// move to `cognitive-memory@2.0.0`, each draft `Preference` becomes the
/// draft-vocabulary type the owner chose for it, and the lineage the draft
/// stored in `derived_from` (now computed, not written) is kept as the
/// `extraction` Activity that produced the Concept.
fn transform(
    exported: &Exported,
    types: &BTreeMap<String, OptionType>,
    packages: &[String],
    report: &mut Report,
) -> Result<Json, BoxError> {
    let mut capsule = exported.capsule.clone();
    let records = capsule["payload"]["records"]
        .as_array_mut()
        .ok_or("the exported Capsule has no records")?;
    let mut missing = Vec::new();
    let mut lineage: Vec<(String, Vec<Json>)> = Vec::new();
    let mut produced: BTreeSet<(String, String)> = BTreeSet::new();
    for record in records.iter_mut() {
        let id = record["id"].as_str().unwrap_or_default().to_string();
        for field in ["schema_ref", "predicate_ref"] {
            if let Some(reference) = record[field].as_str() {
                record[field] = Json::String(rewrite(reference));
            }
        }
        if record["schema_ref"].as_str() == Some(&format!("{PROFILE}Preference")) {
            match types.get(&id) {
                Some(option) => {
                    record["schema_ref"] =
                        Json::String(format!("{DRAFT_VOCABULARY}{}", option.name))
                }
                None => missing.push(id.clone()),
            }
        }
        rewrite_keys(&mut record["structural"]);
        rewrite_keys(&mut record["facets"]);
        if record["kind"] == "concept" {
            record["canonical_id"] = Json::String(canonical(&id));
        }
        if let Some(structural) = record["structural"].as_object_mut()
            && let Some(sources) = structural.remove(&format!("{PROFILE}derived_from"))
        {
            let sources: Vec<Json> = sources.as_array().cloned().unwrap_or_default();
            if !sources.is_empty() {
                lineage.push((id.clone(), sources));
            }
        }
        if record["kind"] == "activity" {
            for output in record["outputs"].as_array().into_iter().flatten() {
                for input in record["inputs"].as_array().into_iter().flatten() {
                    if let (Some(o), Some(i)) = (output["id"].as_str(), input["id"].as_str()) {
                        produced.insert((i.to_string(), o.to_string()));
                    }
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "no option type was chosen for {}; add them to the types file (see --list-options)",
            missing.join(", ")
        )
        .into());
    }

    let mut next_activity = records
        .iter()
        .filter(|r| r["kind"] == "activity")
        .map(|r| number(r["id"].as_str().unwrap_or_default()))
        .max()
        .unwrap_or(0);
    for (concept, sources) in lineage {
        let inputs: Vec<Json> = sources
            .into_iter()
            .filter(|s| {
                s["id"]
                    .as_str()
                    .is_some_and(|i| !produced.contains(&(i.to_string(), concept.clone())))
            })
            .map(|s| json!({"id": s["id"]}))
            .collect();
        if inputs.is_empty() {
            continue;
        }
        next_activity += 1;
        let id = format!("X-{next_activity}");
        report.lineage_activities.push(format!("{id}: {concept}"));
        records.push(json!({
            "id": id,
            "kind": "activity",
            "space_id": DEFAULT_SPACE,
            "activity_class": "extraction",
            "status": "completed",
            "inputs": inputs,
            "outputs": [{"id": concept}],
            "governance": {"authority_class": "descriptive"},
        }));
    }

    // Id order within kind, so the import mints the source's ids.
    records.sort_by(|a, b| {
        let key = |r: &Json| {
            (
                kind_rank(r["kind"].as_str().unwrap_or_default()),
                number(r["id"].as_str().unwrap_or_default()),
            )
        };
        key(a).cmp(&key(b))
    });
    let roots: Vec<Json> = records.iter().map(|r| r["id"].clone()).collect();
    capsule["payload"]["manifest"]["roots"] = Json::Array(roots);
    capsule["payload"]["schema_dependencies"] = Json::Array(
        packages
            .iter()
            .map(|package| json!({"package_ref": package}))
            .collect(),
    );
    Ok(capsule)
}

/// What one transaction may write: its redo plan carries every written row
/// and holds at most 4096 array entries and 16384 JSON nodes.
const CHUNK_ELEMENTS: usize = 500;
const CHUNK_NODES: usize = 10_000;

/// The JSON nodes a value serializes to.
fn nodes(value: &Json) -> usize {
    1 + match value {
        Json::Array(items) => items.iter().map(nodes).sum(),
        Json::Object(map) => map.values().map(nodes).sum(),
        _ => 0,
    }
}

/// Packs records into runs that fit one transaction's plan.
fn pack<'a>(records: &[&'a Json]) -> Vec<Vec<&'a Json>> {
    let mut runs: Vec<Vec<&Json>> = Vec::new();
    let mut size = 0;
    for record in records {
        let weight = nodes(record);
        match runs.last_mut() {
            Some(run) if run.len() < CHUNK_ELEMENTS && size + weight <= CHUNK_NODES => {
                run.push(record);
                size += weight;
            }
            _ => {
                runs.push(vec![record]);
                size = weight;
            }
        }
    }
    runs
}

/// The temporary identity a later chunk resolves an already imported Concept
/// by; cleared once every chunk is in.
fn canonical(id: &str) -> String {
    format!("urn:anda-brain:migrate-draft-space:{id}")
}

/// Splits the records into dependency-ordered imports that each fit one
/// transaction: Concepts, then Propositions, Evidence and Activities, then
/// Assertions. A chunk carries the records it references: Concepts resolve by
/// their temporary canonical id and Propositions by their tuple, so neither is
/// written twice. Evidence and Activities, which have no such identity, travel
/// together, and nothing else references them.
fn chunks(capsule: &Json) -> Result<Vec<Vec<Json>>, BoxError> {
    let records = capsule["payload"]["records"]
        .as_array()
        .ok_or("the Capsule has no records")?;
    let by_id: BTreeMap<&str, &Json> = records
        .iter()
        .filter_map(|r| r["id"].as_str().map(|id| (id, r)))
        .collect();
    let of = |kind: &str| -> Vec<&Json> { records.iter().filter(|r| r["kind"] == kind).collect() };
    let references = |record: &Json, prefix: char| -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        fn walk(value: &Json, prefix: char, own: &str, found: &mut BTreeSet<String>) {
            match value {
                Json::String(s)
                    if s.starts_with(prefix)
                        && s[1..].starts_with('-')
                        && s[2..].chars().all(|c| c.is_ascii_digit())
                        && s != own =>
                {
                    found.insert(s.clone());
                }
                Json::Array(items) => items.iter().for_each(|v| walk(v, prefix, own, found)),
                Json::Object(map) => map
                    .iter()
                    .filter(|(k, _)| k.as_str() != "_system" && k.as_str() != "governance")
                    .for_each(|(_, v)| walk(v, prefix, own, found)),
                _ => {}
            }
        }
        walk(
            record,
            prefix,
            record["id"].as_str().unwrap_or_default(),
            &mut found,
        );
        found
    };
    let with = |members: &[&Json], prefixes: &[char]| -> Result<Vec<Json>, BoxError> {
        let mut carried: BTreeSet<String> = BTreeSet::new();
        for member in members {
            for prefix in prefixes {
                carried.extend(references(member, *prefix));
            }
        }
        // A carried Proposition resolves by its tuple, so its endpoints come too.
        let endpoints: Vec<String> = carried
            .iter()
            .filter(|id| id.starts_with("P-"))
            .filter_map(|id| by_id.get(id.as_str()))
            .flat_map(|record| references(record, 'C'))
            .collect();
        carried.extend(endpoints);
        let mut chunk = Vec::new();
        for id in carried {
            let record = by_id
                .get(id.as_str())
                .ok_or_else(|| format!("{id} is referenced but not exported"))?;
            chunk.push((*record).clone());
        }
        chunk.extend(members.iter().map(|r| (*r).clone()));
        Ok(chunk)
    };
    let mut out = Vec::new();
    for part in pack(&of("concept")) {
        out.push(with(&part, &['C'])?);
    }
    for part in pack(&of("proposition")) {
        out.push(with(&part, &['C'])?);
    }
    let mut events: Vec<&Json> = of("evidence");
    events.extend(of("activity"));
    if !events.is_empty() {
        if pack(&events).len() > 1 {
            return Err("Evidence and Activities must fit one transaction".into());
        }
        out.push(with(&events, &['C'])?);
    }
    for part in pack(&of("assertion")) {
        out.push(with(&part, &['C', 'P'])?);
    }
    Ok(out)
}

async fn run(nexus: &CognitiveNexus, text: &str, parameters: Json) -> Result<Json, BoxError> {
    let mut request = Request::single(text);
    if let Json::Object(map) = parameters {
        request.parameters = Some(map);
    }
    let response = nexus
        .system_session()
        .execute(anda_kip::parse_kip(text)?, &request, &request.operations[0])
        .await;
    if response.status != anda_kip::TopLevelStatus::Succeeded {
        return Err(format!("{text}: {}", serde_json::to_string(&response.error)?).into());
    }
    Ok(response.first_result().cloned().unwrap_or(Json::Null))
}

async fn open(root: &Path, space: &str) -> Result<Arc<AndaDB>, BoxError> {
    let os = object_store::local::LocalFileSystem::new_with_prefix(root)?;
    let os = anda_object_store::MetaStoreBuilder::new(os, 100000).build();
    let storage = crate::export::storage_config();
    Ok(Arc::new(
        AndaDB::open(
            Arc::new(os),
            DBConfig {
                name: space.to_string(),
                description: "Anda Brain database".into(),
                storage: anda_db::storage::StorageConfig {
                    cache_max_capacity: storage.cache_max_capacity,
                    cache_max_bytes: storage.cache_max_bytes,
                    compress_level: storage.compress_level,
                    object_chunk_size: storage.object_chunk_size,
                    bucket_overload_size: storage.bucket_overload_size,
                    max_small_object_size: storage.max_small_object_size,
                },
                lock: None,
            },
        )
        .await?,
    ))
}

pub async fn rebuild(
    root: &Path,
    space: &str,
    exported: &Exported,
    types: &BTreeMap<String, OptionType>,
) -> Result<Report, BoxError> {
    let mut report = Report::default();

    // The generated legacy package is this Space's own artifact: read it from
    // the old Nexus before that Nexus is dropped.
    let db = open(root, space).await?;
    let old = CognitiveNexus::connect(db.clone()).await?;
    let legacy: Vec<Arc<SchemaPackage>> = old
        .store
        .installed_packages()
        .await?
        .into_iter()
        .filter(|(reference, _)| reference.starts_with("kip://legacy/"))
        .map(|(_, package)| package)
        .collect();
    old.close().await?;

    let db = open(root, space).await?;
    for name in db.metadata().collections {
        if NEXUS_COLLECTIONS.contains(&name.as_str()) {
            db.delete_collection(&name).await?;
            report.dropped_collections.push(name);
        } else {
            report.kept_collections.push(name);
        }
    }

    // A clean reopen, so the fresh Nexus sees only what the drop left.
    db.close().await?;
    let db = open(root, space).await?;
    println!(
        "dropped {} Nexus collections; bootstrapping a fresh Nexus",
        report.dropped_collections.len()
    );
    let nexus = CognitiveNexus::connect(db.clone()).await?;
    let profile = SchemaPackage::parse(anda_cognitive_nexus::profiles::COGNITIVE_MEMORY)?;
    let mut lock = SchemaLock::default();
    for package in legacy.iter().map(|p| p.as_ref()).chain([&profile]) {
        nexus
            .install_package(package, "migrate-draft-space")
            .await?;
        let manifest = &package.manifest;
        lock.packages
            .insert(manifest.package_id.clone(), manifest.version.clone());
        lock.states
            .insert(manifest.package_id.clone(), PackageState::Active);
        report.packages.push(manifest.package_ref.clone());
    }
    debug_assert!(lock.packages.contains_key(PROFILE_ID));
    nexus.activate_schema(DEFAULT_SPACE, lock).await?;

    // Each option kind is a draft-vocabulary symbol (Spec §20.16).
    let mut defined: BTreeMap<String, String> = BTreeMap::new();
    for option in types.values() {
        defined
            .entry(option.name.clone())
            .or_insert_with(|| option.description.clone());
    }
    for (name, description) in &defined {
        run(
            &nexus,
            "DEFINE CONCEPT TYPE :name {description: :description}",
            json!({"name": name, "description": description}),
        )
        .await?;
        report.defined_types.push(name.clone());
    }

    let packages = report.packages.clone();
    let capsule = transform(exported, types, &packages, &mut report)?;
    let symbols: Vec<SymbolMapping> = defined
        .keys()
        .map(|name| SymbolMapping {
            kind: "ConceptType".into(),
            from: format!("{DRAFT_VOCABULARY}{name}"),
            to: name.clone(),
        })
        .collect();
    let mut mapping: BTreeMap<String, String> = BTreeMap::new();
    let all = chunks(&capsule)?;
    let total = all.len();
    for (index, chunk) in all.into_iter().enumerate() {
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        for record in &chunk {
            *kinds
                .entry(record["kind"].as_str().unwrap_or_default().to_string())
                .or_default() += 1;
        }
        if (index + 1) % 20 == 0 || index + 1 == total {
            println!(
                "import {}/{total}: last {} ({kinds:?})",
                index + 1,
                chunk
                    .last()
                    .and_then(|r| r["id"].as_str())
                    .unwrap_or_default()
            );
        }
        let mut part = capsule.clone();
        part["payload"]["manifest"]["roots"] =
            Json::Array(chunk.iter().map(|r| r["id"].clone()).collect());
        part["payload"]["records"] = Json::Array(chunk);
        let mut part: anda_kip::Capsule = serde_json::from_value(part)?;
        part.integrity.content_digest =
            anda_cognitive_nexus::capsule::payload_digest(&part.payload)?;
        let imported = anda_cognitive_nexus::capsule::import(
            &nexus,
            &part,
            DEFAULT_SPACE,
            false,
            AuthContext::system(),
            false,
            &symbols,
        )
        .await?;
        for (kind, count) in imported.counts {
            *report.imported.entry(kind).or_default() += count;
        }
        for warning in imported.warnings {
            if !report.warnings.contains(&warning) {
                report.warnings.push(warning);
            }
        }
        mapping.extend(imported.mapping);
    }
    for (source, destination) in &mapping {
        if source != destination {
            report
                .id_changes
                .push((source.clone(), destination.clone()));
        }
    }
    let target =
        |id: &str| -> String { mapping.get(id).cloned().unwrap_or_else(|| id.to_string()) };

    // What an import leaves out. A Space-local key is not an import's to
    // bring (§5.3) nor an UPDATE's to move, so the migration restores the
    // source's own keys directly, and drops the temporary canonical ids the
    // chunked import resolved Concepts by.
    let facts: BTreeMap<&str, &crate::export::Fact> =
        exported.facts.iter().map(|f| (f.id.as_str(), f)).collect();
    let weights: BTreeMap<&str, usize> = exported.capsule["payload"]["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(|id| (id, nodes(r))))
        .collect();
    let mut batches: Vec<Vec<&crate::export::Fact>> = Vec::new();
    let mut size = 0;
    for fact in exported.facts.iter().filter(|f| f.kind == "C") {
        let weight = weights.get(fact.id.as_str()).copied().unwrap_or(1);
        match batches.last_mut() {
            Some(batch) if batch.len() < CHUNK_ELEMENTS && size + weight <= CHUNK_NODES => {
                batch.push(fact);
                size += weight;
            }
            _ => {
                batches.push(vec![fact]);
                size = weight;
            }
        }
    }
    for batch in &batches {
        let auth = AuthContext::system();
        let authority = EffectiveAuthority::resolve(&nexus.store, DEFAULT_SPACE, &auth).await?;
        let mut tx = Transaction::begin(
            &nexus.store,
            DEFAULT_SPACE,
            json!({"migration": "cognitive-memory@2.1.0 draft to 2.0.0"}),
            false,
            authority,
            auth,
        )
        .await?;
        for fact in batch {
            let id: anda_cognitive_nexus::ElementId = target(&fact.id).parse()?;
            // Durable records (SleepTask, Watch) change only under a guard.
            let version = tx.load(id).await?.version();
            tx.expect_versions(
                id,
                &[Guard {
                    version,
                    plane: PlaneKey::Element,
                }],
            )
            .await?;
            if let Element::Concept(row) = tx.load(id).await? {
                row.key = fact.key.clone();
                row.canonical_id.clear();
            }
            tx.mark_changed(id, anda_kip::ChangeOp::Update);
            if !fact.key.is_empty() {
                report.keys_restored += 1;
            }
        }
        tx.commit(JournalEntry::default()).await?;
    }
    for fact in facts.values() {
        if fact.state == "archived" {
            match run(
                &nexus,
                "TRANSITION :id TO \"archived\"",
                json!({"id": target(&fact.id)}),
            )
            .await
            {
                Ok(_) => report.archived_restored += 1,
                Err(error) => report
                    .warnings
                    .push(format!("state of {}: {error}", fact.id)),
            }
        } else if fact.state != "active" {
            report.warnings.push(format!(
                "{} was {}; imported as active",
                fact.id, fact.state
            ));
        }
    }
    if !exported.self_concept.is_empty() {
        let id = target(&exported.self_concept);
        nexus
            .system_session()
            .designate_self(DEFAULT_SPACE, Some(id.parse()?))
            .await?;
        report.self_concept = id;
    }
    nexus.close().await?;
    Ok(report)
}

/// Counts by kind, storage state and type, for the acceptance comparison.
pub async fn census(root: &Path, space: &str) -> Result<BTreeMap<String, usize>, BoxError> {
    let db = open(root, space).await?;
    let nexus = CognitiveNexus::connect(db).await?;
    let mut census = BTreeMap::new();
    for (kind, prefix) in [
        (anda_kip::ElementKind::Concept, "C"),
        (anda_kip::ElementKind::Proposition, "P"),
        (anda_kip::ElementKind::Evidence, "E"),
        (anda_kip::ElementKind::Assertion, "A"),
        (anda_kip::ElementKind::Activity, "X"),
    ] {
        for id in nexus.store.elements(kind).ids() {
            let element = nexus
                .store
                .get_element(format!("{prefix}-{id}").parse()?)
                .await?;
            *census
                .entry(format!(
                    "{prefix} {} {}",
                    element.state(),
                    element.schema_ref()
                ))
                .or_default() += 1;
        }
    }
    let environment = run(&nexus, "DESCRIBE SCHEMA ENVIRONMENT", json!({})).await?;
    census.insert(format!("environment {environment}"), 1);
    let packages: Vec<String> = nexus
        .store
        .installed_packages()
        .await?
        .into_keys()
        .collect();
    census.insert(format!("installed {packages:?}"), 1);
    nexus.close().await?;
    Ok(census)
}

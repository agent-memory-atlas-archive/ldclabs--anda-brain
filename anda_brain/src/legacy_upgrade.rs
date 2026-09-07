//! What happens when a KIP 1.x space is opened by this build.
//!
//! The Cognitive Nexus migrates a 1.x layout automatically and resumably on
//! first open: the 1.x rows are staged verbatim, the colliding collection names
//! are cleared, and every row becomes a 2.0 element with `mode: "imported"` —
//! a database row is not an observation, and the migration refuses to invent
//! one. Nothing here has to ask for that.
//!
//! The part this test exists to pin down is *which vocabulary* the migrated
//! elements land on. A 1.x Anda Brain used `Person`, `Event`, `SleepTask`,
//! `Insight`, `Commitment` and `Preference`; the Cognitive Memory Profile this
//! service activates declares all six. If the migration minted its own
//! `Person` beside the profile's, every command naming the bare local name —
//! which is every command this service and its prompts issue — would fail with
//! `SchemaSymbolAmbiguous` on a space whose data had migrated perfectly.
//!
//! So the load waits for the host to say what its vocabulary is (it runs from
//! `ensure_schema`, not from `connect`), and adopts the host's symbol wherever
//! the name matches. What that buys, and what this test asserts:
//!
//! - a migrated `Person` *is* a profile `Person`, so recall, formation and the
//!   self-test all see it;
//! - a migrated `prefers` is the profile's, so `BELIEF` over it works;
//! - a type the profile does not have (`Topic`) keeps a legacy symbol, because
//!   a 1.x deployment's own word means whatever that deployment meant by it;
//! - 1.x `(type, name)` identity becomes a 2.0 `key`, so
//!   `get_or_init_counterparty` resolves the migrated Person instead of
//!   minting a second one beside it.

use crate::kip;
use anda_cognitive_nexus::CognitiveNexus;
use anda_db::{
    collection::CollectionConfig,
    database::{AndaDB, DBConfig},
    schema::{Document, FieldEntry, FieldType, Fv, Schema, SchemaBuilder},
    storage::StorageConfig,
};
use object_store::memory::InMemory;
use std::sync::Arc;

/// A 1.x `concepts` collection: `type` and `metadata`, and no `space`. That is
/// the shape `migrate::prepare` recognises.
fn v1_concepts_schema() -> Schema {
    let mut b = SchemaBuilder::new();
    for (name, ft) in [
        ("type", FieldType::Text),
        ("name", FieldType::Text),
        ("attributes", FieldType::Json),
        ("metadata", FieldType::Json),
    ] {
        b.add_field(FieldEntry::new(name.to_string(), ft).unwrap())
            .unwrap();
    }
    b.build().unwrap()
}

fn v1_propositions_schema() -> Schema {
    let mut b = SchemaBuilder::new();
    for (name, ft) in [
        ("subject", FieldType::Text),
        ("object", FieldType::Text),
        ("predicates", FieldType::Array(vec![FieldType::Text])),
        ("properties", FieldType::Json),
    ] {
        b.add_field(FieldEntry::new(name.to_string(), ft).unwrap())
            .unwrap();
    }
    b.build().unwrap()
}

/// Writes a small 1.x space: `$self`, a counterparty, a topic, and one
/// `prefers` link between the last two.
async fn write_v1_space(store: Arc<InMemory>, config: DBConfig) {
    let db = Arc::new(AndaDB::create(store, config).await.unwrap());
    let concepts = db
        .open_or_create_collection(
            v1_concepts_schema(),
            CollectionConfig {
                name: "concepts".to_string(),
                description: "1.x concepts".to_string(),
            },
            async |_| Ok(()),
        )
        .await
        .unwrap();
    for (r#type, name) in [
        ("Person", "$self"),
        ("Person", "alice_id"),
        ("Topic", "dark_mode"),
    ] {
        let mut doc = Document::new(concepts.schema());
        doc.set_field("_id", Fv::U64(0)).unwrap();
        doc.set_field("type", Fv::Text(r#type.to_string())).unwrap();
        doc.set_field("name", Fv::Text(name.to_string())).unwrap();
        doc.set_field("attributes", Fv::Json(serde_json::json!({})))
            .unwrap();
        doc.set_field(
            "metadata",
            Fv::Json(serde_json::json!({"author": "$self", "confidence": 0.9})),
        )
        .unwrap();
        concepts.add(doc).await.unwrap();
    }
    concepts.flush(anda_engine::unix_ms()).await.unwrap();

    let propositions = db
        .open_or_create_collection(
            v1_propositions_schema(),
            CollectionConfig {
                name: "propositions".to_string(),
                description: "1.x propositions".to_string(),
            },
            async |_| Ok(()),
        )
        .await
        .unwrap();
    let mut doc = Document::new(propositions.schema());
    doc.set_field("_id", Fv::U64(0)).unwrap();
    doc.set_field("subject", Fv::Text("C:2".to_string()))
        .unwrap();
    doc.set_field("object", Fv::Text("C:3".to_string()))
        .unwrap();
    doc.set_field(
        "predicates",
        Fv::Array(vec![Fv::Text("prefers".to_string())]),
    )
    .unwrap();
    // 1.x kept per-predicate properties, each with its own confidence — which
    // is why one row fans out into one Assertion per predicate rather than one
    // for the row.
    doc.set_field(
        "properties",
        Fv::Json(
            serde_json::json!({"prefers": {"metadata": {"confidence": 0.9, "author": "$self"}}}),
        ),
    )
    .unwrap();
    propositions.add(doc).await.unwrap();
    propositions.flush(anda_engine::unix_ms()).await.unwrap();
    db.close().await.unwrap();
}

/// One read against a freshly upgraded space.
async fn read(nexus: &CognitiveNexus, command: &str) -> Result<serde_json::Value, String> {
    let response = anda_kip::execute_request(nexus, &kip::request(command)).await;
    match kip::ok_result(&response) {
        Some(result) => Ok(result.clone()),
        None => Err(kip::error_message(&response)),
    }
}

#[tokio::test]
async fn a_kip_1x_space_migrates_onto_the_vocabulary_this_service_activates() {
    let store = Arc::new(InMemory::new());
    let config = DBConfig {
        name: "legacy_space".to_string(),
        description: "a KIP 1.x database".to_string(),
        storage: StorageConfig::default(),
        lock: None,
    };
    write_v1_space(store.clone(), config.clone()).await;

    // Reopen exactly as `Space::connect` does: connect, then activate.
    let db = Arc::new(AndaDB::open(store, config).await.unwrap());
    let nexus = CognitiveNexus::connect(db).await.unwrap();
    crate::vocabulary::MemoryVocabulary::load(&nexus)
        .await
        .unwrap()
        .activate(&nexus)
        .await
        .unwrap();

    // The bare local name resolves — which is the whole point, since every
    // command this service and its prompts issue uses one.
    let people = read(
        &nexus,
        r#"FIND(?c.name, ?c.key) WHERE { ?c CONCEPT {type: "Person"} } ORDER BY ?c.name"#,
    )
    .await
    .unwrap();
    assert_eq!(
        people.to_string(),
        r#"[["$self","$self"],["alice_id","alice_id"]]"#,
        "1.x (type, name) identity should have become a 2.0 key"
    );

    // …and it is the profile's Person, not a migrated duplicate of it.
    let refs = read(
        &nexus,
        r#"FIND(?c.schema_ref) WHERE { ?c CONCEPT {type: "Person"} }"#,
    )
    .await
    .unwrap();
    assert!(
        refs.to_string()
            .contains("kip://profiles/cognitive-memory@2.1.0/Person"),
        "{refs}"
    );

    // A counterparty lookup resolves the migrated Person rather than missing it.
    let alice = read(
        &nexus,
        r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: "alice_id"} }"#,
    )
    .await
    .unwrap();
    assert_eq!(alice.as_array().map(Vec::len), Some(1), "{alice}");

    // The migrated link uses the profile predicate, so BELIEF can see it.
    let predicates = read(
        &nexus,
        r#"FIND(?p.predicate_ref) WHERE { ?p (?s, ?pred, ?o) }"#,
    )
    .await
    .unwrap();
    assert_eq!(
        predicates.to_string(),
        r#"["kip://profiles/cognitive-memory@2.1.0/prefers"]"#
    );

    // A type the profile does not declare keeps its legacy symbol: this
    // deployment's `Topic` means what this deployment meant by it, and nothing
    // here is entitled to decide it meant something standard.
    let topic = read(
        &nexus,
        r#"FIND(?c.schema_ref) WHERE { ?c CONCEPT {type: "Topic"} }"#,
    )
    .await
    .unwrap();
    assert_eq!(topic.to_string(), r#"["kip://legacy/nexus@1.0.0/Topic"]"#);

    // Every migrated claim is `imported`, attributed to the migration actor:
    // 1.x recorded a row, not a speaker, and the migration does not promote one
    // into the other.
    let claims = read(
        &nexus,
        r#"FIND(?a.mode, ?a.confidence, ?a.asserted_by) WHERE { ?a ASSERTION {} }"#,
    )
    .await
    .unwrap();
    let self_id = read(
        &nexus,
        r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: "$self"} }"#,
    )
    .await
    .unwrap();
    let self_id = self_id[0].as_str().unwrap();
    assert_eq!(
        claims.to_string(),
        format!(r#"[["imported",0.9,{{"id":"{self_id}"}}]]"#),
        "a 1.x author naming exactly one migrated Concept is a speaker the old \
         system really did record"
    );
}

/// A host that activates nothing still gets its data, against a generated
/// legacy package — the best mapping available when nothing better was
/// declared.
#[tokio::test]
async fn a_host_that_declares_no_vocabulary_can_still_finish_the_migration() {
    let store = Arc::new(InMemory::new());
    let config = DBConfig {
        name: "bare_legacy_space".to_string(),
        description: "a KIP 1.x database".to_string(),
        storage: StorageConfig::default(),
        lock: None,
    };
    write_v1_space(store.clone(), config.clone()).await;

    let db = Arc::new(AndaDB::open(store, config).await.unwrap());
    let nexus = CognitiveNexus::connect(db).await.unwrap();
    // Nothing activated: the load is still pending, and the 1.x rows are
    // staged rather than lost.
    assert_eq!(
        read(&nexus, r#"FIND(?c.id) WHERE { ?c CONCEPT {} }"#)
            .await
            .unwrap()
            .to_string(),
        "[]"
    );

    nexus.finish_migration().await.unwrap();
    let people = read(
        &nexus,
        r#"FIND(?c.name) WHERE { ?c CONCEPT {type: "Person"} } ORDER BY ?c.name"#,
    )
    .await
    .unwrap();
    assert_eq!(people.to_string(), r#"["$self","alice_id"]"#);
}

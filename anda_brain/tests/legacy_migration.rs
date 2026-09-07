use anda_cognitive_nexus::{CognitiveNexus, migrate::LegacyRow};
use anda_db::{
    collection::CollectionConfig,
    database::{AndaDB, DBConfig},
    schema::{Document, FieldEntry, FieldType, Fv, Schema, SchemaBuilder},
    storage::StorageConfig,
};
use anda_kip::{Operation, Request};
use object_store::ObjectStoreExt;
use object_store::memory::InMemory;
use serde_json::{Value, json};
use std::sync::Arc;

fn config() -> DBConfig {
    DBConfig {
        name: "release_probe".into(),
        description: "isolated review fixture".into(),
        storage: StorageConfig::default(),
        lock: None,
    }
}

fn schema(fields: &[(&str, FieldType)]) -> Schema {
    let mut b = SchemaBuilder::new();
    for (name, kind) in fields {
        b.add_field(FieldEntry::new((*name).into(), kind.clone()).unwrap())
            .unwrap();
    }
    b.build().unwrap()
}

async fn seed(types: &[(&str, &str, Value)], metadata: Value) -> Arc<InMemory> {
    let store = Arc::new(InMemory::new());
    let db = Arc::new(AndaDB::create(store.clone(), config()).await.unwrap());
    let concepts = db
        .open_or_create_collection(
            schema(&[
                ("type", FieldType::Text),
                ("name", FieldType::Text),
                ("attributes", FieldType::Json),
                ("metadata", FieldType::Json),
            ]),
            CollectionConfig {
                name: "concepts".into(),
                description: "v1".into(),
            },
            async |_| Ok(()),
        )
        .await
        .unwrap();
    for (t, name, attrs) in types {
        let mut doc = Document::new(concepts.schema());
        doc.set_field("_id", Fv::U64(0)).unwrap();
        doc.set_field("type", Fv::Text((*t).into())).unwrap();
        doc.set_field("name", Fv::Text((*name).into())).unwrap();
        doc.set_field("attributes", Fv::Json(attrs.clone()))
            .unwrap();
        doc.set_field("metadata", Fv::Json(json!({"author":"$self"})))
            .unwrap();
        concepts.add(doc).await.unwrap();
    }
    let propositions = db
        .open_or_create_collection(
            schema(&[
                ("subject", FieldType::Text),
                ("object", FieldType::Text),
                ("predicates", FieldType::Array(vec![FieldType::Text])),
                ("properties", FieldType::Json),
            ]),
            CollectionConfig {
                name: "propositions".into(),
                description: "v1".into(),
            },
            async |_| Ok(()),
        )
        .await
        .unwrap();
    if types.len() >= 2 {
        let mut doc = Document::new(propositions.schema());
        doc.set_field("_id", Fv::U64(0)).unwrap();
        doc.set_field("subject", Fv::Text("C:1".into())).unwrap();
        doc.set_field("object", Fv::Text("C:2".into())).unwrap();
        doc.set_field("predicates", Fv::Array(vec![Fv::Text("prefers".into())]))
            .unwrap();
        doc.set_field(
            "properties",
            Fv::Json(json!({"prefers":{"metadata":metadata,"attributes":{"reason":"old reason"}}})),
        )
        .unwrap();
        propositions.add(doc).await.unwrap();
    }
    db.close().await.unwrap();
    store
}

async fn open(store: Arc<InMemory>) -> Result<CognitiveNexus, String> {
    let db = Arc::new(
        AndaDB::open(store, config())
            .await
            .map_err(|e| e.to_string())?,
    );
    let nexus = CognitiveNexus::connect(db)
        .await
        .map_err(|e| e.to_string())?;
    nexus
        .install_and_activate(
            &[("brain", anda_cognitive_nexus::profiles::COGNITIVE_MEMORY)],
            anda_cognitive_nexus::nexus::DEFAULT_SPACE,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(nexus)
}

async fn read(nexus: &CognitiveNexus, command: &str) -> Value {
    let response = anda_kip::execute_request(
        nexus,
        &Request {
            operations: vec![Operation::new(command)],
            ..Default::default()
        },
    )
    .await;
    println!("{command}: {}", serde_json::to_string(&response).unwrap());
    assert_eq!(response.status, anda_kip::TopLevelStatus::Succeeded);
    response.first_result().cloned().unwrap()
}

#[tokio::test]
async fn legacy_meta_types_can_migrate() {
    let store = seed(
        &[("$ConceptType", "Person", json!({"description":"a human"}))],
        json!({}),
    )
    .await;
    let result = open(store).await;
    assert!(
        result.is_ok(),
        "legacy meta-type migration failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn legacy_event_can_migrate() {
    let store = seed(&[("Event", "chat-42", json!({"summary":"old event", "start_time":"2026-01-01T00:00:00Z", "status":"unsorted"}))], json!({})).await;
    let result = open(store).await;
    assert!(
        result.is_ok(),
        "legacy Event migration failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn legacy_sleep_task_can_migrate() {
    let store = seed(&[("SleepTask", "review-old-event", json!({"target_type":"Event", "target_name":"old-event", "requested_action":"consolidate_to_semantic", "reason":"Multiple preferences mentioned", "status":"pending", "priority":1}))], json!({})).await;
    let result = open(store).await;
    assert!(
        result.is_ok(),
        "legacy SleepTask migration failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn legacy_insight_can_migrate() {
    let store = seed(&[("Insight", "inspect-before-deploy", json!({"insight_class":"lesson_learned", "description":"Inspect before deploying", "correction":"run the checks"}))], json!({})).await;
    let result = open(store).await;
    assert!(
        result.is_ok(),
        "legacy Insight migration failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn superseded_and_expired_claim_does_not_revive() {
    let store = seed(&[("Person", "$self", json!({})), ("Topic", "dark_mode", json!({}))], json!({"author":"$self", "confidence":0.9, "superseded":true, "status":"retracted", "expires_at":"2020-01-01T00:00:00Z", "valid_until":"2020-01-01T00:00:00Z"})).await;
    let nexus = open(store).await.unwrap();
    let assertions = read(&nexus, "FIND(?a) WHERE { ?a ASSERTION {} } LIMIT 10").await;
    let beliefs = read(
        &nexus,
        "FIND(?b) WHERE { ?p PROPOSITION (?s, \"prefers\", ?o) ?b BELIEF (?p) } LIMIT 10",
    )
    .await;
    assert!(
        beliefs
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["status"] != "accepted"),
        "{beliefs}"
    );
    assert!(
        assertions.to_string().contains("retracted")
            || assertions.to_string().contains("superseded"),
        "all legacy lifecycle exclusions disappeared"
    );
}

#[tokio::test]
async fn resume_after_load_before_completion_marker() {
    let store = seed(
        &[
            ("Person", "$self", json!({})),
            ("Topic", "dark_mode", json!({})),
        ],
        json!({"confidence":0.9}),
    )
    .await;
    let nexus = open(store.clone()).await.unwrap();
    let staging = nexus
        .store
        .db
        .open_collection("kip_legacy_v1".into(), async |_| Ok(()))
        .await
        .unwrap();
    let frozen: Value = staging.get_extension_as("migration_plan_v1").unwrap();
    assert_eq!(
        frozen["package"]["manifest"]["package_ref"],
        "kip://legacy/nexus@1.1.0"
    );
    assert_eq!(
        frozen["vocabulary"]["adopted_types"]["Topic"],
        "kip://legacy/nexus@1.1.0/Topic"
    );
    for id in staging.ids() {
        let row: LegacyRow = staging.get_as(id).await.unwrap();
        if row.kind == "marker" {
            staging.remove(id).await.unwrap();
        }
    }
    nexus.close().await.unwrap();
    drop(nexus);
    let result = open(store).await;
    assert!(
        result.is_ok(),
        "resuming migration failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn resume_between_two_legacy_collection_deletes() {
    let store = seed(
        &[
            ("Person", "$self", json!({})),
            ("Topic", "dark_mode", json!({})),
        ],
        json!({}),
    )
    .await;
    let db = Arc::new(AndaDB::open(store.clone(), config()).await.unwrap());
    let staging = db
        .open_or_create_collection(
            LegacyRow::schema().unwrap(),
            CollectionConfig {
                name: "kip_legacy_v1".into(),
                description: "staging".into(),
            },
            async |_| Ok(()),
        )
        .await
        .unwrap();
    for (collection, kind) in [("concepts", "concept"), ("propositions", "proposition")] {
        let source = db
            .open_collection(collection.into(), async |_| Ok(()))
            .await
            .unwrap();
        for id in source.ids() {
            let doc: Value = source.get_as(id).await.unwrap();
            staging
                .add_from(&LegacyRow {
                    _id: 0,
                    kind: kind.into(),
                    legacy_id: id,
                    doc,
                })
                .await
                .unwrap();
        }
    }
    db.flush().await.unwrap();
    db.delete_collection("concepts").await.unwrap();
    db.close().await.unwrap();
    drop(db);
    let result = open(store).await;
    assert!(
        result.is_ok(),
        "resuming between deletes failed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn published_v011_object_store_upgrades_and_reopens() {
    #[derive(serde::Deserialize)]
    struct StoredObject {
        path: String,
        bytes: ic_auth_types::ByteBufB64,
    }
    let snapshot: Vec<StoredObject> =
        cbor2::from_reader(include_bytes!("fixtures/published_v0_11.cbor").as_slice()).unwrap();
    let store = Arc::new(InMemory::new());
    for object in snapshot {
        store
            .put(
                &object_store::path::Path::from(object.path),
                object.bytes.0.into(),
            )
            .await
            .unwrap();
    }
    let config = DBConfig {
        name: "published_v011_fixture".into(),
        ..Default::default()
    };
    let db = Arc::new(AndaDB::open(store.clone(), config.clone()).await.unwrap());
    let nexus = CognitiveNexus::connect(db).await.unwrap();
    nexus
        .install_and_activate(
            &[("brain", anda_cognitive_nexus::profiles::COGNITIVE_MEMORY)],
            anda_cognitive_nexus::nexus::DEFAULT_SPACE,
        )
        .await
        .unwrap();
    assert_eq!(read(&nexus, r#"FIND(?c.attributes.summary, ?c.attributes.task_class, ?c.attributes.status) WHERE { ?c CONCEPT {type: "SleepTask", key: "fixture_task"} }"#).await,
        json!([["Review the preference change","consolidate","pending"]]));
    assert_eq!(read(&nexus, r#"FIND(?c.attributes.summary) WHERE { ?c CONCEPT {type: "Insight", key: "fixture_insight"} }"#).await,
        json!(["Verify changes before deploying"]));
    assert_eq!(read(&nexus, r#"FIND(?c.facets["MnemonicState"].memory_strength, ?c.retention.retention_class) WHERE { ?c CONCEPT {type: "Preference", key: "fixture_pinned"} }"#).await,
        json!([[0.8,"pinned"]]));
    for (key, expected) in [("fixture_dark", false), ("fixture_light", true)] {
        let belief = read(&nexus, &format!(r#"FIND(?b.status) WHERE {{ ?s CONCEPT {{type: "Person", key: "fixture_alice"}} ?o CONCEPT {{type: "Preference", key: "{key}"}} ?p PROPOSITION (?s, "prefers", ?o) ?b BELIEF (?p) }}"#)).await;
        assert_eq!(belief[0] == "accepted", expected, "{key}: {belief}");
    }
    let counts = (
        nexus.store.concepts().len(),
        nexus.store.propositions().len(),
        nexus.store.assertions().len(),
    );
    nexus.close().await.unwrap();
    drop(nexus);
    let db = Arc::new(AndaDB::open(store, config).await.unwrap());
    let nexus = CognitiveNexus::connect(db).await.unwrap();
    nexus
        .install_and_activate(
            &[("brain", anda_cognitive_nexus::profiles::COGNITIVE_MEMORY)],
            anda_cognitive_nexus::nexus::DEFAULT_SPACE,
        )
        .await
        .unwrap();
    assert_eq!(
        (
            nexus.store.concepts().len(),
            nexus.store.propositions().len(),
            nexus.store.assertions().len()
        ),
        counts
    );
    nexus.close().await.unwrap();
}

#[tokio::test]
async fn incomplete_staging_never_discards_a_remaining_source_collection() {
    let store = seed(
        &[
            ("Person", "$self", json!({})),
            ("Preference", "dark", json!({})),
        ],
        json!({}),
    )
    .await;
    let db = Arc::new(AndaDB::open(store.clone(), config()).await.unwrap());
    let source = db
        .open_collection("concepts".into(), async |_| Ok(()))
        .await
        .unwrap();
    let staging = db
        .open_or_create_collection(
            LegacyRow::schema().unwrap(),
            CollectionConfig {
                name: "kip_legacy_v1".into(),
                description: "incomplete recovery".into(),
            },
            async |_| Ok(()),
        )
        .await
        .unwrap();
    for id in source.ids() {
        staging
            .add_from(&LegacyRow {
                _id: 0,
                kind: "concept".into(),
                legacy_id: id,
                doc: source.get_as(id).await.unwrap(),
            })
            .await
            .unwrap();
    }
    db.flush().await.unwrap();
    db.delete_collection("concepts").await.unwrap();
    db.close().await.unwrap();
    drop(staging);
    drop(source);
    drop(db);
    let failure = open(store.clone()).await.err().unwrap();
    assert!(
        failure.contains("refusing to drop propositions"),
        "{failure}"
    );
    let db = AndaDB::open(store, config()).await.unwrap();
    assert!(db.metadata().collections.contains("propositions"));
    assert_eq!(
        db.open_collection("propositions".into(), async |_| Ok(()))
            .await
            .unwrap()
            .len(),
        1
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn legacy_runtime_and_learning_state_cannot_acquire_native_standing() {
    let store = seed(
        &[
            (
                "Skill",
                "same",
                json!({"procedure":"do a thing", "status":"adopted"}),
            ),
            (
                "LegacySkill",
                "same",
                json!({"description":"unrelated custom type"}),
            ),
        ],
        json!({}),
    )
    .await;
    let nexus = open(store).await.unwrap();
    let refs = read(
        &nexus,
        r#"FIND(?c.schema_ref) WHERE { ?c CONCEPT {name: "same"} } ORDER BY ?c.schema_ref"#,
    )
    .await;
    assert_eq!(
        refs,
        json!([
            "kip://legacy/nexus@1.1.0/LegacySkill",
            "kip://legacy/nexus@1.1.0/LegacySkill2"
        ])
    );
}

#[tokio::test]
async fn old_optional_values_and_interrupted_tasks_remain_auditable() {
    let store = seed(
        &[
            (
                "Preference",
                "strength",
                json!({"strength":2.0,"legacy":{"user":"kept"}}),
            ),
            (
                "SleepTask",
                "interrupted",
                json!({"reason":"review","requested_action":"review","status":"in_progress"}),
            ),
        ],
        json!({}),
    )
    .await;
    let nexus = open(store).await.unwrap();
    assert_eq!(read(&nexus, r#"FIND(?c.attributes.strength, ?c.facets["LegacyRecord"].record.attributes.strength, ?c.attributes.legacy.user) WHERE { ?c CONCEPT {key: "strength"} }"#).await,
        json!([[null,2.0,"kept"]]));
    assert_eq!(read(&nexus, r#"FIND(?c.attributes.status, ?c.facets["LegacyRecord"].record.attributes.status) WHERE { ?c CONCEPT {key: "interrupted"} }"#).await,
        json!([["blocked","in_progress"]]));
}

#[tokio::test]
async fn revision_order_does_not_follow_legacy_row_ids_and_survives_retry() {
    let store = seed(
        &[
            ("Person", "$self", json!({})),
            ("Preference", "dark", json!({})),
            ("Preference", "light", json!({})),
        ],
        json!({"author":"$self","status":"retracted"}),
    )
    .await;
    let db = Arc::new(AndaDB::open(store.clone(), config()).await.unwrap());
    let c = db
        .open_collection("propositions".into(), async |_| Ok(()))
        .await
        .unwrap();
    let mut doc = Document::new(c.schema());
    for (name, value) in [
        ("_id", Fv::U64(0)),
        ("subject", Fv::Text("C:1".into())),
        // A recovery source may retain two records for the same canonical
        // tuple. The older row id here is the newer, withdrawn assertion.
        ("object", Fv::Text("C:2".into())),
        ("predicates", Fv::Array(vec![Fv::Text("prefers".into())])),
        (
            "properties",
            Fv::Json(
                json!({"prefers":{"a":{},"m":{"author":"$self","superseded":true,"superseded_by":"P:1:prefers"}}}),
            ),
        ),
    ] {
        doc.set_field(name, value).unwrap();
    }
    c.add(doc).await.unwrap();
    db.close().await.unwrap();
    drop(c);
    drop(db);
    let nexus = open(store.clone()).await.unwrap();
    let statuses = read(
        &nexus,
        "FIND(?a.lifecycle.status) WHERE { ?a ASSERTION {} } ORDER BY ?a.id",
    )
    .await;
    assert_eq!(statuses, json!(["retracted", "superseded"]));
    let staging = nexus
        .store
        .db
        .open_collection("kip_legacy_v1".into(), async |_| Ok(()))
        .await
        .unwrap();
    for id in staging.ids() {
        let row: LegacyRow = staging.get_as(id).await.unwrap();
        if row.kind == "marker" {
            staging.remove(id).await.unwrap();
        }
    }
    nexus.close().await.unwrap();
    drop(staging);
    drop(nexus);
    let nexus = open(store).await.unwrap();
    assert_eq!(
        read(
            &nexus,
            "FIND(?a.lifecycle.status) WHERE { ?a ASSERTION {} } ORDER BY ?a.id"
        )
        .await,
        statuses
    );
}

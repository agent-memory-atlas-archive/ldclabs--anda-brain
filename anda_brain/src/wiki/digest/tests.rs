use super::*;

mod regressions;

async fn review_seed_wiki_fact(
    wiki: &WikiService,
    digest: &WikiDigest,
    title: &str,
) -> (WikiDocRecord, WikiVersionRecord, DigestedFact) {
    let created = wiki
        .commit("a".into(), commit_input(title, "A shared fact.\n"), 1000)
        .await
        .unwrap();
    let doc = wiki.doc_record(created.doc.id).await.unwrap();
    let version = wiki
        .versions
        .get_as::<WikiVersionRecord>(created.version.id)
        .await
        .unwrap();
    let mut item = fact(("Person", "alice"), "prefers", ("Preference", "dark_mode"));
    item.citation = citation_uri("test_space", doc._id, version._id, 0, version.size);
    digest
        .ensure_vocabulary(std::slice::from_ref(&item))
        .await
        .unwrap();
    let request = digest_request(
        &doc,
        &version,
        &Extraction::default(),
        std::slice::from_ref(&item),
        "review",
        1500,
    );
    let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
    assert!(
        kip::succeeded(&response),
        "seed failed: {}",
        kip::error_message(&response)
    );
    wiki.write_event(
        EVENT_DIGEST_EXTRACTED,
        Some(doc._id),
        Some(version._id),
        "wiki_digest".into(),
        BTreeMap::from([("facts".into(), json!([item.clone()]))]),
        1500,
    )
    .await
    .unwrap();
    (doc, version, item)
}

#[tokio::test]
async fn review_wiki_retry_keeps_idempotency() {
    let (wiki, digest) = test_digest("review_wiki_retry").await;
    let (doc, version, item) = review_seed_wiki_fact(&wiki, &digest, "retry doc").await;
    let request = digest_request(
        &doc,
        &version,
        &Extraction::default(),
        &[item],
        "review",
        2500,
    );
    let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
    assert!(
        kip::succeeded(&response),
        "retry failed: {}",
        kip::error_message(&response)
    );
}

#[tokio::test]
async fn review_wiki_retraction_is_document_scoped() {
    let (wiki, digest) = test_digest("review_wiki_scope").await;
    let (first, _, item) = review_seed_wiki_fact(&wiki, &digest, "first doc").await;
    review_seed_wiki_fact(&wiki, &digest, "second doc").await;
    assert_eq!(
        digest_claim_status(&digest, &item).await,
        ["active", "active"]
    );
    let count = digest
        .retract_facts(first._id, std::slice::from_ref(&item), &BTreeSet::new())
        .await
        .unwrap();
    let statuses = digest_claim_status(&digest, &item).await;
    println!("one document retracted: count={count}; statuses={statuses:?}");
    assert_eq!(count, 1);
    assert_eq!(
        statuses.iter().filter(|s| s.as_str() == "active").count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| s.as_str() == "retracted")
            .count(),
        1
    );
}

#[tokio::test]
async fn review_wiki_fact_can_return_in_a_later_version() {
    let (wiki, digest) = test_digest("review_wiki_return").await;
    let (doc, previous, mut item) = review_seed_wiki_fact(&wiki, &digest, "return doc").await;
    digest
        .retract_facts(doc._id, std::slice::from_ref(&item), &BTreeSet::new())
        .await
        .unwrap();
    let mut update = commit_input("return doc", "The shared fact returns.\n");
    update.doc_id = Some(doc._id);
    update.parent_version = Some(previous._id);
    let created = wiki.commit("a".into(), update, 2000).await.unwrap();
    let version = wiki
        .versions
        .get_as::<WikiVersionRecord>(created.version.id)
        .await
        .unwrap();
    item.citation = citation_uri("test_space", doc._id, version._id, 0, version.size);
    let request = digest_request(
        &doc,
        &version,
        &Extraction::default(),
        std::slice::from_ref(&item),
        "review",
        2500,
    );
    let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
    assert!(
        kip::succeeded(&response),
        "returned fact failed: {}",
        kip::error_message(&response)
    );
    assert!(
        digest_claim_status(&digest, &item)
            .await
            .iter()
            .any(|s| s == "active")
    );
}

#[tokio::test]
async fn restoring_the_same_version_starts_a_new_assertion_generation() {
    let (wiki, digest) = test_digest("same_version_restore").await;
    let (doc, version, mut item) = review_seed_wiki_fact(&wiki, &digest, "restored doc").await;
    assert_eq!(
        digest.retract_digested(&doc, &version, 2000).await.unwrap(),
        1
    );
    assert_eq!(digest_claim_status(&digest, &item).await, ["retracted"]);
    let (generation, previous) = digest
        .previous_digest_head(doc._id, version._id + 1)
        .await
        .unwrap();
    assert!(previous.is_empty());
    assert!(generation > 0);
    item.assertion_generation = generation;
    let request = digest_request(
        &doc,
        &version,
        &Extraction::default(),
        std::slice::from_ref(&item),
        "review",
        2500,
    );
    for _ in 0..2 {
        let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
        assert!(
            kip::succeeded(&response),
            "{}",
            kip::error_message(&response)
        );
    }
    let statuses = digest_claim_status(&digest, &item).await;
    assert_eq!(
        statuses.iter().filter(|s| s.as_str() == "active").count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| s.as_str() == "retracted")
            .count(),
        1
    );
}

fn fact(s: (&str, &str), p: &str, o: (&str, &str)) -> DigestedFact {
    DigestedFact {
        subject_type: s.0.to_string(),
        subject_name: s.1.to_string(),
        predicate: p.to_string(),
        object_type: o.0.to_string(),
        object_name: o.1.to_string(),
        confidence: 0.9,
        citation: "wiki://sp/1@2#0-10".to_string(),
        checksum: "sha3-256:x".to_string(),
        assertion_generation: 0,
    }
}

#[test]
fn parse_extraction_tolerates_fences_and_prose() {
    let strict = r#"{"facts": [{"subject": {"type": "A", "name": "a"}, "predicate": "p", "object": {"type": "B", "name": "b"}}]}"#;
    assert_eq!(parse_extraction(strict).unwrap().facts.len(), 1);

    let fenced = format!("Here you go:\n```json\n{strict}\n```\nDone.");
    assert_eq!(parse_extraction(&fenced).unwrap().facts.len(), 1);

    assert!(parse_extraction("no json here").is_err());
}

#[test]
fn clean_ident_rejects_reserved_and_oversized() {
    assert_eq!(clean_ident(" Person "), Some("Person".to_string()));
    assert!(clean_ident("$ConceptType").is_none());
    assert!(clean_ident("_hidden").is_none());
    assert!(clean_ident("").is_none());
    assert!(clean_ident(&"x".repeat(200)).is_none());
}

#[test]
fn clean_type_ident_normalizes_to_upper_camel_case() {
    assert_eq!(clean_type_ident("Drug"), Some("Drug".to_string()));
    assert_eq!(clean_type_ident("drug"), Some("Drug".to_string()));
    assert_eq!(
        clean_type_ident("medical device"),
        Some("MedicalDevice".to_string())
    );
    assert_eq!(
        clean_type_ident("clinical-trial"),
        Some("ClinicalTrial".to_string())
    );
    assert_eq!(clean_type_ident("works_at"), Some("WorksAt".to_string()));
    assert!(clean_type_ident("$ConceptType").is_none());
    assert!(clean_type_ident("3d printer").is_none());
    assert!(clean_type_ident("工作").is_none());
}

#[test]
fn clean_predicate_ident_normalizes_to_snake_case() {
    assert_eq!(
        clean_predicate_ident("works_at"),
        Some("works_at".to_string())
    );
    assert_eq!(
        clean_predicate_ident("worksAt"),
        Some("works_at".to_string())
    );
    assert_eq!(
        clean_predicate_ident("WorksAt"),
        Some("works_at".to_string())
    );
    assert_eq!(
        clean_predicate_ident("works at"),
        Some("works_at".to_string())
    );
    assert_eq!(clean_predicate_ident("treats"), Some("treats".to_string()));
    assert_eq!(
        clean_predicate_ident("has  side-effect"),
        Some("has_side_effect".to_string())
    );
    assert!(clean_predicate_ident("_hidden").is_none());
    assert!(clean_predicate_ident("···").is_none());
}

#[test]
fn render_digest_kml_registers_schema_and_attaches_provenance() {
    let doc = WikiDocRecord {
        generation: 1,
        digest_pending: 0,
        _id: 3,
        namespace: "kb".to_string(),
        slug: "policy".to_string(),
        title: "安全政策".to_string(),
        status: super::super::DOC_STATUS_ACTIVE.to_string(),
        current_version: 7,
        current_checksum: "sha3-256:doc".to_string(),
        tags: vec![],
        acl_label: String::new(),
        source_uri: None,
        metadata: BTreeMap::new(),
        created_by: "a".to_string(),
        updated_by: "a".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    let version = WikiVersionRecord {
        _id: 7,
        doc_id: 3,
        parent_version: None,
        checksum: "sha3-256:v".to_string(),
        content: "c".to_string(),
        size: 1,
        author: "a".to_string(),
        message: None,
        created_at: 0,
    };
    let facts = vec![fact(
        ("Organization", "Acme \"quoted\""),
        "publishes",
        ("Policy", "安全政策"),
    )];
    let request = digest_request(
        &doc,
        &version,
        &Extraction::default(),
        &facts,
        "wiki_digest@v1/test-model",
        1_700_000_000_000,
    );
    request.validate().unwrap();
    let command = request.operations[0].command.clone().unwrap();
    let parameters = request.parameters.clone().unwrap();

    // One transaction: Evidence, endpoints and the attributed claim commit
    // together or not at all.
    assert!(command.starts_with("MUTATE {"), "{command}");
    assert!(command.contains("CREATE EVIDENCE ?e0"), "{command}");
    assert!(command.contains("UPSERT CONCEPT ?c0"), "{command}");
    assert!(
        command.contains(r#"ASSERT ?p0 (?c0, :pred0, ?c1) { by: ?self, mode: "inferred""#),
        "{command}"
    );

    // Nothing extracted is spliced into the command text — a name carrying
    // a quote is data, and the parser never sees it as syntax.
    assert!(!command.contains("Acme"), "{command}");
    assert_eq!(parameters["cn0"], json!(r#"Acme "quoted""#));
    assert_eq!(parameters["ct0"], json!("Organization"));
    assert_eq!(parameters["pred0"], json!("publishes"));
    assert_eq!(parameters["conf0"], json!(0.9));

    // Provenance is Evidence, not metadata on the link.
    assert_eq!(
        parameters["epayload0"]["citation"],
        json!("wiki://sp/1@2#0-10")
    );
    assert_eq!(
        parameters["epayload0"]["extractor"],
        json!("wiki_digest@v1/test-model")
    );

    // Dropping a fact withdraws this reader's claim, scoped to `$self`.
    let retract = retract_request("A-1", 1);
    retract.validate().unwrap();
    let command = retract.operations[0].command.clone().unwrap();
    assert!(
        command.starts_with(r#"TRANSITION :id TO "retracted""#),
        "{command}"
    );
    assert!(command.contains("EXPECT VERSION :version"), "{command}");
    let candidates = claim_candidates_request(&facts[0], "");
    assert!(
        candidates.operations[0]
            .command
            .as_ref()
            .unwrap()
            .contains("asserted_by: ?self")
    );
}

#[test]
fn normalize_facts_resolves_anchors_and_dedupes() {
    let doc = WikiDocRecord {
        generation: 1,
        digest_pending: 0,
        _id: 1,
        namespace: "kb".to_string(),
        slug: "d".to_string(),
        title: "t".to_string(),
        status: super::super::DOC_STATUS_ACTIVE.to_string(),
        current_version: 2,
        current_checksum: String::new(),
        tags: vec![],
        acl_label: String::new(),
        source_uri: None,
        metadata: BTreeMap::new(),
        created_by: String::new(),
        updated_by: String::new(),
        created_at: 0,
        updated_at: 0,
    };
    let version = WikiVersionRecord {
        _id: 2,
        doc_id: 1,
        parent_version: None,
        checksum: "sha3-256:v".to_string(),
        content: "0123456789".to_string(),
        size: 10,
        author: String::new(),
        message: None,
        created_at: 0,
    };
    let chunk = WikiChunkRecord {
        _id: 5,
        doc_id: 1,
        version_id: 2,
        namespace: "kb".to_string(),
        current: 1,
        title: "t".to_string(),
        heading_path: vec![],
        anchor: "sec-0".to_string(),
        ordinal: 0,
        text: "01234".to_string(),
        byte_start: 0,
        byte_end: 5,
        checksum: "sha3-256:chunk".to_string(),
        chunker_version: 1,
        acl_label: String::new(),
    };
    let extraction = Extraction {
        reviews: Vec::new(),
        concepts: vec![],
        facts: vec![
            ExtractedFact {
                subject: ConceptRef {
                    r#type: "A".into(),
                    name: "a".into(),
                },
                predicate: "p".into(),
                object: ConceptRef {
                    r#type: "B".into(),
                    name: "b".into(),
                },
                confidence: Some(2.0),
                anchor: Some("sec-0".into()),
            },
            // Duplicate triple: dropped.
            ExtractedFact {
                subject: ConceptRef {
                    r#type: "A".into(),
                    name: "a".into(),
                },
                predicate: "p".into(),
                object: ConceptRef {
                    r#type: "B".into(),
                    name: "b".into(),
                },
                confidence: None,
                anchor: None,
            },
            // Unknown anchor: cites the whole version.
            ExtractedFact {
                subject: ConceptRef {
                    r#type: "A".into(),
                    name: "a".into(),
                },
                predicate: "q".into(),
                object: ConceptRef {
                    r#type: "B".into(),
                    name: "b".into(),
                },
                confidence: None,
                anchor: Some("missing".into()),
            },
            // Reserved type: dropped.
            ExtractedFact {
                subject: ConceptRef {
                    r#type: "$Evil".into(),
                    name: "x".into(),
                },
                predicate: "p".into(),
                object: ConceptRef {
                    r#type: "B".into(),
                    name: "b".into(),
                },
                confidence: None,
                anchor: None,
            },
        ],
    };

    let (facts, alive) = normalize_facts("sp", &doc, &version, &[chunk], &extraction);
    assert_eq!(facts.len(), 2);
    assert_eq!(alive.len(), 2);
    assert_eq!(facts[0].confidence, 1.0);
    assert_eq!(facts[0].citation, "wiki://sp/1@2#0-5");
    assert_eq!(facts[0].checksum, "sha3-256:chunk");
    assert_eq!(facts[1].citation, "wiki://sp/1@2#0-10");
}

#[test]
fn alive_set_is_not_capped_by_fact_truncation() {
    let doc = WikiDocRecord {
        generation: 1,
        digest_pending: 0,
        _id: 1,
        namespace: "kb".to_string(),
        slug: "d".to_string(),
        title: "t".to_string(),
        status: super::super::DOC_STATUS_ACTIVE.to_string(),
        current_version: 2,
        current_checksum: String::new(),
        tags: vec![],
        acl_label: String::new(),
        source_uri: None,
        metadata: BTreeMap::new(),
        created_by: String::new(),
        updated_by: String::new(),
        created_at: 0,
        updated_at: 0,
    };
    let version = WikiVersionRecord {
        _id: 2,
        doc_id: 1,
        parent_version: None,
        checksum: "sha3-256:v".to_string(),
        content: "x".to_string(),
        size: 1,
        author: String::new(),
        message: None,
        created_at: 0,
    };
    let extraction = Extraction {
        reviews: Vec::new(),
        concepts: vec![],
        facts: (0..MAX_FACTS_PER_VERSION + 10)
            .map(|i| ExtractedFact {
                subject: ConceptRef {
                    r#type: "A".into(),
                    name: format!("a{i}"),
                },
                predicate: "p".into(),
                object: ConceptRef {
                    r#type: "B".into(),
                    name: "b".into(),
                },
                confidence: None,
                anchor: None,
            })
            .collect(),
    };
    let (facts, alive) = normalize_facts("sp", &doc, &version, &[], &extraction);
    // Persisted facts are capped, but the alive set keeps every valid
    // triple so superseding never treats truncated facts as stale.
    assert_eq!(facts.len(), MAX_FACTS_PER_VERSION);
    assert_eq!(alive.len(), MAX_FACTS_PER_VERSION + 10);
    let truncated = &extraction.facts[MAX_FACTS_PER_VERSION + 5];
    let key = (
        "A".to_string(),
        truncated.subject.name.clone(),
        "p".to_string(),
        "B".to_string(),
        "b".to_string(),
    );
    assert!(alive.contains(&key));
}

use super::super::tests::{commit_input, test_wiki};

#[tokio::test]
async fn extraction_source_survives_index_replacement() {
    let wiki = test_wiki("wiki_digest_race").await;
    let v1 = wiki
        .commit(
            "a".to_string(),
            commit_input("竞态", "# 竞态\n\n第一版内容。\n"),
            1000,
        )
        .await
        .unwrap();
    let v1_record = wiki
        .versions
        .get_as::<WikiVersionRecord>(v1.version.id)
        .await
        .unwrap();

    // A concurrent commit lands v2 and removes v1's chunk set — exactly
    // what a digest can observe between its doc check and chunk read.
    let mut update = commit_input("竞态", "# 竞态\n\n第二版内容。\n");
    update.doc_id = Some(v1.doc.id);
    update.parent_version = Some(v1.version.id);
    let v2 = wiki.commit("a".to_string(), update, 2000).await.unwrap();

    // v1 resolves to "raced away" (None), never to an empty chunk list
    // that would supersede the previous digest's facts wholesale.
    let doc = wiki.doc_record(v1.doc.id).await.unwrap();
    let original = source_chunks(&doc, &v1_record).unwrap();
    assert_eq!(
        original
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        v1_record.content
    );
    let v2_record = wiki
        .versions
        .get_as::<WikiVersionRecord>(v2.version.id)
        .await
        .unwrap();
    let chunks = source_chunks(&doc, &v2_record).unwrap();
    assert!(!chunks.is_empty());
}

#[tokio::test]
async fn restore_queues_doc_for_digest_catchup() {
    let wiki = test_wiki("wiki_digest_restore_pending").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("归档文档", "# 归档文档\n\n内容。\n"),
            1000,
        )
        .await
        .unwrap();
    wiki.archive("a".to_string(), out.doc.id, 2000)
        .await
        .unwrap();
    wiki.restore("a".to_string(), out.doc.id, 3000)
        .await
        .unwrap();
    let current = wiki.doc_record(out.doc.id).await.unwrap();
    assert_eq!(current.digest_pending, 1);
    assert_eq!(current.generation, 3);

    // The ledger check driving the catch-up: false until a
    // DigestExtracted event covers the document's current version.
    assert!(
        !version_digested(&wiki, out.doc.id, out.doc.current_version)
            .await
            .unwrap()
    );
    wiki.write_event(
        EVENT_DIGEST_EXTRACTED,
        Some(out.doc.id),
        Some(out.doc.current_version),
        "wiki_digest".to_string(),
        BTreeMap::new(),
        4000,
    )
    .await
    .unwrap();
    assert!(
        version_digested(&wiki, out.doc.id, out.doc.current_version)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn verify_recent_checks_ledger_citations_with_grouped_loads() {
    let wiki = test_wiki("wiki_digest_verify_recent").await;
    let out = wiki
        .commit(
            "a".to_string(),
            commit_input("样本", "# 样本\n\n引用样本内容。\n"),
            1000,
        )
        .await
        .unwrap();
    let version = wiki
        .versions
        .get_as::<WikiVersionRecord>(out.version.id)
        .await
        .unwrap();
    let ok = fact(("A", "a"), "p", ("B", "b"));
    let ok = DigestedFact {
        citation: citation_uri("test_space", out.doc.id, out.version.id, 0, version.size),
        checksum: chunk_checksum(
            &version.checksum,
            0,
            version.size as usize,
            &version.content,
        ),
        ..ok
    };
    let mut stale = ok.clone();
    stale.predicate = "q".into();
    stale.checksum = "sha3-256:wrong".to_string();
    wiki.write_event(
        EVENT_DIGEST_EXTRACTED,
        Some(out.doc.id),
        Some(out.version.id),
        "wiki_digest".to_string(),
        BTreeMap::from([(
            "facts".to_string(),
            serde_json::to_value(vec![ok, stale]).unwrap(),
        )]),
        2000,
    )
    .await
    .unwrap();

    let (checked, invalid) = verify_recent_citations(&wiki, 3000).await.unwrap();
    assert_eq!((checked, invalid), (2, 1));
    // The mismatched recorded checksum sits over intact content: a
    // reference error, not corruption — no audit event.
    let events = wiki
        .list_events(
            Some("CitationVerifyFailed".to_string()),
            None,
            None,
            Some(10),
        )
        .await
        .unwrap();
    assert!(events.events.is_empty());
}

use anda_cognitive_nexus::CognitiveNexus;
use anda_db::{database::AndaDB, database::DBConfig, storage::StorageConfig};
use object_store::memory::InMemory;

/// A digest engine over an in-memory space with a live Cognitive Nexus
/// (no LLM: only the non-extracting paths may run).
async fn test_digest(name: &str) -> (Arc<WikiService>, WikiDigest) {
    let db = Arc::new(
        AndaDB::create(
            Arc::new(InMemory::new()),
            DBConfig {
                name: name.to_string(),
                description: "wiki digest test db".to_string(),
                storage: StorageConfig::default(),
                lock: None,
            },
        )
        .await
        .unwrap(),
    );
    let nexus = CognitiveNexus::connect(db.clone()).await.unwrap();
    // A Space that has activated nothing resolves Core alone, and Core
    // declares no Concept types at all.
    nexus
        .install_and_activate(
            &[(
                "anda_brain",
                anda_cognitive_nexus::profiles::COGNITIVE_MEMORY,
            )],
            anda_cognitive_nexus::nexus::DEFAULT_SPACE,
        )
        .await
        .unwrap();
    let nexus = Arc::new(nexus);
    let memory = Arc::new(MemoryManagement::connect(db.clone(), nexus).await.unwrap());
    let wiki = Arc::new(
        WikiService::connect("test_space".to_string(), db)
            .await
            .unwrap(),
    );
    let digest = WikiDigest::new(wiki.clone(), memory, Arc::new(Models::default()));
    (wiki, digest)
}

/// The lifecycle status of the digest's own Assertion about one fact.
async fn digest_claim_status(digest: &WikiDigest, fact: &DigestedFact) -> Vec<String> {
    let request = kip::request_with(
        r#"FIND(?a.lifecycle.status) WHERE {
  ?s CONCEPT {type: :st, key: :sn}
  ?o CONCEPT {type: :ot, key: :on}
  ?p (?s, :pred, ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
        serde_json::Map::from_iter([
            ("st".to_string(), json!(fact.subject_type)),
            ("sn".to_string(), json!(fact.subject_name)),
            ("pred".to_string(), json!(fact.predicate)),
            ("ot".to_string(), json!(fact.object_type)),
            ("on".to_string(), json!(fact.object_name)),
        ]),
    );
    let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
    serde_json::from_value(kip::ok_result(&response).cloned().unwrap_or_default())
        .unwrap_or_default()
}

#[tokio::test]
async fn labeling_a_document_retracts_its_digested_facts() {
    let (wiki, digest) = test_digest("wiki_digest_retract").await;
    let v1 = wiki
        .commit(
            "a".to_string(),
            commit_input("秘密文档", "# 秘密文档\n\n内容甲。\n"),
            1000,
        )
        .await
        .unwrap();
    let doc = wiki.doc_record(v1.doc.id).await.unwrap();
    let v1_version = wiki
        .versions
        .get_as::<WikiVersionRecord>(v1.version.id)
        .await
        .unwrap();

    // Seed the graph and the ledger as if v1 had been digested.
    let fact = DigestedFact {
        subject_type: "Person".to_string(),
        subject_name: "alice".to_string(),
        predicate: "knows".to_string(),
        object_type: "Topic".to_string(),
        object_name: "secret_topic".to_string(),
        confidence: 0.9,
        citation: citation_uri("test_space", doc._id, v1_version._id, 0, v1_version.size),
        checksum: "sha3-256:x".to_string(),
        assertion_generation: 0,
    };
    // The Space must declare the extracted symbols before a write can name
    // one; that is the host's decision in KIP 2.0, not the write's.
    digest
        .ensure_vocabulary(std::slice::from_ref(&fact))
        .await
        .unwrap();
    let request = digest_request(
        &doc,
        &v1_version,
        &Extraction::default(),
        std::slice::from_ref(&fact),
        "wiki_digest@v1/test",
        1500,
    );
    let response = anda_kip::execute_request(digest.memory.nexus().as_ref(), &request).await;
    assert!(
        kip::succeeded(&response),
        "seed failed: {}",
        kip::error_message(&response)
    );
    wiki.write_event(
        EVENT_DIGEST_EXTRACTED,
        Some(doc._id),
        Some(v1_version._id),
        "wiki_digest".to_string(),
        BTreeMap::from([(
            "facts".to_string(),
            serde_json::to_value(vec![fact.clone()]).unwrap(),
        )]),
        1500,
    )
    .await
    .unwrap();
    assert_eq!(digest_claim_status(&digest, &fact).await, ["active"]);

    // v2 labels the document — the state the digest refuses to distill.
    let mut update = commit_input("秘密文档", "# 秘密文档\n\n内容乙。\n");
    update.doc_id = Some(doc._id);
    update.parent_version = Some(v1_version._id);
    update.acl_label = Some("secret".to_string());
    let v2 = wiki.commit("a".to_string(), update, 2000).await.unwrap();
    let doc = wiki.doc_record(v2.doc.id).await.unwrap();
    assert_eq!(doc.acl_label, "secret");
    let v2_version = wiki
        .versions
        .get_as::<WikiVersionRecord>(v2.version.id)
        .await
        .unwrap();

    let retracted = digest
        .retract_digested(&doc, &v2_version, 3000)
        .await
        .unwrap();
    assert_eq!(retracted, 1);
    // The digest withdrew its own claim; the Proposition survives…
    assert_eq!(digest_claim_status(&digest, &fact).await, ["retracted"]);
    // …the digest head is a clean, retracted slate…
    let head = digest
        .previous_digest_facts(doc._id, v2_version._id + 1)
        .await
        .unwrap();
    assert!(head.is_empty());
    // …and the retraction marker does NOT count as "digested", so a
    // restore/unlabel catch-up would re-digest the version.
    assert!(
        !version_digested(&wiki, doc._id, v2_version._id)
            .await
            .unwrap()
    );

    // Idempotent: a second retraction is a no-op with no ledger growth.
    let events_before = wiki
        .list_events(
            Some(EVENT_DIGEST_EXTRACTED.to_string()),
            Some(doc._id),
            None,
            Some(20),
        )
        .await
        .unwrap()
        .events
        .len();
    assert_eq!(
        digest
            .retract_digested(&doc, &v2_version, 4000)
            .await
            .unwrap(),
        0
    );
    let events_after = wiki
        .list_events(
            Some(EVENT_DIGEST_EXTRACTED.to_string()),
            Some(doc._id),
            None,
            Some(20),
        )
        .await
        .unwrap()
        .events
        .len();
    assert_eq!(events_before, events_after);
}

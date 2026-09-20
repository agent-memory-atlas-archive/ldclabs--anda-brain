use super::*;
use crate::types::MemoryForgetInput;

#[tokio::test]
async fn forget_erases_evidence_and_activity_payloads_and_respects_legal_holds() {
    let app = test_app_state("forget_provenance");
    let space = create_loaded_space(&app, "forget_provenance").await;
    let seed = space.memory.execute(r#"MUTATE {
        CREATE EVIDENCE ?e { SET FIELDS {evidence_class: "user_statement", payload: "private original message"} }
        CREATE EVIDENCE ?held { SET FIELDS {evidence_class: "user_statement", payload: "held original message"} }
        CREATE ACTIVITY ?x { SET FIELDS {activity_class: "private_extraction_summary", status: "completed"} }
    }"#, None).await.unwrap();
    let evidence = seed["handles"]["e"].as_str().unwrap().to_string();
    let activity = seed["handles"]["x"].as_str().unwrap().to_string();
    let held = seed["handles"]["held"].as_str().unwrap().to_string();
    space
        .memory
        .execute(
            "SET RETENTION :id {legal_hold: true}",
            Some(kip::param("id", held.clone())),
        )
        .await
        .unwrap();

    let dry = space
        .forget_memory(MemoryForgetInput {
            entities: vec![evidence.clone(), activity.clone()],
            dry_run: true,
        })
        .await
        .unwrap();
    assert!(
        dry.entities.iter().all(|e| e.existed && e.error.is_none()),
        "{dry:?}"
    );
    assert_eq!(dry.deleted_evidence + dry.deleted_activities, 0);
    assert!(
        space
            .memory
            .query(
                "FIND(?e) WHERE {?e EVIDENCE {id: :id}}",
                Some(kip::param("id", evidence.clone()))
            )
            .await
            .unwrap()
            .to_string()
            .contains("private original message")
    );

    let report = space
        .forget_memory(MemoryForgetInput {
            entities: vec![evidence.clone(), activity.clone(), held.clone()],
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.deleted_evidence, 1, "{report:?}");
    assert_eq!(report.deleted_activities, 1, "{report:?}");
    assert!(
        report.entities[..2]
            .iter()
            .all(|e| e.existed && e.error.is_none())
    );
    assert!(report.entities[2].existed && report.entities[2].error.is_some());
    for (id, kind, original) in [
        (&evidence, "EVIDENCE", "private original message"),
        (&activity, "ACTIVITY", "private_extraction_summary"),
    ] {
        let row = space
            .memory
            .query(
                &format!("FIND(?e) WHERE {{?e {kind} {{id: :id}}}}"),
                Some(kip::param("id", id.clone())),
            )
            .await
            .unwrap();
        assert!(!row.to_string().contains(original), "{row}");
    }
    let held_row = space
        .memory
        .query(
            "FIND(?e) WHERE {?e EVIDENCE {id: :id}}",
            Some(kip::param("id", held)),
        )
        .await
        .unwrap();
    assert!(held_row.to_string().contains("held original message"));
    assert_eq!(space.memory_status().await.metrics.forgotten_entities, 2);
    let replay = space
        .forget_memory(MemoryForgetInput {
            entities: vec![evidence, activity],
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(replay.deleted_evidence + replay.deleted_activities, 0);
    space.close().await.unwrap();
}

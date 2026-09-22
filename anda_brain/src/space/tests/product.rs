use super::*;

#[tokio::test]
async fn product_record_sources_use_host_evidence_bindings_and_preserve_claim_semantics() {
    let app = test_app_state("product_sources");
    let space = create_loaded_space(&app, "product_sources").await;
    let messages = vec![Message {
        role: "user".into(),
        content: vec!["Prefer concise release notes".to_string().into()],
        ..Default::default()
    }];
    let mut request = kip::request(
        r#"MUTATE {
        CREATE CONCEPT ?owner { TYPE "Person" NAME "Owner" SET FIELDS {key:"owner-key"} }
        CREATE CONCEPT ?value { TYPE "Preference" NAME "Concise release notes" }
        ASSERT ?claim (?owner, "prefers", ?value) { by: ?owner, mode: "stated", confidence: 0.9, evidence: :msg1 }
    }"#,
    );
    request.ingest = kip::observation_ingest(
        &messages,
        "2026-09-22T00:00:00.000Z",
        "formation:conversation:42",
        None,
    );
    seed_kip(&space, request).await;
    let page = space.product_records(None, 20).await.unwrap();
    assert_eq!(page.records.len(), 1);
    let record = &page.records[0];
    assert_eq!(record.stance, "support");
    assert_eq!(record.status, "active");
    assert_eq!(record.actor_key.as_deref(), Some("owner-key"));
    assert_eq!(record.subject_label, "Owner");
    assert_eq!(record.object_label, "Concise release notes");
    assert!(record.sources_complete);
    assert_eq!(record.sources[0].formation_conversation, Some(42));
    assert_eq!(record.sources[0].message_index, Some(0));
    assert_eq!(
        record.sources[0].payload_digest,
        Some(
            anda_cognitive_nexus::content_digest(&serde_json::to_value(&messages[0]).unwrap())
                .unwrap()
        )
    );
    assert!(space.product_record("C-1").await.is_err());
    assert!(space.product_records(None, 51).await.is_err());
    space.close().await.unwrap();
}

async fn managed_record(
    space: &std::sync::Arc<Space>,
) -> (crate::product::MemoryRecord, crate::product::SourceIdentity) {
    let caller = anda_core::Principal::management_canister();
    let source = crate::product::SourceIdentity {
        key: "fixture-conversation".into(),
        parents: vec!["fixture-session".into()],
    };
    let messages = vec![Message {
        role: "user".into(),
        content: vec!["Prefer concise release notes".to_string().into()],
        ..Default::default()
    }];
    let input = crate::types::FormationInput {
        messages: messages.clone(),
        context: None,
        timestamp: None,
    };
    let conversation = anda_engine::memory::Conversation {
        user: SELF_USER_ID,
        status: anda_engine::memory::ConversationStatus::Completed,
        label: Some("formation".into()),
        messages: vec![serde_json::json!(Message {
            role: "user".into(),
            content: vec![serde_json::to_string(&input).unwrap().into()],
            ..Default::default()
        })],
        extra: Some(serde_json::json!({"memory_product_source":source})),
        ..Default::default()
    };
    let id = space
        .memory
        .add_conversation(anda_engine::memory::ConversationRef::from(&conversation))
        .await
        .unwrap();
    let mut request = kip::request_with(
        r#"MUTATE {
        CREATE CONCEPT ?owner { TYPE "Person" NAME "Owner" SET FIELDS {key: :owner} }
        CREATE CONCEPT ?value { TYPE "Preference" NAME "Concise release notes" }
        ASSERT ?claim (?owner, "prefers", ?value) { by: ?owner, mode: "stated", confidence: 0.9, evidence: :msg1 }
    }"#,
        kip::param("owner", caller.to_string()),
    );
    request.ingest = kip::observation_ingest(
        &messages,
        "2026-09-22T00:00:00.000Z",
        &format!("formation:conversation:{id}"),
        None,
    );
    seed_kip(space, request).await;
    (
        space
            .product_records(None, 20)
            .await
            .unwrap()
            .records
            .remove(0),
        source,
    )
}

#[tokio::test]
async fn product_correction_preserves_old_claim_and_rejects_stale_or_changed_intents() {
    let app = test_app_state("product_correct");
    let space = create_loaded_space(&app, "product_correct").await;
    let caller = anda_core::Principal::management_canister();
    let (record, source) = managed_record(&space).await;
    let input = crate::product::ChangeInput {
        operation_id: "correction-1".into(),
        record_id: record.id.clone(),
        expected_revision: record.revision,
        kind: crate::product::ChangeKind::Correct,
        new_value: Some("Risk section first".into()),
    };
    let preview = space.product_prepare(caller, input.clone()).await.unwrap();
    let mut competing = input.clone();
    competing.operation_id = "other-client".into();
    competing.new_value = Some("Other preference".into());
    let competing = space.product_prepare(caller, competing).await.unwrap();
    assert_eq!(
        space.product_record(&record.id).await.unwrap().status,
        "active"
    );
    let receipt = space
        .product_commit(
            caller,
            input.operation_id.clone(),
            preview.preview_digest.clone(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, "confirmed", "{receipt:?}");
    assert_eq!(
        space.product_record(&record.id).await.unwrap().status,
        "retracted"
    );
    assert!(
        space
            .product_commit(caller, competing.operation_id, competing.preview_digest)
            .await
            .is_err()
    );
    let new = space
        .product_record(receipt.replacement_record.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(new.object_label, "Risk section first");
    assert_eq!(new.status, "active");
    assert_ne!(new.proposition_id, record.proposition_id);
    assert_eq!(
        space
            .product_commit(caller, input.operation_id.clone(), preview.preview_digest)
            .await
            .unwrap()
            .replacement_record,
        receipt.replacement_record
    );
    let mut conflict = input.clone();
    conflict.new_value = Some("Different text".into());
    assert!(space.product_prepare(caller, conflict).await.is_err());
    let mut stale = input;
    stale.operation_id = "correction-2".into();
    assert!(space.product_prepare(caller, stale).await.is_err());
    assert!(!space.product_source_allowed(&source));
    let before = space.conversations.len();
    let error = space
        .ingest_product(
            SELF_USER_ID,
            crate::types::FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec!["Replay the old preference".to_string().into()],
                    ..Default::default()
                }],
                context: None,
                timestamp: None,
            },
            source,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<crate::product::SourceAdmissionError>(),
        Some(crate::product::SourceAdmissionError::Suppressed)
    ));
    assert_eq!(space.conversations.len(), before);
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_delete_erases_declared_claims_and_rejects_old_processing_epochs() {
    let app = test_app_state("product_delete");
    let space = create_loaded_space(&app, "product_delete").await;
    let caller = anda_core::Principal::management_canister();
    let (record, source) = managed_record(&space).await;
    let admitted_epoch = space.product_control.admit_source(&source).unwrap();
    let preview = space
        .product_prepare(
            caller,
            crate::product::ChangeInput {
                operation_id: "delete-1".into(),
                record_id: record.id.clone(),
                expected_revision: record.revision,
                kind: crate::product::ChangeKind::Delete,
                new_value: None,
            },
        )
        .await
        .unwrap();
    assert!(
        preview
            .preview
            .targets
            .iter()
            .any(|target| target.kind == "evidence")
    );
    let receipt = space
        .product_commit(caller, "delete-1".into(), preview.preview_digest)
        .await
        .unwrap();
    assert_eq!(receipt.state, "confirmed", "{receipt:?}");
    assert!(space.product_record(&record.id).await.is_err());
    assert!(!space.product_source_allowed(&source));
    let ctx = space
        .engine
        .ctx_with(
            SELF_USER_ID,
            FormationAgent::NAME,
            "",
            anda_core::RequestMeta::default(),
        )
        .unwrap();
    ctx.base.set_state(admitted_epoch);
    assert_eq!(
        space.product_control.admit_source(&source).err(),
        Some(crate::product::SourceAdmissionError::Suppressed)
    );
    let tool = crate::agents::GuardedMemory::new(space.memory.clone())
        .with_product_control(space.product_control.clone());
    let args=serde_json::from_value(serde_json::json!({"command":"CREATE CONCEPT ?x {TYPE \"Preference\" NAME \"Old context must not write\"}"})).unwrap();
    let error = tool
        .call(ctx.child_base("execute_kip").unwrap(), args, vec![])
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("memory changed"));
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_admitted_write_survives_a_lost_api_waiter() {
    let app = test_app_state("product_owned");
    let space = create_loaded_space(&app, "product_owned").await;
    let caller = anda_core::Principal::management_canister();
    let (record, _) = managed_record(&space).await;
    let preview = space
        .product_prepare(
            caller,
            crate::product::ChangeInput {
                operation_id: "owned-1".into(),
                record_id: record.id.clone(),
                expected_revision: record.revision,
                kind: crate::product::ChangeKind::Delete,
                new_value: None,
            },
        )
        .await
        .unwrap();
    let guard = space.product_control.gate.lock().await;
    let task_space = space.clone();
    let task = tokio::spawn(async move {
        task_space
            .product_commit(caller, "owned-1".into(), preview.preview_digest)
            .await
    });
    while !space.product_control.tasks.is_busy() {
        tokio::task::yield_now().await;
    }
    task.abort();
    drop(guard);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let receipt = space.product_change(caller, "owned-1").await.unwrap();
        if receipt.state == "confirmed" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{receipt:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(space.product_record(&record.id).await.is_err());
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_recovery_reopens_durable_admission_without_resending_old_sources() {
    let app = test_app_state("product_restart");
    let name = "product_restart";
    let space = create_loaded_space(&app, name).await;
    let caller = anda_core::Principal::management_canister();
    let (record, source) = managed_record(&space).await;
    let receipt = space
        .product_prepare(
            caller,
            crate::product::ChangeInput {
                operation_id: "restart-1".into(),
                record_id: record.id.clone(),
                expected_revision: record.revision,
                kind: crate::product::ChangeKind::Delete,
                new_value: None,
            },
        )
        .await
        .unwrap();
    // Crash boundary: admission/source fence is durable, but the operation's
    // own state still says prepared. No native request has executed yet.
    let mut state = space.product_control.snapshot();
    state.epoch += 1;
    state.pending = Some(receipt.operation_key.clone());
    state
        .suppressed
        .extend(receipt.preview.excluded_sources.clone());
    space.product_control.save(state).await.unwrap();
    assert_eq!(
        space.product_control.admit_source(&source).err(),
        Some(crate::product::SourceAdmissionError::Busy)
    );
    space.close().await.unwrap();
    app.spaces.write().await.remove(name);
    drop(space);
    let space = app.load_space_with(name, false, false).await.unwrap();
    assert!(space.product_available());
    assert_eq!(space.product_epoch(), 1);
    assert!(!space.product_source_allowed(&source));
    assert!(space.product_record(&record.id).await.is_err());
    assert_eq!(
        space
            .product_change(caller, "restart-1")
            .await
            .unwrap()
            .state,
        "confirmed"
    );
    assert_eq!(
        space
            .product_commit(caller, "restart-1".into(), receipt.preview_digest)
            .await
            .unwrap()
            .state,
        "confirmed"
    );
    assert_eq!(space.product_epoch(), 1);
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_suppression_keeps_auditable_records_but_excludes_active_reads_and_old_sources() {
    use crate::product::{ChangeInput, ChangeKind};
    let app = test_app_state("product_suppress");
    let space = create_loaded_space(&app, "product_suppress").await;
    let caller = anda_core::Principal::management_canister();
    let (record, source) = managed_record(&space).await;
    let receipt = space
        .product_prepare(
            caller,
            ChangeInput {
                operation_id: "suppress-1".into(),
                record_id: record.id.clone(),
                expected_revision: record.revision,
                kind: ChangeKind::Suppress,
                new_value: None,
            },
        )
        .await
        .unwrap();
    let result = space
        .product_commit(caller, "suppress-1".into(), receipt.preview_digest)
        .await
        .unwrap();
    assert_eq!(result.state, "confirmed", "{result:?}");
    assert_eq!(
        space
            .product_record(&record.id)
            .await
            .unwrap()
            .storage_state,
        "archived"
    );
    assert!(!space.product_source_allowed(&source));
    let response = space
        .execute_kip_readonly(anda_kip::Request::single(
            "FIND(?a) WHERE {?a ASSERTION {}} LIMIT 20",
        ))
        .await
        .unwrap();
    assert!(
        crate::kip::ok_result(&response)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let current = anda_kip::Request::single("FIND(?a) WHERE {?a ASSERTION {}} LIMIT 20");
    assert!(space.product_control.current_request(&current).is_ok());
    for command in [
        "FIND(?a) WHERE {?a ASSERTION {state: :state}} LIMIT 20",
        "FIND(?a) WHERE {OPTIONAL {?a ASSERTION {state: \"archived\"}}} LIMIT 20",
        "FIND(?a) WHERE {?a ASSERTION {}} AS OF SEQ 1 LIMIT 20",
    ] {
        let request = anda_kip::Request::single(command);
        request.parse_operations().unwrap();
        assert!(
            space.product_control.current_request(&request).is_err(),
            "{command}"
        );
    }
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_discards_are_final_and_retention_changes_refuse_admission() {
    use crate::product::{ChangeInput, ChangeKind};
    let app = test_app_state("product_hold");
    let space = create_loaded_space(&app, "product_hold").await;
    let caller = anda_core::Principal::management_canister();
    let (record, source) = managed_record(&space).await;
    let input = ChangeInput {
        operation_id: "discard-1".into(),
        record_id: record.id.clone(),
        expected_revision: record.revision,
        kind: ChangeKind::Correct,
        new_value: Some("New preference".into()),
    };
    space.product_prepare(caller, input.clone()).await.unwrap();
    space
        .product_discard(caller, input.operation_id.clone())
        .await
        .unwrap();
    assert!(space.product_prepare(caller, input).await.is_err());
    let preview = space
        .product_prepare(
            caller,
            ChangeInput {
                operation_id: "hold-1".into(),
                record_id: record.id.clone(),
                expected_revision: record.revision,
                kind: ChangeKind::Delete,
                new_value: None,
            },
        )
        .await
        .unwrap();
    seed_kip(
        &space,
        kip::request_with(
            "SET RETENTION :id {legal_hold:true}",
            kip::param("id", record.sources[0].evidence_id.clone()),
        ),
    )
    .await;
    assert!(
        space
            .product_commit(caller, "hold-1".into(), preview.preview_digest)
            .await
            .is_err()
    );
    assert!(space.product_available());
    assert!(space.product_source_allowed(&source));
    assert_eq!(space.product_epoch(), 0);
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_delete_clears_correction_copies_and_prepared_previews_across_reload() {
    use crate::product::{ChangeInput, ChangeKind};
    let app = test_app_state("product_delete_copies");
    let space = create_loaded_space(&app, "product_delete_copies").await;
    let caller = Principal::management_canister();
    let (record, _) = managed_record(&space).await;
    let correction = space
        .product_prepare(
            caller,
            ChangeInput {
                operation_id: "correct-copy".into(),
                record_id: record.id,
                expected_revision: record.revision,
                kind: ChangeKind::Correct,
                new_value: Some("private-corrected-value".into()),
            },
        )
        .await
        .unwrap();
    let corrected = space
        .product_commit(caller, correction.operation_id, correction.preview_digest)
        .await
        .unwrap();
    let record = space
        .product_record(corrected.replacement_record.as_deref().unwrap())
        .await
        .unwrap();
    let source = record.sources[0].clone();
    assert_eq!(
        space
            .product_correction_source(caller, &source)
            .await
            .unwrap()
            .as_deref(),
        Some("private-corrected-value")
    );
    let competing_input = ChangeInput {
        operation_id: "uncommitted-copy".into(),
        record_id: record.id.clone(),
        expected_revision: record.revision,
        kind: ChangeKind::Correct,
        new_value: Some("uncommitted-private-value".into()),
    };
    let competing = space
        .product_prepare(caller, competing_input.clone())
        .await
        .unwrap();
    let deletion = space
        .product_prepare(
            caller,
            ChangeInput {
                operation_id: "delete-copies".into(),
                record_id: record.id,
                expected_revision: record.revision,
                kind: ChangeKind::Delete,
                new_value: None,
            },
        )
        .await
        .unwrap();
    let deleted = space
        .product_commit(caller, deletion.operation_id, deletion.preview_digest)
        .await
        .unwrap();
    assert_eq!(deleted.state, "confirmed");
    assert!(
        space
            .product_correction_source(caller, &source)
            .await
            .unwrap()
            .is_none()
    );
    for operation in [&corrected, &competing] {
        let saved = space
            .product_change(caller, &operation.operation_id)
            .await
            .unwrap();
        assert!(saved.preview.new_value.is_none());
        assert!(saved.preview.record.text.is_empty());
        let raw = space
            .product_control
            .journal
            .read::<serde_json::Value>(&format!("changes/{}", operation.operation_key))
            .await
            .unwrap()
            .unwrap()
            .value;
        assert_eq!(raw["requests"], serde_json::json!([]));
        assert!(raw["input"]["new_value"].is_null());
        assert!(!raw.to_string().contains("private-value"));
        assert!(!raw.to_string().contains("private-corrected-value"));
    }
    assert_eq!(
        space
            .product_change(caller, &competing.operation_id)
            .await
            .unwrap()
            .state,
        "discarded"
    );
    assert!(
        space
            .product_prepare(caller, competing_input)
            .await
            .is_err()
    );
    assert!(
        space
            .product_commit(caller, competing.operation_id, competing.preview_digest)
            .await
            .is_err()
    );
    space.close().await.unwrap();
    app.spaces.write().await.remove("product_delete_copies");
    drop(space);
    let space = app
        .load_space_with("product_delete_copies", false, false)
        .await
        .unwrap();
    assert!(
        space
            .product_correction_source(caller, &source)
            .await
            .unwrap()
            .is_none()
    );
    let retried = space
        .product_commit(caller, corrected.operation_id, corrected.preview_digest)
        .await
        .unwrap();
    assert_eq!(retried.state, "confirmed");
    assert!(retried.preview.new_value.is_none());
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_delete_preserves_a_surviving_corrections_source() {
    use crate::product::{ChangeInput, ChangeKind};
    let app = test_app_state("product_keep_correction");
    let space = create_loaded_space(&app, "product_keep_correction").await;
    let caller = Principal::management_canister();
    let (mut record, _) = managed_record(&space).await;
    let mut changes = Vec::new();
    for value in ["earlier-value", "current-value"] {
        let prepared = space
            .product_prepare(
                caller,
                ChangeInput {
                    operation_id: value.into(),
                    record_id: record.id,
                    expected_revision: record.revision,
                    kind: ChangeKind::Correct,
                    new_value: Some(value.into()),
                },
            )
            .await
            .unwrap();
        let changed = space
            .product_commit(caller, prepared.operation_id, prepared.preview_digest)
            .await
            .unwrap();
        record = space
            .product_record(changed.replacement_record.as_deref().unwrap())
            .await
            .unwrap();
        changes.push(changed);
    }
    let old = space
        .product_record(changes[0].replacement_record.as_deref().unwrap())
        .await
        .unwrap();
    let deletion = space
        .product_prepare(
            caller,
            ChangeInput {
                operation_id: "delete-earlier".into(),
                record_id: old.id,
                expected_revision: old.revision,
                kind: ChangeKind::Delete,
                new_value: None,
            },
        )
        .await
        .unwrap();
    assert!(
        !deletion
            .preview
            .targets
            .iter()
            .any(|target| target.id == record.id)
    );
    space
        .product_commit(caller, deletion.operation_id, deletion.preview_digest)
        .await
        .unwrap();
    assert_eq!(
        space.product_record(&record.id).await.unwrap().status,
        "active"
    );
    assert_eq!(
        space
            .product_correction_source(caller, &record.sources[0])
            .await
            .unwrap()
            .as_deref(),
        Some("current-value")
    );
    let receipt = space
        .product_change(caller, &changes[1].operation_id)
        .await
        .unwrap();
    assert!(receipt.preview.record.object.is_null());
    assert_eq!(receipt.preview.new_value.as_deref(), Some("current-value"));
    space.close().await.unwrap();
}

#[tokio::test]
async fn product_changes_reject_old_cursors_but_preserve_new_pagination_after_reload() {
    use crate::product::{ChangeInput, ChangeKind};
    let app = test_app_state("product_cursors");
    let space = create_loaded_space(&app, "product_cursors").await;
    let caller = Principal::management_canister();
    let (record, _) = managed_record(&space).await;
    let mut first_page = kip::request_with(
        "FIND(?c) WHERE {?c CONCEPT {}} LIMIT :limit",
        kip::param("limit", 1),
    );
    first_page.operations[0].parameters = Some(serde_json::Map::new());
    let old = space
        .execute_kip_readonly(first_page.clone())
        .await
        .unwrap()
        .results[0]
        .next_cursor
        .clone()
        .unwrap();
    let change = space
        .product_prepare(
            caller,
            ChangeInput {
                operation_id: "stop-before-paging".into(),
                record_id: record.id,
                expected_revision: record.revision,
                kind: ChangeKind::Suppress,
                new_value: None,
            },
        )
        .await
        .unwrap();
    space
        .product_commit(caller, change.operation_id, change.preview_digest)
        .await
        .unwrap();
    let fresh = space
        .execute_kip_readonly(first_page)
        .await
        .unwrap()
        .results[0]
        .next_cursor
        .clone()
        .unwrap();
    space.close().await.unwrap();
    app.spaces.write().await.remove("product_cursors");
    drop(space);
    let space = app
        .load_space_with("product_cursors", false, false)
        .await
        .unwrap();
    let ctx = space
        .ctx_for_test(SELF_USER_ID, crate::agents::RecallAgent::NAME)
        .unwrap();
    ctx.base.set_state(crate::product::control::ProcessingEpoch(
        space.product_epoch(),
    ));
    let tool = TimedMemoryReadonly::new(space.memory.clone())
        .with_product_control(space.product_control.clone());
    for (cursor, allowed) in [(old, false), (fresh, true)] {
        for literal in [false, true] {
            let mut request = kip::request_with(
                if literal {
                    format!(
                        "FIND(?c) WHERE {{?c CONCEPT {{}}}} LIMIT :limit CURSOR {}",
                        kip::string_literal(&cursor)
                    )
                } else {
                    "FIND(?c) WHERE {?c CONCEPT {}} LIMIT :limit CURSOR :cursor".into()
                },
                kip::param("limit", 1),
            );
            if !literal {
                request.operations[0].parameters = Some(kip::param("cursor", cursor.clone()));
            } else {
                request.operations[0].parameters = Some(serde_json::Map::new());
            }
            let args = serde_json::from_value(serde_json::json!({
                "operations": request.operations, "parameters": request.parameters,
            }))
            .unwrap();
            let result = tool
                .call(
                    ctx.child_base("execute_kip_readonly").unwrap(),
                    args,
                    vec![],
                )
                .await;
            if allowed {
                let response = result.unwrap().output;
                assert!(kip::succeeded(&response), "{response:?}");
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("historical or inactive"),
                    "{error}"
                );
            }
        }
    }
    space.close().await.unwrap();
}

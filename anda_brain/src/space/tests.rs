use super::{
    AppState, Hooks, MAINTENANCE_MAX_INTERVAL_MS, Space, SpaceEntry, init_conversation_collection,
    init_resource_collection, settlement_error_messages,
};
use crate::settlement;
use crate::{
    agents::{BrainHook, FormationAgent, MaintenanceAgent, SELF_USER_ID, TimedMemoryReadonly},
    kip,
    payload::StringOr,
    testkit::{app_state_core, create_loaded_space, signed_token, signing_key},
    types::{
        AddSpaceTokenInput, FormationInput, InputContext, MaintenanceInput, MaintenanceParameters,
        MaintenanceScope, MemoryPolicy, ModelConfig, RecallInput, SpaceTier, SpaceToken,
        TokenScope, UpdateSpaceInput,
    },
};
use anda_core::{
    AgentOutput, BoxError, BoxPinFut, CompletionRequest, Message, Principal, Resource, Tool, Usage,
};
use anda_db::collection::CollectionConfig;
use anda_engine::{
    context::BaseCtx,
    memory::{Conversation, ConversationRef, ConversationStatus, KipArgs, MemoryReadonly},
    model::{CompletionFeaturesDyn, Model, Models},
    unix_ms,
};
use ic_cose_types::cose::ed25519::{SigningKey, VerifyingKey};
use object_store::memory::InMemory;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct FinalCompleter;

impl CompletionFeaturesDyn for FinalCompleter {
    fn model_name(&self) -> String {
        "final-test-model".to_string()
    }

    fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(async move {
            Ok(AgentOutput {
                content: "done".to_string(),
                chat_history: vec![Message {
                    role: "assistant".to_string(),
                    content: vec![format!("processed: {}", req.prompt).into()],
                    ..Default::default()
                }],
                ..Default::default()
            })
        })
    }
}

/// Answers the self-test query-generation call: the first candidate (by
/// id order) gets a query matching its subject name, the second gets
/// unfindable gibberish — one grounded, one not.
#[derive(Debug)]
struct SelfTestCompleter;

impl CompletionFeaturesDyn for SelfTestCompleter {
    fn model_name(&self) -> String {
        "self-test-model".to_string()
    }

    fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(async move {
            let candidates: Vec<serde_json::Value> =
                serde_json::from_str(&req.prompt).unwrap_or_default();
            let mut ids: Vec<(String, String)> = candidates
                .iter()
                .filter_map(|candidate| {
                    Some((
                        candidate.get("id")?.as_str()?.to_string(),
                        candidate.get("subject_name")?.as_str()?.to_string(),
                    ))
                })
                .collect();
            ids.sort();
            let queries: Vec<serde_json::Value> = ids
                .iter()
                .enumerate()
                .map(|(index, (id, subject_name))| {
                    let query = if index == 0 {
                        subject_name.clone()
                    } else {
                        "qqqzzzxxx nonsense".to_string()
                    };
                    serde_json::json!({"id": id, "query": query})
                })
                .collect();
            Ok(AgentOutput {
                content: serde_json::json!({ "queries": queries }).to_string(),
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

fn test_app_state_with_self_test_model(name: &str) -> AppState {
    let models = Models::default();
    models.set_model(Model::with_completer(Arc::new(SelfTestCompleter)));
    test_app_state_with_models(name, Arc::new(models))
}

#[derive(Debug)]
struct SlowCompleter;

impl CompletionFeaturesDyn for SlowCompleter {
    fn model_name(&self) -> String {
        "slow-test-model".to_string()
    }

    fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(async move {
            sleep(Duration::from_millis(150)).await;
            Ok(AgentOutput {
                content: "slow done".to_string(),
                chat_history: vec![Message {
                    role: "assistant".to_string(),
                    content: vec![format!("slow processed: {}", req.prompt).into()],
                    ..Default::default()
                }],
                ..Default::default()
            })
        })
    }
}

fn test_app_state(name: &str) -> AppState {
    test_app_state_with_models(name, Arc::new(Models::default()))
}

fn test_app_state_with_final_model(name: &str) -> AppState {
    let models = Models::default();
    models.set_model(Model::with_completer(Arc::new(FinalCompleter)));
    test_app_state_with_models(name, Arc::new(models))
}

fn test_app_state_with_slow_model(name: &str) -> AppState {
    let models = Models::default();
    models.set_model(Model::with_completer(Arc::new(SlowCompleter)));
    test_app_state_with_models(name, Arc::new(models))
}

fn test_app_state_with_pubkeys(name: &str) -> AppState {
    let mut bytes = [0x66; 32];
    bytes[0] = 0x58;
    let key = VerifyingKey::from_bytes(&bytes).unwrap();
    app_state_core(name, Arc::new(Models::default()), vec![key], "test", 0)
}

fn test_app_state_with_signing_key(name: &str, signing_key: &SigningKey) -> AppState {
    app_state_core(
        name,
        Arc::new(Models::default()),
        vec![signing_key.verifying_key()],
        "test",
        0,
    )
}

fn test_app_state_with_models(name: &str, models: Arc<Models>) -> AppState {
    app_state_core(name, models, vec![], "test", 0)
}

async fn wait_until_idle(space: &Space) {
    for _ in 0..100 {
        if !space.is_processing() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("space did not become idle");
}

#[tokio::test]
async fn copy_space_objects_forks_space_into_isolated_store() {
    let app = test_app_state("fork_src");
    let space = create_loaded_space(&app, "fork_space").await;
    space
        .update(
            UpdateSpaceInput {
                name: Some("before fork".to_string()),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();
    space.db.close().await.unwrap();

    let fork_store: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
    let copied = super::copy_space_objects(&app.object_store(), &fork_store, "fork_space")
        .await
        .unwrap();
    assert!(copied > 0);

    // Copying a missing space fails loudly instead of forking nothing.
    let empty: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
    assert!(
        super::copy_space_objects(&app.object_store(), &empty, "missing_space")
            .await
            .is_err()
    );

    // The fork opens under the same id in its own store, sees the same
    // state, and mutations do not leak back to the source store.
    let fork_state = app.fork_with_store(fork_store);
    let fork = fork_state.load_space("fork_space", true).await.unwrap();
    assert_eq!(fork.get_info().name.as_deref(), Some("before fork"));
    fork.update(
        UpdateSpaceInput {
            name: Some("after fork".to_string()),
            ..Default::default()
        },
        unix_ms(),
    )
    .await
    .unwrap();
    fork.db.close().await.unwrap();

    let fork_store2: Arc<dyn super::ObjectStore> = Arc::new(InMemory::new());
    super::copy_space_objects(&app.object_store(), &fork_store2, "fork_space")
        .await
        .unwrap();
    let fork_state2 = app.fork_with_store(fork_store2);
    let fork2 = fork_state2.load_space("fork_space", true).await.unwrap();
    assert_eq!(fork2.get_info().name.as_deref(), Some("before fork"));
    fork2.db.close().await.unwrap();
}

#[test]
fn space_entry_starts_uninitialized_with_recent_access_time() {
    let before = unix_ms();
    let entry = SpaceEntry::new();
    let after = unix_ms();

    assert!(!entry.cell.initialized());
    assert!(entry.last_access_ms() >= before);
    assert!(entry.last_access_ms() <= after);
}

#[test]
fn space_entry_touch_refreshes_last_access_time() {
    let entry = SpaceEntry::new();
    entry.last_access_ms.store(0, Ordering::Relaxed);
    let before_touch = unix_ms();

    entry.touch();

    assert!(entry.last_access_ms() >= before_touch);
}

#[tokio::test]
async fn create_space_persists_metadata_before_returning() {
    let object_store = Arc::new(InMemory::new());
    let db_config = crate::testkit::db_config("create_space_persists_metadata");
    let creator = Principal::from_slice(&[1]);
    let owner = Principal::from_slice(&[2]);

    let info = Space::create(
        object_store.clone(),
        db_config.clone(),
        creator,
        owner,
        1,
        123,
    )
    .await
    .unwrap();

    assert_eq!(info.owner, owner.to_string());
    assert_eq!(info.tier.tier, 1);

    let db = anda_db::database::AndaDB::open(object_store, db_config)
        .await
        .unwrap();
    let persisted_owner: String = db.get_extension_as("owner").unwrap();
    let persisted_tier: SpaceTier = db.get_extension_as("tier").unwrap();

    assert_eq!(persisted_owner, owner.to_string());
    assert_eq!(persisted_tier.tier, 1);

    db.close().await.unwrap();
}

#[tokio::test]
async fn collection_bootstrap_helpers_create_and_prune_indexes() {
    let object_store = Arc::new(InMemory::new());
    let db_config = crate::testkit::db_config("collection_bootstrap_helpers");
    let db = anda_db::database::AndaDB::create(object_store, db_config)
        .await
        .unwrap();
    let mut conversation_schema = Conversation::schema().unwrap();
    conversation_schema.with_version(4);

    let conversations = db
        .open_or_create_collection(
            conversation_schema,
            CollectionConfig {
                name: "conversations".to_string(),
                description: "conversations collection".to_string(),
            },
            async |collection| {
                collection.create_btree_index_nx(&["thread"]).await?;
                collection.create_btree_index_nx(&["period"]).await?;
                collection
                    .create_bm25_index_nx(&["messages", "resources", "artifacts"])
                    .await?;
                init_conversation_collection(collection).await
            },
        )
        .await
        .unwrap();
    let meta = conversations.metadata();
    assert!(meta.btree_indexes.contains_key("user"));
    assert!(!meta.btree_indexes.contains_key("thread"));
    assert!(!meta.btree_indexes.contains_key("period"));
    assert!(
        !meta
            .bm25_indexes
            .contains_key("messages-resources-artifacts")
    );

    let resources = db
        .open_or_create_collection(
            Resource::schema().unwrap(),
            CollectionConfig {
                name: "resources".to_string(),
                description: "Resources collection".to_string(),
            },
            async |collection| {
                collection
                    .create_bm25_index_nx(&["name", "description", "metadata"])
                    .await?;
                init_resource_collection(collection).await
            },
        )
        .await
        .unwrap();
    let meta = resources.metadata();
    assert!(meta.btree_indexes.contains_key("tags"));
    assert!(meta.btree_indexes.contains_key("hash"));
    assert!(meta.btree_indexes.contains_key("mime_type"));
    assert!(!meta.bm25_indexes.contains_key("name-description-metadata"));

    db.close().await.unwrap();
}

/// `Space::connect` opens every conversation collection itself, then hands
/// the same names to `MemoryManagement::connect` / `Conversations::connect`
/// so the engine wrappers adopt the already-open handles. If they ever
/// re-ran their own bootstrap instead, they would recreate exactly the
/// indexes `init_conversation_collection` drops.
#[tokio::test]
async fn connected_space_keeps_the_trimmed_conversation_index_layout() {
    let app = test_app_state("space_index_layout");
    let space = create_loaded_space(&app, "space_index_layout").await;

    for collection in [
        &space.conversations,
        &space.recall.conversations_collection,
        &space.maintenance.conversations_collection,
    ] {
        let meta = collection.metadata();
        assert!(meta.btree_indexes.contains_key("user"));
        assert!(!meta.btree_indexes.contains_key("thread"));
        assert!(!meta.btree_indexes.contains_key("period"));
        assert!(
            !meta
                .bm25_indexes
                .contains_key("messages-resources-artifacts")
        );
    }
}

#[test]
fn app_state_allows_local_auth_when_no_pubkeys_are_configured() {
    let app = test_app_state("local_auth");
    let now_ms = 123;

    let admin = app
        .check_admin("", "space", TokenScope::Write, now_ms)
        .unwrap();
    assert_eq!(admin.user, Principal::management_canister());
    assert_eq!(admin.audience, "space");
    assert_eq!(admin.scope, TokenScope::Write);

    let user = app
        .check_auth("", "space", TokenScope::Read, now_ms)
        .unwrap();
    assert_eq!(user.user, SELF_USER_ID);

    let optional = app
        .check_auth_if("", "space", TokenScope::Read, now_ms)
        .unwrap()
        .unwrap();
    assert_eq!(optional.user, SELF_USER_ID);
}

#[test]
fn app_state_rejects_invalid_tokens_when_pubkeys_are_configured() {
    let app = test_app_state_with_pubkeys("configured_auth");
    let now_ms = 123;

    assert!(
        app.check_auth_if("short", "space", TokenScope::Read, now_ms)
            .unwrap()
            .is_none()
    );
    assert!(
        app.check_auth("not-base64", "space", TokenScope::Read, now_ms)
            .is_err()
    );
    assert!(
        app.check_admin("not-base64", "space", TokenScope::Write, now_ms)
            .is_err()
    );
}

#[test]
fn app_state_accepts_valid_signed_tokens_and_rejects_scope_mismatches() {
    let signing_key = signing_key(7);
    let app = test_app_state_with_signing_key("signed_auth", &signing_key);
    let now_ms = 1_725_000_000_000;

    let read_token = signed_token(&signing_key, SELF_USER_ID, "space-a", "read");
    let auth = app
        .check_auth(&read_token, "space-a", TokenScope::Read, now_ms)
        .unwrap();
    assert_eq!(auth.user, SELF_USER_ID);
    assert_eq!(auth.audience, "space-a");
    assert_eq!(auth.scope, TokenScope::Read);
    assert!(
        app.check_auth(&read_token, "space-a", TokenScope::Write, now_ms)
            .err()
            .unwrap()
            .to_string()
            .contains("insufficient scope")
    );
    assert!(
        app.check_auth(&read_token, "space-b", TokenScope::Read, now_ms)
            .err()
            .unwrap()
            .to_string()
            .contains("invalid audience")
    );

    let admin_token = signed_token(&signing_key, SELF_USER_ID, "*", "*");
    let admin = app
        .check_admin(&admin_token, "any-space", TokenScope::Write, now_ms)
        .unwrap();
    assert_eq!(admin.user, SELF_USER_ID);
    assert_eq!(admin.scope, TokenScope::All);

    let optional = app
        .check_auth_if(&admin_token, "any-space", TokenScope::Read, now_ms)
        .unwrap()
        .unwrap();
    assert_eq!(optional.audience, "*");

    let non_admin = signed_token(&signing_key, Principal::from_slice(&[99]), "*", "*");
    assert!(
        app.check_admin(&non_admin, "any-space", TokenScope::Read, now_ms)
            .err()
            .unwrap()
            .to_string()
            .contains("admin access required")
    );
}

#[tokio::test]
async fn app_state_loads_spaces_once_and_rejects_duplicate_loaded_space() {
    let app = test_app_state("load_cache");
    let id = "load_cache_space";
    let owner = Principal::from_slice(&[3]);

    let info = app
        .admin_create_space(Principal::from_slice(&[1]), owner, id.to_string(), 2, 456)
        .await
        .unwrap();
    assert_eq!(info.id, id);
    assert_eq!(info.owner, owner.to_string());

    let loaded = app.load_space(id, false).await.unwrap();
    let loaded_again = app.load_space(id, false).await.unwrap();
    assert!(Arc::ptr_eq(&loaded, &loaded_again));

    let err = app
        .admin_create_space(Principal::from_slice(&[1]), owner, id.to_string(), 2, 456)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[tokio::test]
async fn app_state_background_shutdown_and_idle_eviction_paths() {
    let app = test_app_state("background_eviction");
    let space_id = "background_eviction_space";
    let space = create_loaded_space(&app, space_id).await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), app.start_background_tasks(cancel))
        .await
        .unwrap();

    let entry = {
        let spaces = app.spaces.read().await;
        spaces.get(space_id).unwrap().clone()
    };
    app.flush_and_evict_once(unix_ms(), 10_000).await;
    assert!(app.spaces.read().await.contains_key(space_id));

    entry.last_access_ms.store(0, Ordering::Relaxed);
    assert!(!app.try_evict_idle_space(space_id, &entry, 10_000, 1).await);

    let wrong_entry = Arc::new(SpaceEntry::new());
    assert!(
        !app.try_evict_idle_space(space_id, &wrong_entry, 10_000, 1)
            .await
    );

    drop(space);
    for _ in 0..100 {
        let space_refs = entry.cell.get().map(Arc::strong_count).unwrap_or_default();
        if space_refs == 1 {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    drop(entry);
    app.flush_and_evict_once(10_000, 1).await;
    assert!(!app.spaces.read().await.contains_key(space_id));

    let missing_entry = Arc::new(SpaceEntry::new());
    assert!(
        !app.try_evict_idle_space("missing_space", &missing_entry, 10_000, 1)
            .await
    );

    assert!(app.load_space("never_created_space", false).await.is_err());
    let uninitialized = {
        let spaces = app.spaces.read().await;
        spaces.get("never_created_space").unwrap().clone()
    };
    assert!(
        !app.try_evict_idle_space("never_created_space", &uninitialized, 10_000, 1)
            .await
    );
}

#[tokio::test]
async fn flush_and_evict_removes_idle_uninitialized_placeholders() {
    let app = test_app_state("placeholder_eviction");
    assert!(app.load_space("placeholder_space", false).await.is_err());
    {
        let spaces = app.spaces.read().await;
        let entry = spaces.get("placeholder_space").unwrap();
        assert!(!entry.cell.initialized());
    }

    // Not idle yet: the placeholder entry is kept for retrying.
    app.flush_and_evict_once(unix_ms(), 10_000).await;
    assert!(app.spaces.read().await.contains_key("placeholder_space"));

    // Idle: the placeholder is dropped so probes for unknown space IDs
    // cannot grow the map unboundedly.
    app.flush_and_evict_once(unix_ms() + 20_000, 10_000).await;
    assert!(!app.spaces.read().await.contains_key("placeholder_space"));
}

#[tokio::test]
async fn space_metadata_tier_byok_and_tokens_roundtrip() {
    let app = test_app_state("space_metadata");
    let space = create_loaded_space(&app, "space_metadata").await;

    let tier = space.admin_update_tier(3, 999).await.unwrap();
    assert_eq!(tier.tier, 3);
    assert_eq!(space.get_tier().tier, 3);

    space
        .update(
            UpdateSpaceInput {
                name: Some("Research Brain".to_string()),
                description: Some("memory space".to_string()),
                public: Some(true),
                ..Default::default()
            },
            1000,
        )
        .await
        .unwrap();
    assert!(space.is_public());

    let info = space.get_info();
    assert_eq!(info.name.as_deref(), Some("Research Brain"));
    assert_eq!(info.description.as_deref(), Some("memory space"));
    assert_eq!(info.tier.tier, 3);

    let byok = ModelConfig {
        family: "openai".to_string(),
        model: "gpt-test".to_string(),
        api_base: "https://api.example.test".to_string(),
        api_key: "test-key".to_string(),
        ..Default::default()
    };
    space.update_byok(byok.clone()).await.unwrap();
    assert_eq!(space.get_byok().unwrap().model, byok.model);

    let disabled_byok = ModelConfig {
        family: "openai".to_string(),
        model: "disabled-test".to_string(),
        api_base: "https://api.example.test".to_string(),
        api_key: "test-key".to_string(),
        disabled: true,
        ..Default::default()
    };
    let err = space.update_byok(disabled_byok).await.unwrap_err();
    assert!(err.to_string().contains("model is disabled"));
    assert_eq!(space.get_byok().unwrap().model, byok.model);

    let token = "STtest-token".to_string();
    let st = space
        .add_space_token(
            token.clone(),
            AddSpaceTokenInput {
                scope: TokenScope::Read,
                name: "reader".to_string(),
                expires_at: Some(2000),
                labels: None,
            },
            1100,
        )
        .await
        .unwrap();
    assert_eq!(st.scope, TokenScope::Read);
    assert_eq!(st.name, "reader");

    space
        .verify_space_token(token.clone(), TokenScope::Read, 1200)
        .unwrap();
    assert!(
        space
            .verify_space_token(token.clone(), TokenScope::Write, 1200)
            .is_err()
    );
    assert!(
        space
            .verify_space_token(token.clone(), TokenScope::Read, 2500)
            .is_err()
    );

    let tokens = space.list_space_tokens().unwrap();
    assert_eq!(tokens.len(), 1);
    // The listing redacts the credential to a display prefix.
    assert_eq!(tokens[0].token, "STtest-t…");
    assert_eq!(tokens[0].usage, 1);

    // Revocation works by name (the listing no longer echoes values)…
    assert!(space.revoke_space_token_by_name("reader").await.unwrap());
    assert!(!space.revoke_space_token_by_name("reader").await.unwrap());
    // …and by full token value.
    let st2 = space
        .add_space_token(
            "STtest-token".to_string(),
            AddSpaceTokenInput {
                scope: TokenScope::Read,
                name: "reader".to_string(),
                expires_at: None,
                labels: None,
            },
            1300,
        )
        .await
        .unwrap();
    assert_eq!(st2.token, "STtest-token");
    assert!(space.revoke_space_token("STtest-token").await.unwrap());
    assert!(!space.revoke_space_token("STtest-token").await.unwrap());

    // Platform-managed extensions must not be deletable through the
    // space-token revoke API.
    assert!(space.revoke_space_token("tier").await.is_err());
    assert_eq!(space.get_tier().tier, 3);
    assert!(space.revoke_space_token("byok").await.is_err());
    assert!(space.get_byok().is_some());

    space
        .update(
            UpdateSpaceInput {
                ..Default::default()
            },
            3000,
        )
        .await
        .unwrap();
    assert!(space.get_byok().is_some());
}

#[tokio::test]
async fn labeled_space_tokens_are_read_only_wiki_viewers() {
    let app = test_app_state("labeled_tokens");
    let space = create_loaded_space(&app, "labeled_tokens").await;

    // Labels are trimmed and deduped.
    let st = space
        .add_space_token(
            "STlabeled".to_string(),
            AddSpaceTokenInput {
                scope: TokenScope::Read,
                name: "auditor".to_string(),
                expires_at: None,
                labels: Some(vec![" hr ".to_string(), "hr".to_string(), " ".to_string()]),
            },
            1000,
        )
        .await
        .unwrap();
    assert_eq!(st.labels, Some(vec!["hr".to_string()]));

    // Labels with a write-capable scope are rejected at creation: they
    // would allow committing to / exporting documents behind labels the
    // token cannot read (launch review P0-2/P1-1).
    for (idx, scope) in [TokenScope::Write, TokenScope::All].into_iter().enumerate() {
        let err = space
            .add_space_token(
                format!("STw{idx}"),
                AddSpaceTokenInput {
                    scope,
                    name: format!("writer{idx}"),
                    expires_at: None,
                    labels: Some(vec!["hr".to_string()]),
                },
                1000,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read scope"), "{err}");
    }

    // A legacy labeled row carrying a write scope fails closed at verify
    // but still works as the read-only viewer it was meant to be.
    let legacy = SpaceToken {
        token: "STlegacy".to_string(),
        scope: TokenScope::All,
        name: "legacy".to_string(),
        labels: Some(vec!["hr".to_string()]),
        ..Default::default()
    };
    space
        .db
        .save_extension_from("STlegacy".to_string(), &legacy.to_ref())
        .await
        .unwrap();
    assert!(
        space
            .verify_space_token("STlegacy".to_string(), TokenScope::All, 2000)
            .is_err()
    );
    assert!(
        space
            .verify_space_token("STlegacy".to_string(), TokenScope::Write, 2000)
            .is_err()
    );
    assert!(
        space
            .verify_space_token("STlegacy".to_string(), TokenScope::Read, 2000)
            .is_ok()
    );

    // Token names are audit identities (`st:{name}`): duplicates would
    // make two tokens indistinguishable in the event log.
    let err = space
        .add_space_token(
            "STdup".to_string(),
            AddSpaceTokenInput {
                scope: TokenScope::Read,
                name: "auditor".to_string(),
                expires_at: None,
                labels: None,
            },
            1000,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[tokio::test]
async fn memory_policy_round_trips_and_rejects_invalid_values() {
    let app = test_app_state("memory_policy");
    let space = create_loaded_space(&app, "memory_policy").await;

    // Absent policy means defaults (compiled-in behavior).
    assert_eq!(space.memory_policy(), MemoryPolicy::default());

    let policy = MemoryPolicy {
        memory_strength_decay_factor: 0.9,
        orphan_max_count: 5,
        ..Default::default()
    };
    space
        .update(
            UpdateSpaceInput {
                memory_policy: Some(policy.clone()),
                ..Default::default()
            },
            1000,
        )
        .await
        .unwrap();
    assert_eq!(space.memory_policy(), policy);

    // Invalid values reject the update and leave the stored policy alone.
    let invalid = MemoryPolicy {
        memory_strength_decay_factor: 0.0,
        ..Default::default()
    };
    let err = space
        .update(
            UpdateSpaceInput {
                memory_policy: Some(invalid),
                ..Default::default()
            },
            1001,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("memory_strength_decay_factor"));
    assert_eq!(space.memory_policy(), policy);

    // Budget knobs are capped, not just floored: this object is settable
    // over HTTP, and an unbounded self-test budget is a cost bomb.
    let bomb = MemoryPolicy {
        self_test_queries_per_cycle: u32::MAX,
        ..Default::default()
    };
    let err = space
        .update(
            UpdateSpaceInput {
                memory_policy: Some(bomb),
                ..Default::default()
            },
            1002,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("self_test_queries_per_cycle"));
    assert_eq!(space.memory_policy(), policy);
}

#[tokio::test]
async fn maintenance_fills_parameters_from_memory_policy() {
    let app = test_app_state_with_slow_model("maintenance_policy_params");
    let space = create_loaded_space(&app, "maintenance_policy_params").await;
    space
        .update(
            UpdateSpaceInput {
                memory_policy: Some(MemoryPolicy {
                    unconsolidated_max_backlog: 42,
                    ..Default::default()
                }),
                ..Default::default()
            },
            1000,
        )
        .await
        .unwrap();

    let output = space
        .maintenance(SELF_USER_ID, MaintenanceInput::default())
        .await
        .unwrap();
    let conversation = space
        .get_conversation(
            Some("maintenance".to_string()),
            output.conversation.unwrap(),
        )
        .await
        .unwrap();
    let encoded = serde_json::to_string(&conversation.messages).unwrap();
    // The prompt is the pretty-printed MaintenanceInput JSON, escaped
    // inside the stored message text.
    assert!(encoded.contains("\\\"unconsolidated_max_backlog\\\": 42"));
    assert!(encoded.contains("\\\"memory_strength_decay_factor\\\": 0.95"));
}

#[tokio::test]
async fn maintenance_keeps_explicit_parameters() {
    let app = test_app_state_with_slow_model("maintenance_explicit_params");
    let space = create_loaded_space(&app, "maintenance_explicit_params").await;

    let input = MaintenanceInput {
        parameters: Some(MaintenanceParameters {
            stale_event_threshold_days: Some(3),
            memory_strength_decay_factor: None,
            unconsolidated_max_backlog: None,
            orphan_max_count: None,
        }),
        ..Default::default()
    };
    let output = space.maintenance(SELF_USER_ID, input).await.unwrap();
    let conversation = space
        .get_conversation(
            Some("maintenance".to_string()),
            output.conversation.unwrap(),
        )
        .await
        .unwrap();
    let encoded = serde_json::to_string(&conversation.messages).unwrap();
    assert!(encoded.contains("\\\"stale_event_threshold_days\\\": 3"));
    // Explicit values win; omitted values use the same effective policy as settlement.
    assert!(encoded.contains("memory_strength_decay_factor"));
}

#[tokio::test]
async fn maintenance_override_controls_the_actual_decay_without_changing_space_policy() {
    let app = test_app_state_with_final_model("decay_override");
    let space = create_loaded_space(&app, "decay_override").await;
    space.memory.execute(r#"CREATE CONCEPT ?c { TYPE "Preference" NAME "keep accessible" SET FACET "MnemonicState" {memory_strength: 0.8} }"#, None).await.unwrap();
    let previous_policy = space.memory_policy();
    space
        .maintenance(
            SELF_USER_ID,
            MaintenanceInput {
                scope: MaintenanceScope::Quick,
                parameters: Some(MaintenanceParameters {
                    memory_strength_decay_factor: Some(1.0),
                    stale_event_threshold_days: None,
                    unconsolidated_max_backlog: None,
                    orphan_max_count: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let response = space.execute_kip_readonly(kip::request(r#"FIND(?c.facets["MnemonicState"].memory_strength) WHERE { ?c CONCEPT {type: "Preference"} } LIMIT 10"#)).await.unwrap();
    assert_eq!(kip::ok_result(&response), Some(&serde_json::json!([0.8])));
    assert_eq!(space.memory_policy(), previous_policy);
}

#[tokio::test]
async fn a_published_v1_space_resets_only_legacy_bookkeeping_once() {
    use object_store::ObjectStoreExt;
    #[derive(serde::Deserialize)]
    struct StoredObject {
        path: String,
        bytes: ic_auth_types::ByteBufB64,
    }
    let app = test_app_state("published_v011_fixture");
    let snapshot: Vec<StoredObject> =
        cbor2::from_reader(include_bytes!("../../tests/fixtures/published_v0_11.cbor").as_slice())
            .unwrap();
    for object in snapshot {
        app.object_store
            .put(
                &object_store::path::Path::from(object.path),
                object.bytes.0.into(),
            )
            .await
            .unwrap();
    }
    let db = Arc::new(
        anda_db::database::AndaDB::open(
            app.object_store.clone(),
            crate::testkit::db_config("published_v011_fixture"),
        )
        .await
        .unwrap(),
    );
    let ledger = crate::ledger::UsageLedger::connect(&db).await.unwrap();
    ledger
        .record_recall(&BTreeSet::from(["C:1".into(), "C-999".into()]), unix_ms())
        .await
        .unwrap();
    let misses = crate::ledger::MissCache::connect(&db).await.unwrap();
    misses.record_miss("old search", unix_ms()).await.unwrap();
    db.save_extension_from(
        "memory_metrics".into(),
        &crate::types::MemoryMetrics {
            recalls_completed: 9,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.close().await.unwrap();
    drop(ledger);
    drop(misses);
    drop(db);
    let space = app
        .load_space_with("published_v011_fixture", false, false)
        .await
        .unwrap();
    assert!(space.ledger.get("C:1").await.unwrap().is_none());
    assert!(space.ledger.get("C-999").await.unwrap().is_some());
    assert!(space.db.get_extension("memory_metrics").is_none());
    assert!(
        !space
            .miss_cache
            .is_fresh_miss("old search", unix_ms())
            .await
            .unwrap()
    );
    space
        .db
        .save_extension_from(
            "memory_metrics".into(),
            &crate::types::MemoryMetrics {
                recalls_completed: 7,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    super::reset_v1_bookkeeping(&space.db).await.unwrap();
    assert_eq!(
        space
            .db
            .get_extension_as::<crate::types::MemoryMetrics>("memory_metrics")
            .unwrap()
            .recalls_completed,
        7
    );
    space.close().await.unwrap();
}

#[tokio::test]
async fn usage_ledger_counts_recalls_and_corrections_without_touching_the_graph() {
    let app = test_app_state("usage_ledger");
    let space = create_loaded_space(&app, "usage_ledger").await;

    let entities = std::collections::BTreeSet::from(["P:1:prefers".to_string(), "C:9".to_string()]);
    space.ledger.record_recall(&entities, 100).await.unwrap();
    space
        .ledger
        .record_recall(
            &std::collections::BTreeSet::from(["P:1:prefers".to_string()]),
            200,
        )
        .await
        .unwrap();

    let row = space.ledger.get("P:1:prefers").await.unwrap().unwrap();
    assert_eq!(row.recall_count, 2);
    assert_eq!(row.last_recalled_at, 200);
    assert_eq!(
        space.ledger.get("C:9").await.unwrap().unwrap().recall_count,
        1
    );

    // Corrections record once per entity.
    assert!(
        space
            .ledger
            .record_correction("P:1:prefers", 300)
            .await
            .unwrap()
    );
    assert!(
        !space
            .ledger
            .record_correction("P:1:prefers", 400)
            .await
            .unwrap()
    );
    let row = space.ledger.get("P:1:prefers").await.unwrap().unwrap();
    assert_eq!(row.correction_count, 1);
    assert_eq!(row.last_corrected_at, 300);

    // The counts stay in the ledger and reach the graph through nothing:
    // the entities recalled above carry no `MnemonicState` at all, which
    // is what "reading does not reinforce" has to look like from the
    // graph's side.
    assert_eq!(mnemonic_state(&space, "C:9").await, serde_json::Value::Null);
}

/// The gate is only worth anything if the engine really does tell the
/// tool which agent called it. `GuardedMemory` branches on
/// `BaseCtx::agent`, so this drives the real dispatch path — an agent
/// context, its `child_base` tool context — rather than the predicate,
/// which `kip.rs` already covers on its own.
///
/// Both halves, because the divergence this closes was a default-open
/// `else`: Formation was gated by name and every other agent fell through
/// to the raw tool. Maintenance being admitted where Formation is refused
/// proves the branch; Maintenance being refused where nobody is admitted
/// proves it is a branch and not a bypass.
#[tokio::test(flavor = "multi_thread")]
async fn each_writing_agent_reaches_its_own_half_of_the_gate() {
    use anda_core::Tool;
    use anda_engine::memory::KipArgs;

    let app = test_app_state("formation_gate");
    let space = create_loaded_space(&app, "formation_gate").await;
    let guarded = crate::agents::GuardedMemory::new(space.memory.clone());

    // Concepts to aim at, so a refusal cannot be confused with a miss.
    seed_kip(
        &space,
        kip::request(
            r#"MUTATE {
  UPSERT CONCEPT ?p { MATCH {type: "Person", key: "victim"} SET FIELDS {name: "Victim"} }
  UPSERT CONCEPT ?b { MATCH {type: "Person", key: "bystander"} SET FIELDS {name: "Bystander"} }
}"#,
        ),
    )
    .await;

    const ARCHIVE_VICTIM: &str = r#"TRANSITION ?c TO "archived"
WHERE { ?c CONCEPT {type: "Person", key: "victim"} } LIMIT 1"#;
    let archive = || KipArgs {
        command: Some(ARCHIVE_VICTIM.to_string()),
        ..Default::default()
    };

    // Formation: refused on what the command parses to.
    let ctx = space
        .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
        .unwrap();
    let out = guarded
        .call(ctx.child_base("execute_kip").unwrap(), archive(), vec![])
        .await
        .unwrap();
    assert_eq!(out.is_error, Some(true));
    let refusal = kip::error_message(&out.output);
    assert!(refusal.contains("archived"), "{refusal}");

    // Formation's own writes still go through the same tool.
    let write = guarded
            .call(
                ctx.child_base("execute_kip").unwrap(),
                KipArgs {
                    command: Some(
                        r#"MUTATE { CREATE ACTIVITY ?a { SET FIELDS { activity_class: "extraction", status: "completed" } } }"#
                            .to_string(),
                    ),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap();
    assert_eq!(write.is_error, None, "{:?}", write.output);

    // Maintenance: the same tool, the same command, admitted.
    let ctx = space
        .ctx_for_test(SELF_USER_ID, MaintenanceAgent::NAME)
        .unwrap();
    let out = guarded
        .call(ctx.child_base("execute_kip").unwrap(), archive(), vec![])
        .await
        .unwrap();
    assert_eq!(out.is_error, None, "{:?}", out.output);
    assert_eq!(
        kip::transitioned(&out.output, "archived"),
        1,
        "{:?}",
        out.output
    );

    // ... and still refused the two things no plan gets: erasure, and a
    // hold that would block somebody else's. Aimed at a Concept the
    // archive above did not touch, so the survival check below reads a
    // live element rather than an archived one.
    for (command, expected) in [
        (
            r#"PURGE ?c WHERE { ?c CONCEPT {type: "Person", key: "bystander"} } LIMIT 1 CONFIRM "PURGE""#,
            "PURGE",
        ),
        (
            r#"SET RETENTION ?c { retention_class: "standard", legal_hold: true } WHERE { ?c CONCEPT {type: "Person", key: "bystander"} } LIMIT 1"#,
            "legal hold",
        ),
    ] {
        let out = guarded
            .call(
                ctx.child_base("execute_kip").unwrap(),
                KipArgs {
                    command: Some(command.to_string()),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true), "{command}");
        let refusal = kip::error_message(&out.output);
        assert!(refusal.contains(expected), "{command}: {refusal}");
    }

    // The Concept the purge aimed at is still there, so the refusal was a
    // refusal and not a failed erasure.
    let survivor = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person", key: "bystander"} } LIMIT 1"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        kip::ok_result(&survivor)
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1),
        "{survivor:?}"
    );
}

/// The runtime's copy of what was said is the copy that gets stored.
///
/// Spec §71.1 exists because a model retyping an observation into a
/// `payload` truncates it, normalizes its whitespace, fixes its spelling or
/// paraphrases it, and the record then says the source said something they
/// did not (§88.12). The command below never contains the sentence — it
/// cites `:msg1` — so finding the sentence verbatim proves it did not pass
/// through model-generated text on the way in.
#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_mints_the_observation_the_model_only_cites() {
    use anda_core::Tool;
    use anda_engine::memory::KipArgs;

    let app = test_app_state("formation_ingest");
    let space = create_loaded_space(&app, "formation_ingest").await;
    let guarded = crate::agents::GuardedMemory::new(space.memory.clone());

    let said = "Please keep answers concise — I mean it, ≤ 3 sentences.";
    let messages = vec![anda_core::Message {
        role: "user".to_string(),
        content: vec![said.to_string().into()],
        ..Default::default()
    }];
    let observation =
        kip::observation_ingest(&messages, "2026-08-20T00:00:00Z", "formation:chat-42", None)
            .expect("one message, one entry");

    let ctx = space
        .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
        .unwrap();
    ctx.base
        .set_state(crate::agents::Observation(Some(Arc::new(observation))));

    let plan = || KipArgs {
        command: Some(
            r#"MUTATE {
  UPSERT CONCEPT ?alice { MATCH {type: "Person", key: "alice"} SET FIELDS {name: "Alice"} }
  CREATE CONCEPT ?concise {
    TYPE "Preference"
    NAME "Alice concise answers"
    SET ATTRIBUTES {preference_class: "communication"}
  }
  ASSERT ?a (?alice, "prefers", ?concise) {
    by: ?alice, mode: "stated", confidence: 0.95, evidence: :msg1
  }
}"#
            .to_string(),
        ),
        ..Default::default()
    };
    let written = guarded
        .call(ctx.child_base("execute_kip").unwrap(), plan(), vec![])
        .await
        .unwrap();
    assert_eq!(written.is_error, None, "{:?}", written.output);

    let stored = |space: Arc<Space>| async move {
        let response = space
            .execute_kip_readonly(kip::request(
                "FIND(?e.payload, ?e.evidence_class, ?e.observed_at) WHERE { ?e EVIDENCE {} } \
                     LIMIT 5",
            ))
            .await
            .unwrap();
        kip::ok_result(&response)
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let rows = stored(space.clone()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0],
        serde_json::json!([
            // The whole message, role included — who said a thing is part
            // of what was observed — and byte for byte, em dash and `≤`
            // intact.
            {"mode": "inline", "inline": serde_json::to_value(&messages[0]).unwrap()},
            // From the speaker's role, never from anything a model chose.
            "user_statement",
            "2026-08-20T00:00:00.000Z",
        ])
    );

    // The same pass writing again resolves to the record it already minted
    // rather than observing the same sentence twice: the `client_key` is
    // what makes attaching this to every request in a multi-turn formation
    // safe (§52.1).
    let again = guarded
        .call(ctx.child_base("execute_kip").unwrap(), plan(), vec![])
        .await
        .unwrap();
    assert_eq!(again.is_error, None, "{:?}", again.output);
    assert_eq!(stored(space.clone()).await.len(), 1);
}

/// The 2.1 candidate has immutable behavior; descriptive feedback is no grade.
#[tokio::test]
async fn skill_candidates_keep_their_revision_and_unproven_standing() {
    let app = test_app_state("revision_contract");
    let space = create_loaded_space(&app, "revision_contract").await;
    let attributes =
        serde_json::json!({"task_family":"deploy", "procedure":"verify before deploying"});
    let digest = anda_cognitive_nexus::content_digest(&attributes).unwrap();
    seed_kip(&space, kip::request_with(r#"MUTATE {
      CREATE CONCEPT ?skill { TYPE "Skill" SET ATTRIBUTES {skill_class:"workflow",summary:"verify",status:"proposed"} SET STRUCTURAL {("current_revision",?revision)} }
      CREATE CONCEPT ?revision { TYPE "SkillRevision" SET ATTRIBUTES {task_family:"deploy",procedure:"verify before deploying",behavior_digest: :digest} SET STRUCTURAL {("revision_of",?skill)} }
    }"#, kip::param("digest",digest))).await;
    for _ in 0..8 {
        seed_kip(&space, kip::request(r#"CREATE EVIDENCE ?e { SET FIELDS {evidence_class:"agent_statement",payload:"deployment succeeded"} }"#)).await;
    }
    let report = space
        .settle_memory_metabolism(MaintenanceScope::Quick, unix_ms())
        .await
        .unwrap();
    assert_eq!(report.skills.transitions, 0);
    assert!(report.skills.unsupported_reason.is_some());
    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?s.attributes.status) WHERE { ?s CONCEPT {type:"Skill"} } LIMIT 10"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        kip::ok_result(&response).unwrap(),
        &serde_json::json!(["proposed"])
    );
    let refused = space.run_kip_settlement(kip::request(r#"UPDATE ?r SET ATTRIBUTES {procedure:"skip verification"} WHERE { ?r CONCEPT {type:"SkillRevision"} } LIMIT 1"#)).await.unwrap();
    assert!(!kip::succeeded(&refused));
}

async fn created_ref(
    space: &Space,
    command: &str,
    parameters: serde_json::Map<String, serde_json::Value>,
) -> String {
    let response = space
        .run_kip_settlement(kip::request_with(command, parameters))
        .await
        .unwrap();
    assert!(
        kip::succeeded(&response),
        "{}",
        kip::error_message(&response)
    );
    kip::ok_result(&response).unwrap()["handles"]["item"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn element_version(space: &Space, id: &str) -> u64 {
    let response = space
        .execute_kip_readonly(kip::request_with(
            "FIND(?c._system.version) WHERE { ?c CONCEPT {id: :id} } LIMIT 1",
            kip::param("id", id),
        ))
        .await
        .unwrap();
    kip::ok_result(&response).unwrap()[0].as_u64().unwrap()
}

async fn runtime_work(space: &Space, operation: &str, id: &str) -> serde_json::Value {
    crate::cognitive::MemoryRuntimeTool::new(space.memory.clone())
        .execute(
            crate::cognitive::RuntimeArgs {
                operation: operation.into(),
                target_ref: Some(id.into()),
                expected_version: Some(element_version(space, id).await),
                content: None,
            },
            true,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn watches_use_nexus_coverage_and_never_infer_text_consumption() {
    let app = test_app_state("watch_contract");
    let space = create_loaded_space(&app, "watch_contract").await;
    let target = created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "vendor"}"#,
        Default::default(),
    )
    .await;
    let mut ids = Vec::new();
    for (class, condition) in [
        (
            "delta",
            serde_json::json!({"element":target,"ops":["update"]}),
        ),
        (
            "silence",
            serde_json::json!({"element":target,"ops":["create"]}),
        ),
        ("silence", serde_json::json!("no vendor reply")),
        (
            "silence",
            serde_json::json!({"element":target,"text":"no vendor reply"}),
        ),
    ] {
        let id = created_ref(&space, r#"CREATE CONCEPT ?item { TYPE "Watch" SET ATTRIBUTES {watch_class: :class,summary:"wait",status:"disarmed",condition: :condition,due_at:"2020-01-01T00:00:00Z"} }"#,
            serde_json::Map::from_iter([("class".into(),serde_json::json!(class)),("condition".into(),condition)])).await;
        runtime_work(&space, "arm_watch", &id).await;
        ids.push(id);
    }
    let stale = element_version(&space, &ids[0]).await;
    seed_kip(
        &space,
        kip::request_with(
            "UPDATE :id SET FIELDS {name: \"vendor changed\"}",
            kip::param("id", target),
        ),
    )
    .await;
    let first = settlement::sweep_watches(space.as_ref()).await;
    assert_eq!(
        (first.fired, first.deferred, first.conflicted),
        (2, 2, 0),
        "{first:?}"
    );
    let next = settlement::sweep_watches(space.as_ref()).await;
    assert_eq!((next.fired, next.deferred), (0, 2));
    let assessment = space.maintenance_assessment(None).await;
    assert!(assessment.armed_watches.iter().all(|watch| {
        watch.schema_ref == "kip://profiles/cognitive-memory@2.1.0/Watch" && watch.version.is_some()
    }));
    let nexus = space.memory.nexus();
    let error = nexus
        .system_session()
        .advance_watch(
            anda_cognitive_nexus::nexus::DEFAULT_SPACE,
            &ids[0],
            stale,
            1,
            200,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, anda_kip::KipErrorCode::VersionConflict);
}

#[tokio::test]
async fn deferred_text_watches_do_not_starve_structured_work() {
    let app = test_app_state("watch_fairness");
    let space = create_loaded_space(&app, "watch_fairness").await;
    let target = created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "vendor"}"#,
        Default::default(),
    )
    .await;
    for index in 0..22 {
        let condition = if index == 21 {
            serde_json::json!({"element":target,"ops":["update"]})
        } else {
            serde_json::json!(format!("no reply {index}"))
        };
        let id = created_ref(&space,r#"CREATE CONCEPT ?item {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"wait",status:"disarmed",condition: :condition}}"#,kip::param("condition",condition)).await;
        runtime_work(&space, "arm_watch", &id).await;
    }
    seed_kip(
        &space,
        kip::request_with(
            r#"UPDATE :id SET FIELDS {name:"vendor changed"}"#,
            kip::param("id", target),
        ),
    )
    .await;
    let report = settlement::sweep_watches(space.as_ref()).await;
    assert_eq!(report.fired, 1, "{report:?}");
    assert!(report.error.is_none(), "{report:?}");
}

#[tokio::test]
async fn task_completion_requires_a_live_lease_and_current_version() {
    let app = test_app_state("lease_contract");
    let space = create_loaded_space(&app, "lease_contract").await;
    let task = created_ref(&space,r#"CREATE CONCEPT ?item { TYPE "SleepTask" SET ATTRIBUTES {task_class:"consolidate",summary:"review",status:"pending"} }"#, Default::default()).await;
    created_ref(
        &space,
        r#"CREATE CONCEPT ?item {TYPE "Person" NAME "memory"}"#,
        Default::default(),
    )
    .await;
    let settlement = space
        .settle_memory_metabolism(MaintenanceScope::Quick, unix_ms())
        .await
        .unwrap();
    assert!(settlement.decay_error.is_none(), "{settlement:?}");
    assert!(settlement.decayed > 0);
    assert_eq!(element_version(&space, &task).await, 1);
    let early = space
        .run_kip_settlement(kip::request_with(
            r#"UPDATE :id SET ATTRIBUTES {status:"completed"} EXPECT VERSION 1"#,
            kip::param("id", task.clone()),
        ))
        .await
        .unwrap();
    assert!(!kip::succeeded(&early));
    let lease = runtime_work(&space, "lease_task", &task).await;
    assert_eq!(lease["lease"]["fencing_token"], 1);
    let version = element_version(&space, &task).await;
    seed_kip(&space,kip::request_with(format!(r#"MUTATE {{ UPDATE :id SET ATTRIBUTES {{status:"completed"}} EXPECT VERSION {version} CREATE CONCEPT ?output {{ TYPE "Event" NAME "review completed" SET ATTRIBUTES {{summary:"review completed"}} }} }}"#),kip::param("id",task))).await;
}

/// Maintenance was reachable only by counting formation conversations, so
/// a Space that stopped ingesting stopped metabolizing: no Commitment
/// review, no retention expiry, no self-test. The reference policy's
/// triggers are "scheduled, threshold, or change-driven".
#[tokio::test]
async fn maintenance_comes_due_on_the_clock_not_only_on_traffic() {
    let app = test_app_state("maintenance_clock");
    let space = create_loaded_space(&app, "maintenance_clock").await;
    let now_ms = unix_ms();

    // A Space with nothing in it is never overdue: firing a cycle at every
    // freshly created Space would spend a model call to learn there is
    // nothing to consolidate.
    assert!(!space.maintenance_overdue(now_ms));

    // Formed something, never maintained: due now, whatever the count of
    // conversations says.
    space.formation.set_processed_for_test(1).await;
    assert!(space.maintenance_overdue(now_ms));

    // Maintained just now: not due again until the interval passes.
    space.maintenance.set_start_at(now_ms).await.unwrap();
    assert!(!space.maintenance_overdue(now_ms));
    assert!(!space.maintenance_overdue(now_ms + MAINTENANCE_MAX_INTERVAL_MS - 1));
    assert!(space.maintenance_overdue(now_ms + MAINTENANCE_MAX_INTERVAL_MS));
}

async fn seed_kip(space: &Space, request: anda_kip::Request) {
    let response = space.run_kip_settlement(request).await.unwrap();
    assert!(
        kip::succeeded(&response),
        "seed failed: {}",
        kip::error_message(&response)
    );
}

/// One Concept's `MnemonicState`, or `Json::Null` when it has none.
async fn mnemonic_state(space: &Space, id: &str) -> serde_json::Value {
    let response = space
        .execute_kip_readonly(kip::request_with(
            r#"FIND(?c.facets["MnemonicState"]) WHERE { ?c {id: :id} } LIMIT 1"#,
            kip::param("id", id),
        ))
        .await
        .unwrap();
    kip::ok_result(&response)
        .and_then(|result| result.as_array())
        .and_then(|rows| rows.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// True when the element still exists.
async fn element_exists(space: &Space, id: &str) -> bool {
    let command = if crate::assess::is_concept_entity_id(id) {
        "FIND(?c) WHERE { ?c {id: :id} } LIMIT 1"
    } else {
        "FIND(?p) WHERE { ?p (id: :id) } LIMIT 1"
    };
    let response = space
        .execute_kip_readonly(kip::request_with(command, kip::param("id", id)))
        .await
        .unwrap();
    kip::ok_result(&response)
        .and_then(|result| result.as_array())
        .is_some_and(|rows| !rows.is_empty())
}

/// Seeds three Person Concepts and two `prefers` claims from `alpha`,
/// each at `memory_strength` 0.8. Returns `(concept ids, proposition ids)`,
/// both sorted, with the Concepts in `alpha, beta, gamma` order.
///
/// `Person` and `prefers` come from the Cognitive Memory Profile the space
/// activates at open: KIP 2.0 resolves every symbol through the Schema
/// Environment, so a test cannot invent a `Topic` type on the way in the way
/// the 1.x fixtures did.
async fn seed_people(space: &Space) -> (Vec<String>, Vec<String>) {
    seed_kip(
        space,
        kip::request(
            r#"MUTATE {
  UPSERT CONCEPT ?alpha { MATCH {type: "Person", key: "alpha"} SET FIELDS {name: "alpha"}
                          SET FACET "MnemonicState" {memory_strength: 0.8, salience: 0.5} }
  UPSERT CONCEPT ?beta  { MATCH {type: "Person", key: "beta"}  SET FIELDS {name: "beta"}
                          SET FACET "MnemonicState" {memory_strength: 0.8, salience: 0.5} }
  UPSERT CONCEPT ?gamma { MATCH {type: "Person", key: "gamma"} SET FIELDS {name: "gamma"}
                          SET FACET "MnemonicState" {memory_strength: 0.8, salience: 0.5} }
  ASSERT ?ab (?alpha, "prefers", ?beta) { by: ?alpha, mode: "stated", confidence: 0.8 }
  ASSERT ?ag (?alpha, "prefers", ?gamma) { by: ?alpha, mode: "stated", confidence: 0.8 }
}"#,
        ),
    )
    .await;

    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person"} } ORDER BY ?c.name"#,
        ))
        .await
        .unwrap();
    let concepts: Vec<String> =
        serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
    assert_eq!(concepts.len(), 3, "{response:?}");

    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?p.id) WHERE { ?p (?s, "prefers", ?o) }"#,
        ))
        .await
        .unwrap();
    let mut propositions: Vec<String> =
        serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
    propositions.sort();
    assert_eq!(propositions.len(), 2, "{response:?}");
    (concepts, propositions)
}

/// The id of the Assertion about one exact `(subject, "prefers", object)`.
async fn assertion_about(space: &Space, subject: &str, object: &str) -> String {
    let response = space
        .execute_kip_readonly(kip::request_with(
            r#"FIND(?a.id) WHERE {
  ?s CONCEPT {id: :subject}
  ?o CONCEPT {id: :object}
  ?p (?s, "prefers", ?o)
  ?a ASSERTION {proposition: ?p}
} LIMIT 1"#,
            serde_json::Map::from_iter([
                ("subject".to_string(), serde_json::Value::from(subject)),
                ("object".to_string(), serde_json::Value::from(object)),
            ]),
        ))
        .await
        .unwrap();
    serde_json::from_value::<Vec<String>>(kip::ok_result(&response).cloned().unwrap_or_default())
        .unwrap_or_default()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no assertion about ({subject}, prefers, {object})"))
}

fn strength(state: &serde_json::Value) -> f64 {
    state["memory_strength"]
        .as_f64()
        .unwrap_or_else(|| panic!("no memory_strength in {state}"))
}

#[tokio::test]
async fn settlement_metabolizes_every_memory_and_reinforces_none() {
    let app = test_app_state("settlement");
    let space = create_loaded_space(&app, "settlement").await;
    let now_ms = unix_ms();
    let (concepts, _) = seed_people(&space).await;
    let (alpha, beta, gamma) = (
        concepts[0].clone(),
        concepts[1].clone(),
        concepts[2].clone(),
    );

    // One Concept was surfaced by a recall; the others never were. Under
    // the reference Recall policy that must make no difference to the
    // graph: reading is observed, never rewarded (§1, §32, invariant 2).
    space
        .ledger
        .record_recall(&BTreeSet::from([beta.clone()]), now_ms)
        .await
        .unwrap();

    let report = space
        .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
        .await
        .unwrap();
    assert!(report.decay_ran);
    assert_eq!(report.decayed, 3, "{report:?}");
    assert_eq!(report.new_corrections, 0);

    // All three decayed by the policy factor (0.8 × 0.95), the recalled
    // one included. It earned no gain and bought no exemption — the
    // ledger row exists, and the graph does not know about it.
    for id in [&alpha, &beta, &gamma] {
        let state = mnemonic_state(&space, id).await;
        assert!((strength(&state) - 0.76).abs() < 1e-9, "{id}: {state}");
    }
    assert_eq!(
        space.ledger.get(&beta).await.unwrap().unwrap().recall_count,
        1,
        "the recall is still recorded, just not paid out"
    );

    // Idempotence: an immediate re-settlement does not re-decay (weekly
    // rate limit).
    let report = space
        .settle_memory_metabolism(MaintenanceScope::Full, now_ms + 1)
        .await
        .unwrap();
    assert_eq!(report.decayed, 0, "{report:?}");

    // Nothing decayed the *claims*: KIP 2.0 forbids letting time erode a
    // stance, and a settlement that quietly did would be the single
    // easiest way for this brain to start lying slowly.
    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?a.confidence) WHERE { ?a ASSERTION {} }"#,
        ))
        .await
        .unwrap();
    let confidences: Vec<f64> =
        serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
    assert_eq!(confidences.len(), 2, "{response:?}");
    assert!(
        confidences.iter().all(|c| (c - 0.8).abs() < 1e-9),
        "{confidences:?}"
    );
}

#[tokio::test]
async fn settlement_records_an_actors_own_revision_as_a_correction() {
    let app = test_app_state("corrections");
    let space = create_loaded_space(&app, "corrections").await;
    let now_ms = unix_ms();
    let (concepts, _) = seed_people(&space).await;
    let alpha = concepts[0].clone();

    // The asserting actor revises their own claim. In KIP 1.x this was a
    // `superseded: true` flag on the link; here it is a new Assertion plus
    // a supersession, and the old one keeps saying what it said.
    // Pick the Assertion about one exact tuple: a supersession must stay
    // inside one Proposition, and the engine refuses one that wanders.
    let old = assertion_about(&space, &alpha, &concepts[2]).await;

    // A mutation endpoint is a handle, a parameter or a literal — this
    // engine does not run the KQL solver on the write path — so the
    // endpoints arrive as element references.
    seed_kip(
            &space,
            kip::request_with(
                r#"ASSERT ?new (:alpha, "prefers", :gamma) { by: :alpha, mode: "stated", confidence: 0.4 }
SUPERSEDING :old"#,
                serde_json::Map::from_iter([
                    ("alpha".to_string(), serde_json::json!({"id": alpha})),
                    (
                        "gamma".to_string(),
                        serde_json::json!({"id": concepts[2]}),
                    ),
                    ("old".to_string(), serde_json::Value::from(old.as_str())),
                ]),
            ),
        )
        .await;

    let report = space
        .settle_memory_metabolism(MaintenanceScope::Quick, now_ms)
        .await
        .unwrap();
    // Disuse metabolism is paced by `DECAY_MIN_INTERVAL_MS`, not by the
    // cycle scope: a `quick` cycle sweeps too. These Concepts have never
    // carried `MnemonicState`, so they metabolize from the baseline rather
    // than being skipped — "the model forgot to set MnemonicState" must
    // not mean "this memory never fades".
    assert!(report.decay_ran);
    assert!(report.decayed > 0, "{report:?}");
    assert_eq!(report.new_corrections, 1, "{report:?}");
    let row = space.ledger.get(&old).await.unwrap().unwrap();
    assert_eq!(row.correction_count, 1);

    // The actor whose claim needed revising is charged, not the caller:
    // attribution is cognition, authority is Governance.
    let reliability: std::collections::BTreeMap<String, crate::types::SourceReliability> =
        space.db.get_extension_as("source_reliability").unwrap();
    assert_eq!(reliability[&alpha].corrections, 1, "{reliability:?}");

    // The sequence watermark, not a graph flag, is the scan cursor: the
    // processed revision falls behind it and the next pass finds nothing.
    let report = space
        .settle_memory_metabolism(MaintenanceScope::Quick, now_ms + 1)
        .await
        .unwrap();
    assert_eq!(report.new_corrections, 0, "{report:?}");

    // The settlement report is persisted for observability.
    assert!(space.memory_settlement().is_some());
}

#[tokio::test]
async fn probe_memory_uses_negative_knowledge_cache() {
    let app = test_app_state("probe_memory");
    let space = create_loaded_space(&app, "probe_memory").await;
    seed_people(&space).await;

    let hit = space.probe_memory("alpha", None).await.unwrap();
    assert!(hit.found, "{hit:?}");
    assert!(!hit.negative_cached);
    assert!(
        hit.hits
            .iter()
            .any(|citation| citation.name.as_deref() == Some("alpha"))
    );

    let miss = space
        .probe_memory("qqqzzzxxx nonsense", None)
        .await
        .unwrap();
    assert!(!miss.found);
    assert!(!miss.negative_cached);

    // The second identical miss is answered from the cache.
    let cached = space
        .probe_memory("qqqzzzxxx nonsense", None)
        .await
        .unwrap();
    assert!(!cached.found);
    assert!(cached.negative_cached);

    // Formation completion clears negative knowledge (hook calls this).
    space.miss_cache.clear().await.unwrap();
    let fresh = space
        .probe_memory("qqqzzzxxx nonsense", None)
        .await
        .unwrap();
    assert!(!fresh.negative_cached);

    // Oversized queries are never cached (unauthenticated probes on
    // public spaces must not be a disk-write amplifier): the identical
    // repeat still misses without a cache hit.
    let long_query = format!("qqqzzzxxx {}", "x".repeat(600));
    let miss = space.probe_memory(&long_query, None).await.unwrap();
    assert!(!miss.found);
    let repeat = space.probe_memory(&long_query, None).await.unwrap();
    assert!(!repeat.negative_cached);
}

/// A full settlement honours what retention wrote, and keeps the record
/// clock apart from the claim clock.
#[tokio::test]
async fn settlement_expires_lapsed_records_and_claims() {
    let app = test_app_state("retention_sweep");
    let space = create_loaded_space(&app, "retention_sweep").await;
    let now_ms = unix_ms();
    let (concepts, _) = seed_people(&space).await;

    // One record whose retention lapsed, one whose claim's window closed,
    // and one held. `expires_at` and `valid_time.until` are two different
    // clocks, and the sweep must not confuse them.
    seed_kip(
        &space,
        kip::request_with(
            r#"MUTATE {
  SET RETENTION :lapsed { expires_at: "2020-01-01T00:00:00Z" }
  SET RETENTION :held { expires_at: "2020-01-01T00:00:00Z", legal_hold: true }
}"#,
            serde_json::Map::from_iter([
                ("lapsed".to_string(), concepts[0].clone().into()),
                ("held".to_string(), concepts[1].clone().into()),
            ]),
        ),
    )
    .await;

    // The claim is created with its window already closed rather than
    // edited into one: an Assertion's epistemic payload is immutable, and
    // a changed commitment is a new Assertion, not a rewrite.
    seed_kip(
        &space,
        kip::request(
            r#"MUTATE {
  UPSERT CONCEPT ?alpha { MATCH {type: "Person", key: "alpha"} }
  UPSERT CONCEPT ?gamma { MATCH {type: "Person", key: "gamma"} }
  ASSERT ?lapsed (?alpha, "prefers", ?gamma) {
    by: ?alpha, mode: "stated", confidence: 0.8,
    valid: {from: "2019-01-01T00:00:00Z", until: "2020-01-01T00:00:00Z"}
  }
}"#,
        ),
    )
    .await;

    // Quick scope does not sweep: forgetting is a full-cycle decision.
    let quick = space
        .settle_memory_metabolism(MaintenanceScope::Quick, now_ms)
        .await
        .unwrap();
    assert_eq!(quick.retention.archived, 0, "{quick:?}");
    assert_eq!(quick.retention.expired_assertions, 0, "{quick:?}");

    let report = space
        .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
        .await
        .unwrap();
    assert_eq!(report.retention.error, None, "{report:?}");
    // §163: the hold blocks the sweep that authorized it, and is counted
    // rather than dropped — "archived 1" when 2 lapsed is not the truth.
    assert_eq!(report.retention.archived, 1, "{report:?}");
    assert_eq!(report.retention.held, 1, "{report:?}");
    assert!(report.retention.expired_assertions >= 1, "{report:?}");

    // Archived, not destroyed: the element is still there to be read.
    let still_there = space
        .execute_kip_readonly(kip::request_with(
            "FIND(?c) WHERE { ?c CONCEPT {id: :id} } LIMIT 1",
            kip::param("id", concepts[0].as_str()),
        ))
        .await
        .unwrap();
    assert!(kip::succeeded(&still_there), "{still_there:?}");

    // §14.3: the lapsed claim is `expired` — not retracted and not
    // superseded, because nobody withdrew it and nothing replaced it.
    let status = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?a.lifecycle.status) WHERE {
  ?a ASSERTION {}
  FILTER(?a.lifecycle.status == "expired")
} LIMIT 5"#,
        ))
        .await
        .unwrap();
    assert!(
        kip::ok_result(&status)
            .and_then(|value| value.as_array())
            .is_some_and(|rows| !rows.is_empty()),
        "{status:?}"
    );

    // Idempotent: a second full cycle finds nothing left to act on.
    let again = space
        .settle_memory_metabolism(MaintenanceScope::Full, now_ms + 1)
        .await
        .unwrap();
    assert_eq!(again.retention.archived, 0, "{again:?}");
    assert_eq!(again.retention.expired_assertions, 0, "{again:?}");
    assert_eq!(again.retention.held, 1, "{again:?}");
}

#[tokio::test]
async fn pin_exempts_from_metabolism_and_forget_removes_for_real() {
    let app = test_app_state("pin_forget");
    let space = create_loaded_space(&app, "pin_forget").await;
    let now_ms = unix_ms();
    let (concepts, propositions) = seed_people(&space).await;
    let (alpha, pinned, plain) = (
        concepts[0].clone(),
        concepts[1].clone(),
        concepts[2].clone(),
    );

    // Pin one Concept: metabolism must skip it (plan M6 + M2 integration).
    assert_eq!(space.pin_memory(&pinned, true).await.unwrap(), 1);
    let report = space
        .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
        .await
        .unwrap();
    assert_eq!(report.decayed, 2, "{report:?}");
    let state = mnemonic_state(&space, &pinned).await;
    assert!((strength(&state) - 0.8).abs() < 1e-9, "{state}");
    assert!((strength(&mnemonic_state(&space, &plain).await) - 0.76).abs() < 1e-9);

    // Dry run reports without erasing.
    let doomed = propositions[0].clone();
    let report = space
        .forget_memory(crate::types::MemoryForgetInput {
            entities: vec![doomed.clone()],
            dry_run: true,
        })
        .await
        .unwrap();
    assert!(report.dry_run);
    assert!(report.entities[0].existed);
    assert_eq!(report.deleted_propositions, 0);
    assert!(element_exists(&space, &doomed).await);

    // A real forget purges the Proposition and its ledger row, and reports
    // a bogus id per entity without aborting the batch.
    space
        .ledger
        .record_recall(&BTreeSet::from([doomed.clone()]), now_ms)
        .await
        .unwrap();
    let report = space
        .forget_memory(crate::types::MemoryForgetInput {
            entities: vec![doomed.clone(), "bogus".to_string()],
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.deleted_propositions, 1, "{report:?}");
    assert!(
        report
            .entities
            .iter()
            .any(|entry| entry.entity == "bogus" && entry.error.is_some())
    );
    assert!(space.ledger.get(&doomed).await.unwrap().is_none());

    // Forgetting a Concept cascades to the Propositions that quote it —
    // and to their ledger rows: usage traces of a forgotten memory must
    // not survive the memory.
    let survivor = propositions[1].clone();
    space
        .ledger
        .record_recall(&BTreeSet::from([survivor.clone()]), now_ms)
        .await
        .unwrap();
    let report = space
        .forget_memory(crate::types::MemoryForgetInput {
            entities: vec![alpha.clone()],
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.deleted_concepts, 1, "{report:?}");
    assert!(report.deleted_propositions >= 1, "{report:?}");
    assert!(
        space.ledger.get(&survivor).await.unwrap().is_none(),
        "a cascaded proposition must lose its ledger row"
    );
}

#[tokio::test]
async fn memory_self_test_flags_unfindable_memories() {
    let app = test_app_state_with_self_test_model("memory_self_test");
    let space = create_loaded_space(&app, "memory_self_test").await;
    let now_ms = unix_ms();
    let (_, propositions) = seed_people(&space).await;

    let report = space
        .run_memory_self_test(now_ms)
        .await
        .unwrap()
        .expect("self-test must run");
    assert_eq!(report.tested, 2, "{report:?}");
    assert_eq!(report.grounded, 1, "{report:?}");
    assert_eq!(report.reencode_tasks, 1, "{report:?}");
    assert_eq!(report.groundability(), Some(0.5));

    // The ungroundable memory produced one pending SleepTask about its
    // subject Concept.
    let response = space
            .execute_kip_readonly(kip::request(
                r#"FIND(?task) WHERE { ?task CONCEPT {type: "SleepTask"} FILTER(?task.attributes.status == "pending") } LIMIT 10"#,
            ))
            .await
            .unwrap();
    let tasks = kip::ok_result(&response)
        .map(crate::assess::citations_from_json)
        .unwrap_or_default();
    assert_eq!(tasks.len(), 1, "{response:?}");

    // Guardrail: self-tests count only into self_test_count — never into
    // usage reinforcement.
    for id in &propositions {
        let row = space.ledger.get(id).await.unwrap().unwrap();
        assert_eq!(row.self_test_count, 1);
        assert_eq!(row.recall_count, 0);
        assert_eq!(row.last_recalled_at, 0);
    }

    // Every candidate was already tested: the next pass has nothing to do.
    assert!(
        space
            .run_memory_self_test(now_ms + 1)
            .await
            .unwrap()
            .is_none()
    );

    // The exclusion is the sequence cursor, not the ledger. KIP 1.x stamped
    // `self_tested_at` on the link; a Proposition is immutable, so coverage
    // now slides on `_system.space_seq` — and still holds with the ledger
    // rows gone.
    for id in &propositions {
        space.ledger.forget_entity(id).await.unwrap();
    }
    assert!(
        space
            .run_memory_self_test(now_ms + 2)
            .await
            .unwrap()
            .is_none()
    );

    // A newly formed memory enters the window on the next pass.
    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Person"} } ORDER BY ?c.name"#,
        ))
        .await
        .unwrap();
    let people: Vec<String> =
        serde_json::from_value(kip::ok_result(&response).cloned().unwrap()).unwrap();
    seed_kip(
        &space,
        kip::request_with(
            r#"ASSERT (:beta, "prefers", :gamma) { by: :beta, mode: "stated", confidence: 0.8 }"#,
            serde_json::Map::from_iter([
                ("beta".to_string(), serde_json::json!({"id": people[1]})),
                ("gamma".to_string(), serde_json::json!({"id": people[2]})),
            ]),
        ),
    )
    .await;
    let report = space
        .run_memory_self_test(now_ms + 3)
        .await
        .unwrap()
        .expect("new memory must be sampled");
    assert_eq!(report.tested, 1, "{report:?}");

    // The report persists and surfaces as the groundability graph stat.
    let stored: crate::types::SelfTestReport = space
        .db
        .get_extension_as("memory_self_test")
        .expect("report stored");
    assert_eq!(stored.groundability(), Some(1.0));
}

#[derive(Debug)]
struct JudgeCompleter;

impl CompletionFeaturesDyn for JudgeCompleter {
    fn model_name(&self) -> String {
        "judge-test-model".to_string()
    }

    fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(async move {
            Ok(AgentOutput {
                content: "judge verdict".to_string(),
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn judge_complete_routes_to_independent_model() {
    use crate::assess::AssessContext;
    let app = test_app_state_with_final_model("judge_route");
    let space = create_loaded_space(&app, "judge_route").await;
    let request = || CompletionRequest {
        prompt: "judge this".to_string(),
        ..Default::default()
    };

    // Without a judge model, judge completions share the space model.
    let out = AssessContext::judge_complete(space.as_ref(), request())
        .await
        .unwrap();
    assert_eq!(out.content, "done");

    space.set_judge_model_for_test(Model::with_completer(Arc::new(JudgeCompleter)));
    let out = AssessContext::judge_complete(space.as_ref(), request())
        .await
        .unwrap();
    assert_eq!(out.content, "judge verdict");

    // Non-judge completions (simulator, optimizer) keep the space model.
    let out = AssessContext::complete(space.as_ref(), request())
        .await
        .unwrap();
    assert_eq!(out.content, "done");
}

/// Answers the scenario-mining call with a fixed valid scenario that
/// deliberately contains PII the miner must scrub.
#[derive(Debug)]
struct MinerCompleter;

impl CompletionFeaturesDyn for MinerCompleter {
    fn model_name(&self) -> String {
        "miner-test-model".to_string()
    }

    fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(async move {
            let scenario = serde_json::json!({
                "scenario": {
                    "id": "pref_fix",
                    "hidden_profile": {"contact": "work address"},
                    "timeline": [
                        {"turn": 1, "type": "normal",
                         "timestamp": "2026-06-01T10:00:00Z",
                         "user": "My email is bob@example.com and card 12345678901."},
                        {"turn": 2, "type": "normal",
                         "timestamp": "2026-06-05T10:00:00Z",
                         "user": "Correction: use my work address instead."},
                        {"turn": 3, "type": "maintenance",
                         "maintenance": {"trigger": "on_demand", "scope": "quick"}},
                        {"turn": 4, "type": "checkpoint_synthetic",
                         "timestamp": "2026-06-06T10:00:00Z",
                         "query": "Which contact should you use?",
                         "evaluation": {
                             "scoring_rubric": "honor the correction",
                             "required_answer_terms": ["work"],
                             "forbidden_answer_terms": ["card"]
                         }}
                    ]
                }
            });
            Ok(AgentOutput {
                content: scenario.to_string(),
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 15,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn mine_scenarios_distills_corrections_and_scrubs_pii() {
    let models = Models::default();
    models.set_model(Model::with_completer(Arc::new(MinerCompleter)));
    let app = test_app_state_with_models("mine_corrections", Arc::new(models));
    let space = create_loaded_space(&app, "mine_corrections").await;
    let now_ms = unix_ms();
    seed_people(&space).await;

    // One revised Assertion is the mining signal.
    let response = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?a.id) WHERE { ?a ASSERTION {} } LIMIT 1"#,
        ))
        .await
        .unwrap();
    let signal: String =
        serde_json::from_value::<Vec<String>>(kip::ok_result(&response).cloned().unwrap())
            .unwrap()
            .remove(0);
    space
        .ledger
        .record_correction(&signal, now_ms)
        .await
        .unwrap();

    let (mined, usage) = crate::eval::mine::mine_scenarios(
        space.as_ref(),
        &crate::eval::mine::MineConfig {
            since_ms: 0,
            max_scenarios: 4,
        },
    )
    .await
    .unwrap();

    assert_eq!(mined.len(), 1);
    assert_eq!(mined[0].signal, signal);
    let scenario = &mined[0].scenario;
    assert_eq!(scenario.id, "mined_pref_fix");
    assert!(
        scenario
            .description
            .as_deref()
            .unwrap()
            .contains("review before adding")
    );
    // PII scrubbed from the produced scenario.
    let encoded = serde_json::to_string(scenario).unwrap();
    assert!(encoded.contains("[email]"), "{encoded}");
    assert!(encoded.contains("[number]"), "{encoded}");
    assert!(!encoded.contains("bob@example.com"));
    assert!(!encoded.contains("12345678901"));
    assert!(usage.input_tokens > 0);
}

#[tokio::test]
async fn memory_status_aggregates_counters_and_schema_audit() {
    let app = test_app_state("memory_status");
    let space = create_loaded_space(&app, "memory_status").await;
    let now_ms = unix_ms();
    let (concepts, _) = seed_people(&space).await;

    // Probe activity: one hit, one miss, one negative-cache hit.
    assert!(space.probe_memory("alpha", None).await.unwrap().found);
    assert!(!space.probe_memory("qqqzzz", None).await.unwrap().found);
    assert!(
        space
            .probe_memory("qqqzzz", None)
            .await
            .unwrap()
            .negative_cached
    );

    // One completed recall surfacing one entity.
    let message = serde_json::json!(Message {
        role: "assistant".to_string(),
        content: vec![
            anda_core::ContentPart::ToolCall {
                name: "execute_kip_readonly".to_string(),
                args: serde_json::json!({"command": "FIND"}),
                call_id: Some("c1".to_string()),
            },
            anda_core::ContentPart::ToolOutput {
                name: "execute_kip_readonly".to_string(),
                // A rendered element, not a bare reference: the ledger
                // meters what an answer actually read.
                output: serde_json::json!([{
                    "id": concepts[0],
                    "name": "alpha",
                    "schema_ref": "kip://profiles/cognitive-memory@2.1.0/Person",
                }]),
                is_error: None,
                call_id: Some("c1".to_string()),
                remote_id: None,
            }
        ],
        ..Default::default()
    });
    space.record_recall_usage(&[message]).await.unwrap();

    // One correction + a full settlement (metabolism + schema census).
    let old = assertion_about(&space, &concepts[0], &concepts[1]).await;
    seed_kip(
            &space,
            kip::request_with(
                r#"ASSERT ?new (:alpha, "prefers", :beta) { by: :alpha, mode: "stated", confidence: 0.4 }
SUPERSEDING :old"#,
                serde_json::Map::from_iter([
                    (
                        "alpha".to_string(),
                        serde_json::json!({"id": concepts[0]}),
                    ),
                    ("beta".to_string(), serde_json::json!({"id": concepts[1]})),
                    ("old".to_string(), serde_json::Value::from(old.as_str())),
                ]),
            ),
        )
        .await;
    space
        .settle_memory_metabolism(MaintenanceScope::Full, now_ms)
        .await
        .unwrap();

    let status = space.memory_status().await;
    assert_eq!(status.metrics.probe_hits, 1);
    assert_eq!(status.metrics.probe_misses, 1);
    assert_eq!(status.metrics.negative_cache_hits, 1);
    assert_eq!(status.metrics.recalls_completed, 1);
    assert_eq!(status.metrics.entities_recalled, 1, "{status:?}");
    assert_eq!(status.metrics.corrections, 1);
    assert_eq!(status.probe_hit_rate, Some(0.5));
    assert_eq!(status.correction_rate, Some(1.0));
    assert!(status.graph.concepts > 0);
    assert!(status.graph.predicate_types.unwrap_or(0) >= 1);
    assert!(status.last_settlement.is_some());

    // The full settlement also refreshed the per-predicate census. The
    // vocabulary is the Space's Schema Environment now, so the census
    // covers every declared predicate — including the ones nothing uses.
    let audit = status.last_schema_audit.expect("schema audit reported");
    // Two Propositions, not three: the revision above added an Assertion
    // about a tuple that already existed. A Proposition is the statement,
    // and how many actors have an opinion about it is a separate question.
    assert_eq!(audit.predicates.get("prefers"), Some(&2));
    assert_eq!(audit.predicates.get("same_as"), Some(&0));

    // ... and the same census reaches the Maintenance prompt, which its
    // deployment contract (§A.1) has always claimed. Correction discovery
    // recorded one revision above, so the actor tally travels with it.
    let assessment = space.maintenance_assessment(None).await;
    assert_eq!(assessment.predicates.get("prefers"), Some(&2));
    assert_eq!(assessment.audited_at, Some(audit.audited_at));
    assert_eq!(
        assessment
            .source_reliability
            .values()
            .map(|source| source.corrections)
            .sum::<u64>(),
        1,
        "{assessment:?}"
    );
}

#[test]
fn current_settlement_failure_replaces_a_stale_stored_report() {
    let stored = crate::types::MemorySettlementReport {
        decay_error: Some("previous decay failure".into()),
        ..Default::default()
    };
    assert_eq!(
        settlement_error_messages(Some(&stored), Some("usage ledger unavailable")),
        vec!["settlement: usage ledger unavailable"]
    );
    assert_eq!(
        settlement_error_messages(Some(&stored), None),
        vec!["decay: previous decay failure"]
    );
}

/// Shadow judge: always votes for answer B — with deterministic A/B
/// alternation this splits the wins 1:1, proving the swap works.
#[derive(Debug)]
struct ShadowJudgeCompleter;

impl CompletionFeaturesDyn for ShadowJudgeCompleter {
    fn model_name(&self) -> String {
        "shadow-judge-model".to_string()
    }

    fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        Box::pin(async move {
            Ok(AgentOutput {
                content: serde_json::json!({"winner": "b", "reason": "richer"}).to_string(),
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn shadow_eval_compares_policies_without_touching_live_space() {
    let app = test_app_state_with_final_model("shadow_eval");
    let space = create_loaded_space(&app, "shadow_eval").await;
    space.set_judge_model_for_test(Model::with_completer(Arc::new(ShadowJudgeCompleter)));

    // Two completed recall conversations: one stores a serialized
    // RecallInput, one a raw query string.
    for (id_suffix, prompt) in [
        (1u64, r#"{"query": "What tea do I drink?"}"#),
        (2u64, "Where do I work?"),
    ] {
        let now = unix_ms() + id_suffix;
        let conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            messages: vec![serde_json::json!(Message {
                role: "user".to_string(),
                content: vec![prompt.to_string().into()],
                ..Default::default()
            })],
            label: Some("recall".to_string()),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        space
            .recall
            .conversations
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
    }

    let candidate = crate::types::MemoryPolicy {
        memory_strength_decay_factor: 0.9,
        ..Default::default()
    };
    let report = app
        .run_shadow_eval(
            "shadow_eval",
            crate::types::ShadowEvalInput {
                policy: candidate.clone(),
                replay_sample: Some(2),
            },
        )
        .await
        .unwrap();

    assert_eq!(report.replayed, 2, "{report:?}");
    assert_eq!(report.judge_errors, 0, "{report:?}");
    // The judge always votes "B"; the deterministic order alternation
    // maps that to one win per side.
    assert_eq!(report.candidate_wins, 1, "{report:?}");
    assert_eq!(report.baseline_wins, 1, "{report:?}");
    assert_eq!(report.samples.len(), 2);
    assert_eq!(report.candidate_policy.memory_strength_decay_factor, 0.9);

    // The report persists on the live space...
    let stored: crate::types::ShadowReport = space
        .db
        .get_extension_as("shadow_report")
        .expect("report stored");
    assert_eq!(stored.replayed, 2);
    // ...while the live space itself stayed untouched: no policy change,
    // no usage recorded by the fork replays (plan guardrail 4).
    assert_eq!(space.memory_policy(), crate::types::MemoryPolicy::default());
    assert_eq!(space.memory_status().await.metrics.recalls_completed, 0);
}

#[tokio::test]
async fn space_token_limit_and_tier_node_limit_are_enforced() {
    let app = test_app_state("space_limits");
    let space = create_loaded_space(&app, "space_limits").await;
    space.admin_update_tier(0, 1).await.unwrap();

    for idx in 0..100 {
        space
            .add_space_token(
                format!("STlimit-{idx}"),
                AddSpaceTokenInput {
                    scope: TokenScope::Read,
                    name: format!("reader-{idx}"),
                    expires_at: None,
                    labels: None,
                },
                idx,
            )
            .await
            .unwrap();
    }
    let err = space
        .add_space_token(
            "STlimit-overflow".to_string(),
            AddSpaceTokenInput {
                scope: TokenScope::Read,
                name: "overflow".to_string(),
                expires_at: None,
                labels: None,
            },
            101,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("space token limit reached"));

    for idx in 0..101 {
        let conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: idx,
            updated_at: idx,
            label: Some("formation".to_string()),
            ..Default::default()
        };
        space
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
    }
    // Empty input is rejected before any other check…
    let err = space
        .ingest(
            SELF_USER_ID,
            StringOr::Value(FormationInput {
                messages: vec![],
                context: None,
                timestamp: None,
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
    // …including a non-empty content array holding only blank text parts
    // (the part-count check alone would let this burn a formation cycle)…
    let err = space
        .ingest(
            SELF_USER_ID,
            StringOr::Value(FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec!["   ".to_string().into()],
                    ..Default::default()
                }],
                context: None,
                timestamp: None,
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
    // …while real input still hits the tier node limit.
    let err = space
        .ingest(
            SELF_USER_ID,
            StringOr::Value(FormationInput {
                messages: vec![Message {
                    role: "user".into(),
                    content: vec!["remember this".to_string().into()],
                    ..Default::default()
                }],
                context: None,
                timestamp: None,
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("node limit exceeded"));
}

#[tokio::test]
async fn space_conversations_are_accessible_across_collections() {
    let app = test_app_state("space_conversations");
    let space = create_loaded_space(&app, "space_conversations").await;
    let now = unix_ms();

    let formation = Conversation {
        user: SELF_USER_ID,
        status: ConversationStatus::Completed,
        created_at: now,
        updated_at: now,
        label: Some("formation".to_string()),
        ..Default::default()
    };
    let recall = Conversation {
        user: SELF_USER_ID,
        status: ConversationStatus::Completed,
        created_at: now + 1,
        updated_at: now + 1,
        label: Some("recall".to_string()),
        ..Default::default()
    };
    let maintenance = Conversation {
        user: SELF_USER_ID,
        status: ConversationStatus::Completed,
        created_at: now + 2,
        updated_at: now + 2,
        label: Some("maintenance".to_string()),
        ..Default::default()
    };

    let formation_id = space
        .memory
        .add_conversation(ConversationRef::from(&formation))
        .await
        .unwrap();
    let recall_id = space
        .recall
        .conversations
        .add_conversation(ConversationRef::from(&recall))
        .await
        .unwrap();
    let maintenance_id = space
        .maintenance
        .conversations
        .add_conversation(ConversationRef::from(&maintenance))
        .await
        .unwrap();

    assert_eq!(
        space
            .get_conversation(None, formation_id)
            .await
            .unwrap()
            .label,
        Some("formation".to_string())
    );
    assert_eq!(
        space
            .get_conversation(Some("recall".to_string()), recall_id)
            .await
            .unwrap()
            .label,
        Some("recall".to_string())
    );
    assert_eq!(
        space
            .get_conversation(Some("maintenance".to_string()), maintenance_id)
            .await
            .unwrap()
            .label,
        Some("maintenance".to_string())
    );

    let (items, cursor) = space.list_conversations(None, None, Some(1)).await.unwrap();
    assert_eq!(items.len(), 1);
    assert!(cursor.is_some());

    let (recall_items, _) = space
        .list_conversations(Some("recall".to_string()), None, Some(10))
        .await
        .unwrap();
    assert_eq!(recall_items.len(), 1);

    let status = space.formation_status();
    assert_eq!(status.conversations, 1);
    assert!(!status.formation_processing);
    assert!(!status.maintenance_processing);

    assert!(
        space
            .list_conversations(None, Some("not-a-cursor".to_string()), Some(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn list_conversations_clamps_limit_to_safe_bounds() {
    let app = test_app_state("list_limit_clamp");
    let space = create_loaded_space(&app, "list_limit_clamp").await;

    // limit=0 on an empty collection must not panic on the cursor below.
    let (items, cursor) = space.list_conversations(None, None, Some(0)).await.unwrap();
    assert!(items.is_empty());
    assert!(cursor.is_none());

    for idx in 0..3 {
        let conversation = Conversation {
            user: SELF_USER_ID,
            status: ConversationStatus::Completed,
            created_at: idx,
            updated_at: idx,
            label: Some("formation".to_string()),
            ..Default::default()
        };
        space
            .memory
            .add_conversation(ConversationRef::from(&conversation))
            .await
            .unwrap();
    }

    // limit=0 is clamped to 1 instead of dumping the whole collection.
    let (items, cursor) = space.list_conversations(None, None, Some(0)).await.unwrap();
    assert_eq!(items.len(), 1);
    assert!(cursor.is_some());

    // "formation" is the documented name of the default collection (API
    // docs, MCP tool schemas): the canonical spelling must stay valid,
    // while typos keep erroring instead of silently reading formation.
    let (items, _) = space
        .list_conversations(Some("formation".to_string()), None, Some(10))
        .await
        .unwrap();
    assert_eq!(items.len(), 3);
    let got = space
        .get_conversation(Some("formation".to_string()), items[0]._id)
        .await
        .unwrap();
    assert_eq!(got._id, items[0]._id);
    assert!(
        space
            .list_conversations(Some("Formation".to_string()), None, Some(10))
            .await
            .is_err()
    );
    assert!(
        space
            .get_conversation(Some("Recall".to_string()), items[0]._id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn space_agent_entrypoints_use_memory_and_model_without_network() {
    let app = test_app_state_with_final_model("space_agent_entrypoints");
    let space = create_loaded_space(&app, "space_agent_entrypoints").await;

    let formation = FormationInput {
        messages: vec![Message {
            role: "user".to_string(),
            content: vec![
                "remember that the preferred color is blue"
                    .to_string()
                    .into(),
            ],
            ..Default::default()
        }],
        context: Some(InputContext {
            counterparty: Some("external-user-formation".to_string()),
            agent: Some("agent-a".to_string()),
            source: Some("thread-1".to_string()),
            topic: Some("preferences".to_string()),
        }),
        timestamp: Some("2026-06-05T00:00:00Z".to_string()),
    };
    let formation_output = space
        .ingest(SELF_USER_ID, StringOr::Value(formation))
        .await
        .unwrap();
    let formation_id = formation_output.conversation.unwrap();
    wait_until_idle(&space).await;

    let formation_conversation = space.get_conversation(None, formation_id).await.unwrap();
    assert_eq!(formation_conversation.status, ConversationStatus::Completed);
    assert_eq!(space.formation.get_processed(), Some(formation_id));

    let counterparty = space
        .formation
        .get_or_init_counterparty(
            "external-user-formation".to_string(),
            Some("Formation User".to_string()),
        )
        .await
        .unwrap();
    // A Concept names its type by the exact schema symbol it was created
    // under, so the meaning cannot drift when a package is republished.
    assert_eq!(
        counterparty["schema_ref"],
        "kip://profiles/cognitive-memory@2.1.0/Person"
    );
    assert_eq!(counterparty["key"], "external-user-formation");
    assert_eq!(counterparty["name"], "Formation User");

    let recall = RecallInput {
        query: "What color is preferred?".to_string(),
        context: Some(InputContext {
            counterparty: Some("external-user-formation".to_string()),
            agent: None,
            source: None,
            topic: Some("preferences".to_string()),
        }),
    };
    let recall_output = space
        .query(SELF_USER_ID, StringOr::Value(recall))
        .await
        .unwrap();
    let recall_id = recall_output.conversation.unwrap();
    let recall_conversation = space
        .get_conversation(Some("recall".to_string()), recall_id)
        .await
        .unwrap();
    assert_eq!(recall_conversation.status, ConversationStatus::Completed);

    let maintenance_output = space
        .maintenance(
            SELF_USER_ID,
            MaintenanceInput {
                scope: MaintenanceScope::Quick,
                formation_id,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(maintenance_output.conversation.is_some());
    wait_until_idle(&space).await;
    assert_eq!(space.maintenance.get_processed_at().quick, formation_id);
    space
        .maintenance
        .set_processed_at(MaintenanceScope::Full, formation_id + 1)
        .await
        .unwrap();
    space
        .maintenance
        .set_processed_at(MaintenanceScope::Daydream, formation_id + 2)
        .await
        .unwrap();
    let maintenance_at = space.maintenance.get_processed_at();
    assert_eq!(maintenance_at.full, formation_id + 1);
    assert_eq!(maintenance_at.daydream, formation_id + 2);

    let primer = space
        .execute_kip_readonly(kip::request("DESCRIBE PRIMER"))
        .await
        .unwrap();
    assert!(kip::ok_result(&primer).is_some(), "{primer:?}");

    let restart_err = space
        .restart_formation(SELF_USER_ID, formation_id + 1)
        .await
        .unwrap_err();
    assert!(
        restart_err
            .to_string()
            .contains("No pending formation conversation")
    );
}

#[tokio::test]
async fn space_agent_guards_and_readonly_tool_paths() {
    let app = test_app_state_with_final_model("space_agent_guards");
    let space = create_loaded_space(&app, "space_agent_guards").await;

    let readonly = TimedMemoryReadonly::new(space.memory.clone());
    assert_eq!(Tool::<BaseCtx>::name(&readonly), MemoryReadonly::NAME);
    // The read-only definition is the one `anda_kip` ships with the
    // protocol: no write vocabulary, and no `execution` modes for a read
    // path to choose between.
    let definition = Tool::<BaseCtx>::definition(&readonly);
    assert_eq!(definition.name, MemoryReadonly::NAME);
    assert!(
        definition.parameters["properties"]
            .get("execution")
            .is_none()
    );

    let ok_ctx = space
        .engine
        .base_ctx_with(
            SELF_USER_ID,
            "recall_memory",
            MemoryReadonly::NAME,
            Default::default(),
        )
        .unwrap();
    let ok = Tool::<BaseCtx>::call(
        &readonly,
        ok_ctx,
        KipArgs {
            command: Some("DESCRIBE PRIMER".to_string()),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(ok.is_error, None);

    let err_ctx = space
        .engine
        .base_ctx_with(
            SELF_USER_ID,
            "recall_memory",
            MemoryReadonly::NAME,
            Default::default(),
        )
        .unwrap();
    let err = Tool::<BaseCtx>::call(
        &readonly,
        err_ctx,
        KipArgs {
            command: Some("NOT A VALID KIP COMMAND".to_string()),
            ..Default::default()
        },
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(err.is_error, Some(true));
}

#[tokio::test]
async fn maintenance_rejects_concurrent_runs() {
    let app = test_app_state_with_slow_model("maintenance_concurrent");
    let space = create_loaded_space(&app, "maintenance_concurrent").await;

    let first = space
        .maintenance(
            SELF_USER_ID,
            MaintenanceInput {
                scope: MaintenanceScope::Quick,
                formation_id: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(first.conversation.is_some());

    // The maintenance slot is claimed before settlement (review P1-3),
    // so a concurrent start fails the claim and errors out instead of
    // returning a placeholder output.
    let second = space
        .maintenance(
            SELF_USER_ID,
            MaintenanceInput {
                scope: MaintenanceScope::Quick,
                formation_id: 2,
                ..Default::default()
            },
        )
        .await;
    assert!(
        second
            .unwrap_err()
            .to_string()
            .contains("already in progress")
    );

    wait_until_idle(&space).await;
}

#[tokio::test]
async fn hooks_handle_unbound_space_and_accumulate_usage() {
    let app = test_app_state_with_final_model("hooks_usage");
    let space = create_loaded_space(&app, "hooks_usage").await;
    let unbound = Hooks::new(space.db.clone());

    assert!(!BrainHook::is_maintenance_processing(&unbound));
    BrainHook::try_start_formation(&unbound).await;
    assert!(
        BrainHook::try_start_maintenance(&unbound, 168)
            .await
            .is_none()
    );

    let hooks = Hooks::new(space.db.clone());
    hooks.bind_space(Arc::downgrade(&space));
    assert!(!BrainHook::is_maintenance_processing(&hooks));
    space
        .conversations
        .save_extension("brain_processed".to_string(), 7_u64.into())
        .await
        .unwrap();
    BrainHook::try_start_formation(&hooks).await;

    let conversation = Conversation {
        usage: Usage {
            input_tokens: 11,
            output_tokens: 7,
            cached_tokens: 3,
            requests: 2,
        },
        ..Default::default()
    };

    BrainHook::on_conversation_end(&hooks, "recall_memory", &conversation).await;
    BrainHook::on_conversation_end(&hooks, "formation_memory", &conversation).await;
    BrainHook::on_conversation_end(&hooks, "maintenance_memory", &conversation).await;
    BrainHook::on_conversation_end(&hooks, "unknown_agent", &conversation).await;

    let info = space.get_info();
    assert_eq!(info.recall_usage.requests, 2);
    assert_eq!(info.formation_usage.input_tokens, 11);
    assert_eq!(info.maintenance_usage.output_tokens, 7);
    assert_eq!(info.maintenance_usage.cached_tokens, 3);
}

#[tokio::test]
async fn hooks_schedule_maintenance_at_thresholds() {
    let app = test_app_state_with_final_model("hooks_thresholds");
    let space = create_loaded_space(&app, "hooks_thresholds").await;
    let hooks = Hooks::new(space.db.clone());
    hooks.bind_space(Arc::downgrade(&space));

    assert!(BrainHook::try_start_maintenance(&hooks, 20).await.is_none());

    space
        .conversations
        .save_extension("brain_processed".to_string(), 21_u64.into())
        .await
        .unwrap();
    let daydream = BrainHook::try_start_maintenance(&hooks, 21).await.unwrap();
    wait_until_idle(&space).await;
    assert_eq!(space.maintenance_for_test().get_processed_at().daydream, 21);

    space
        .conversations
        .save_extension("brain_processed".to_string(), 42_u64.into())
        .await
        .unwrap();
    let quick = BrainHook::try_start_maintenance(&hooks, 42).await.unwrap();
    wait_until_idle(&space).await;
    assert!(quick > daydream);
    assert_eq!(space.maintenance_for_test().get_processed_at().quick, 42);

    space
        .conversations
        .save_extension("brain_processed".to_string(), 168_u64.into())
        .await
        .unwrap();
    let full = BrainHook::try_start_maintenance(&hooks, 168).await.unwrap();
    wait_until_idle(&space).await;
    assert!(full > quick);
    assert_eq!(space.maintenance_for_test().get_processed_at().full, 168);
}

/// M4 acceptance: an exported OKF bundle plus its manifest replays into
/// an empty space with every document checksum intact.
#[cfg(feature = "wiki")]
#[tokio::test]
async fn wiki_export_bundle_replays_into_empty_space() {
    use crate::wiki::{WikiBundleEntry, WikiCommitInput, WikiImportInput};

    let app = test_app_state("wiki_replay_src");
    let source = create_loaded_space(&app, "wiki_replay_source").await;
    for (title, body) in [
        ("部署指南", "# 部署指南\n\n回滚使用上一版本快照。\n"),
        ("安全政策", "# 安全政策\n\n密钥必须存放在 KMS。\n"),
    ] {
        let mut input = WikiCommitInput {
            title: title.to_string(),
            content: body.to_string(),
            ..Default::default()
        };
        input.namespace = Some("kb".to_string());
        source
            .wiki
            .commit("op".to_string(), input, unix_ms())
            .await
            .unwrap();
    }
    let export = source
        .wiki
        .export_bundle("op".to_string(), Some("kb".to_string()), unix_ms())
        .await
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_str(
        &export
            .entries
            .iter()
            .find(|e| e.path == "manifest.json")
            .unwrap()
            .content,
    )
    .unwrap();

    // Replay into a brand-new space.
    let replay_app = test_app_state("wiki_replay_dst");
    let target = create_loaded_space(&replay_app, "wiki_replay_target").await;
    let entries: Vec<WikiBundleEntry> = export
        .entries
        .iter()
        .filter(|e| e.path.ends_with(".md"))
        .cloned()
        .collect();
    let imported = target
        .wiki
        .import_bundle(
            "op".to_string(),
            WikiImportInput {
                entries,
                namespace: Some("kb".to_string()),
            },
            unix_ms(),
        )
        .await
        .unwrap();
    assert_eq!(imported.created, export.docs);

    // Every replayed document matches the manifest checksum: the bundle
    // is a faithful backup.
    for doc in manifest["docs"].as_array().unwrap() {
        let path = doc["path"].as_str().unwrap();
        let checksum = doc["checksum"].as_str().unwrap();
        let restored = imported
            .docs
            .iter()
            .find(|d| d.path == path)
            .unwrap_or_else(|| panic!("missing {path}"));
        let info = target.wiki.get_doc(restored.doc_id).await.unwrap();
        assert_eq!(info.current_checksum, checksum, "checksum drift for {path}");
    }

    // SpaceInfo exposes the M4 wiki metrics.
    let info = target.get_info();
    assert_eq!(info.wiki_docs, export.docs);
    assert!(info.wiki_versions >= export.docs);
}

/// Replays scripted completion responses in order; used to drive the
/// wiki digest extraction deterministically.
#[cfg(feature = "wiki")]
#[derive(Debug)]
struct ScriptedCompleter(std::sync::Mutex<std::collections::VecDeque<String>>);

#[cfg(feature = "wiki")]
impl CompletionFeaturesDyn for ScriptedCompleter {
    fn model_name(&self) -> String {
        "scripted-test-model".to_string()
    }

    fn completion(&self, _req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
        let next = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| r#"{"facts": []}"#.to_string());
        Box::pin(async move {
            Ok(AgentOutput {
                content: next,
                ..Default::default()
            })
        })
    }
}

#[cfg(feature = "wiki")]
#[tokio::test]
async fn wiki_digest_extracts_supersedes_and_verifies() {
    use crate::wiki::WikiCommitInput;

    let extraction_v1 = serde_json::json!({
            "concepts": [
                {"type": "Organization", "name": "Acme", "attributes": {"description": "发布政策的组织"}}
            ],
            "facts": [
                {
                    "subject": {"type": "Organization", "name": "Acme"},
                    "predicate": "publishes",
                    "object": {"type": "Policy", "name": "安全政策"},
                    "confidence": 0.95,
                    "anchor": "安全政策-0"
                },
                {
                    "subject": {"type": "Policy", "name": "安全政策"},
                    "predicate": "requires",
                    "object": {"type": "Procedure", "name": "密钥轮换"},
                    "confidence": 0.9,
                    "anchor": "no-such-anchor"
                }
            ]
        })
        .to_string();
    let extraction_v2 = serde_json::json!({
        "facts": [
            {
                "subject": {"type": "Organization", "name": "Acme"},
                "predicate": "publishes",
                "object": {"type": "Policy", "name": "安全政策"},
                "confidence": 0.95,
                "anchor": "安全政策-0"
            },
            {
                "subject": {"type": "Policy", "name": "安全政策"},
                "predicate": "requires",
                "object": {"type": "Procedure", "name": "双因素认证"},
                "confidence": 0.9,
                "anchor": "安全政策-0"
            }
        ]
    })
    .to_string();

    let models = Models::default();
    models.set_model(Model::with_completer(Arc::new(ScriptedCompleter(
        std::sync::Mutex::new([extraction_v1, extraction_v2].into_iter().collect()),
    ))));
    let app = test_app_state_with_models("wiki_digest_app", Arc::new(models));
    let space = create_loaded_space(&app, "wiki_digest_space").await;

    // RecallAgent exposes the wiki evidence tools to its LLM loop.
    {
        use anda_core::Agent;
        let deps = space.recall.tool_dependencies();
        assert!(deps.contains(&"wiki_search".to_string()));
        assert!(deps.contains(&"wiki_read".to_string()));
    }

    // Digest is opt-in: disabled spaces refuse to run.
    let err = space.run_wiki_digest(SELF_USER_ID).await.unwrap_err();
    assert!(err.to_string().contains("disabled"));
    space
        .update(
            crate::types::UpdateSpaceInput {
                wiki_digest: Some(true),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();
    assert!(space.wiki_digest_enabled());

    let v1 = space
        .wiki
        .commit(
            "tester".to_string(),
            WikiCommitInput {
                title: "安全政策".to_string(),
                content: "# 安全政策\n\n所有系统必须启用密钥轮换。\n".to_string(),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();

    let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
    assert_eq!(report.digested, 1);
    assert_eq!(report.facts, 2);
    assert_eq!(report.superseded, 0);
    assert!(report.citations_checked >= 2);
    assert_eq!(report.citations_invalid, 0);

    // The Space's Schema Environment grew to hold the extracted
    // vocabulary — in KIP 2.0 the digest cannot mint a type on the way in,
    // so `Organization` existing at all is the host having decided it does.
    let types = space
        .execute_kip_readonly(kip::request("LIST TYPES LIMIT 500"))
        .await
        .unwrap();
    assert!(
        kip::ok_result(&types)
            .unwrap()
            .to_string()
            .contains("Organization"),
        "{types:?}"
    );

    // §5.6/§64.2: the digest minted the `$self` Person, so the Space now
    // designates one — and the primer every agent reads reports it as a
    // different thing from the authenticated Principal.
    let primer = space
        .execute_kip_readonly(kip::request("DESCRIBE PRIMER"))
        .await
        .unwrap();
    let primer = kip::ok_result(&primer).unwrap();
    let designated = primer
        .pointer("/cognitive_identity/self_concept/id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        designated.starts_with("C-"),
        "cognitive_identity: {}",
        primer["cognitive_identity"]
    );
    assert!(
        primer.pointer("/execution_context/principal/id").is_some(),
        "the primer distinguishes the Principal from $self: {primer}"
    );

    // The graph holds the concepts, and the claim about them carries its
    // provenance as Evidence rather than as metadata on the link.
    let acme = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?c.id) WHERE { ?c CONCEPT {type: "Organization", key: "Acme"} }"#,
        ))
        .await
        .unwrap();
    let acme: Vec<String> =
        serde_json::from_value(kip::ok_result(&acme).cloned().unwrap()).unwrap();
    assert_eq!(acme.len(), 1, "{acme:?}");

    // The claim cites its Evidence…
    let cited = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?a.evidence) WHERE {
  ?s CONCEPT {type: "Organization", key: "Acme"}
  ?o CONCEPT {type: "Policy", key: "安全政策"}
  ?p (?s, "publishes", ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
        ))
        .await
        .unwrap();
    let cited_text = kip::ok_result(&cited).unwrap().to_string();
    // §13.2: a citation is `{"id": "E-…", "role": "support"}` — the role is
    // what makes it a citation rather than a bare pointer, so assert on it.
    assert!(cited_text.contains(r#""role""#), "citations: {cited_text}");
    assert!(cited_text.contains("\"E-"), "citations: {cited_text}");

    // …and the Evidence carries the passage it was read from.
    let evidence = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?e.payload) WHERE { ?e EVIDENCE {evidence_class: "document"} }"#,
        ))
        .await
        .unwrap();
    let evidence_text = kip::ok_result(&evidence).unwrap().to_string();
    assert!(
        evidence_text.contains("wiki://"),
        "evidence: {evidence_text}"
    );
    assert!(
        evidence_text.contains("wiki_digest@v1"),
        "evidence: {evidence_text}"
    );
    assert!(
        evidence_text.contains(&format!("@{}", v1.version.id)),
        "the citation should pin version {}: {evidence_text}",
        v1.version.id
    );

    // Digest ledger event recorded with both facts.
    let events = space
        .wiki
        .list_events(Some("DigestExtracted".to_string()), None, None, Some(10))
        .await
        .unwrap();
    assert_eq!(events.events.len(), 1);

    // No pending versions: the next run is a no-op (cursor advanced).
    let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
    assert_eq!(report.digested, 0);

    // Revision drops the 密钥轮换 requirement; digesting it must mark the
    // stale proposition superseded while the surviving fact stays live.
    let v2 = space
        .wiki
        .commit(
            "tester".to_string(),
            WikiCommitInput {
                doc_id: Some(v1.doc.id),
                parent_version: Some(v1.version.id),
                title: "安全政策".to_string(),
                content: "# 安全政策\n\n所有系统必须启用双因素认证。\n".to_string(),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();
    let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
    assert_eq!(report.digested, 1);
    assert_eq!(report.superseded, 1);

    // The dropped fact: the digest withdrew its own claim, and the
    // Proposition itself survives untouched. KIP 1.x flagged the link
    // `superseded`, which spoke for every actor at once; a retraction says
    // only that *this* reader stopped saying it.
    let stale = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?a.lifecycle.status) WHERE {
  ?s CONCEPT {type: "Policy", key: "安全政策"}
  ?o CONCEPT {type: "Procedure", key: "密钥轮换"}
  ?p (?s, "requires", ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
        ))
        .await
        .unwrap();
    let statuses: Vec<String> =
        serde_json::from_value(kip::ok_result(&stale).cloned().unwrap()).unwrap();
    assert_eq!(statuses, vec!["retracted".to_string()], "{stale:?}");

    // The surviving fact stays believed.
    let live = space
        .execute_kip_readonly(kip::request(
            r#"FIND(?a.lifecycle.status) WHERE {
  ?s CONCEPT {type: "Organization", key: "Acme"}
  ?o CONCEPT {type: "Policy", key: "安全政策"}
  ?p (?s, "publishes", ?o)
  ?a ASSERTION {proposition: ?p}
}"#,
        ))
        .await
        .unwrap();
    let statuses: Vec<String> =
        serde_json::from_value(kip::ok_result(&live).cloned().unwrap()).unwrap();
    assert_eq!(statuses, vec!["active".to_string()], "{live:?}");
    let _ = v2;

    // Labeled documents never reach the graph: the Cognitive Nexus has
    // no ACL, so digesting them would leak restricted facts to any Read
    // principal (launch review P1-3).
    space
        .wiki
        .commit(
            "tester".to_string(),
            WikiCommitInput {
                title: "受限预案".to_string(),
                content: "# 受限预案\n\n机密事实：夜航坐标由 Acme 维护。\n".to_string(),
                acl_label: Some("secret".to_string()),
                ..Default::default()
            },
            unix_ms(),
        )
        .await
        .unwrap();
    let report = space.run_wiki_digest(SELF_USER_ID).await.unwrap();
    assert_eq!(report.digested, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.facts, 0);
}

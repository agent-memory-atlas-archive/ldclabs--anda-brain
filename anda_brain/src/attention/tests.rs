use super::*;
use anda_cognitive_nexus::attention::{AttentionConfig, RuntimePin};
use object_store::{PutMode, memory::InMemory};
use std::sync::Arc;

fn config(id: &str) -> AttentionConfig {
    AttentionConfig {
        scope: RuntimeScope {
            space_id: "kip:space:default".into(),
            space_instance: id.into(),
        },
        pins: RuntimePins {
            policy: RuntimePin {
                id: "test".into(),
                digest: format!("sha256:{}", "a".repeat(64)),
            },
            evaluator: None,
            binding: None,
        },
    }
}

#[tokio::test]
async fn directory_cas_does_not_clear_new_dirty_work_and_reads_all_pages() {
    let store = Arc::new(InMemory::new());
    let d = Directory::new(store.clone(), 7);
    let mut old = d.register("first", &config("first"), true).await.unwrap();
    let fresh = d.register("first", &config("first"), true).await.unwrap();
    old.last_report.scan_complete = true;
    assert!(!d.finish(old).await.unwrap());
    assert_eq!(
        d.entry("first")
            .await
            .unwrap()
            .unwrap()
            .value
            .registration
            .reconciled_generation,
        0
    );
    assert_eq!(fresh.registration.dirty_generation, 2);
    for id in ["second", "third", "fourth", "fifth"] {
        d.register(id, &config(id), true).await.unwrap();
    }
    let d = Directory::new(store, 7);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..3 {
        let page = d.page(2).await.unwrap();
        assert!(page.len() <= 2);
        for slot in page {
            seen.insert(
                d.read_slot(slot)
                    .await
                    .unwrap()
                    .unwrap()
                    .registration
                    .scope
                    .space_id,
            );
            d.advance_cursor(slot).await.unwrap();
        }
    }
    assert_eq!(seen.len(), 5);
    assert_eq!(d.page(2).await.unwrap(), vec![1, 2]);
    assert!(
        d.register("first", &config("replacement-instance"), true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn directory_reconciles_lost_ack_and_rejects_wrong_scope() {
    use anda_object_store::fault::{FaultKind, FaultOp, FaultRule, FaultStore};
    let (store, faults) = FaultStore::wrap(InMemory::new());
    let d = Directory::new(Arc::new(store), 3);
    faults.push_rule(FaultRule {
        kind: FaultKind::ErrorAfter,
        ..FaultRule::fail_once(FaultOp::Put, "/spaces/")
    });
    let entry = d.register("safe", &config("safe"), true).await.unwrap();
    let mut bad = entry.clone();
    bad.registration.shard = 999;
    let key = format!(
        "spaces/{}",
        &anda_cognitive_nexus::content_digest(&serde_json::json!("safe")).unwrap()[7..]
    );
    d.put(&key, &bad, PutMode::Overwrite).await.unwrap();
    assert!(d.read_slot(entry.slot).await.is_err());
}

#[tokio::test]
async fn local_directory_cas_survives_metastore_restart() {
    use anda_object_store::MetaStoreBuilder;
    let dir = std::env::temp_dir().join(format!("brain-r2-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    let make = || {
        Arc::new(
            MetaStoreBuilder::new(
                object_store::local::LocalFileSystem::new_with_prefix(&dir).unwrap(),
                100,
            )
            .build(),
        )
    };
    let d = Directory::new(make(), 0);
    let mut entry = d.register("local", &config("local"), true).await.unwrap();
    entry.last_report.scan_complete = true;
    assert!(d.finish(entry).await.unwrap());
    drop(d);
    let reopened = Directory::new(make(), 0);
    let entry = reopened.read_slot(1).await.unwrap().unwrap();
    assert_eq!(
        entry.registration.dirty_generation,
        entry.registration.reconciled_generation
    );
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

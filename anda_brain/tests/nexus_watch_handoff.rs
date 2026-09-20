//! Downstream acceptance against the published Nexus atomic handoff.
use anda_cognitive_nexus::{
    CognitiveNexus,
    nexus::DEFAULT_SPACE,
    profiles::COGNITIVE_MEMORY,
    store::{Element, rows::ControlRecordRow},
};
use anda_db::database::{AndaDB, DBConfig};
use anda_kip::{Request, TopLevelStatus};
use object_store::memory::InMemory;
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture {
    store: Arc<InMemory>,
    config: DBConfig,
    nexus: CognitiveNexus,
    target: String,
    watch: String,
    version: u64,
    generation: u64,
    matched_seq: u64,
}

async fn run(nexus: &CognitiveNexus, command: &str, parameters: Value) -> Value {
    let mut request = Request::single(command);
    request.parameters = parameters.as_object().cloned();
    let response = anda_kip::execute_request(nexus, &request).await;
    assert_eq!(response.status, TopLevelStatus::Succeeded, "{response:?}");
    response.first_result().cloned().expect("operation result")
}

impl Fixture {
    async fn new() -> Self {
        let store = Arc::new(InMemory::new());
        let config = DBConfig {
            name: "r0_watch_handoff".into(),
            ..Default::default()
        };
        let db = Arc::new(
            AndaDB::connect(store.clone(), config.clone())
                .await
                .unwrap(),
        );
        let nexus = CognitiveNexus::connect(db).await.unwrap();
        nexus
            .install_and_activate(&[("brain", COGNITIVE_MEMORY)], DEFAULT_SPACE)
            .await
            .unwrap();
        let target = run(
            &nexus,
            r#"CREATE CONCEPT ?target {TYPE "Person" NAME "before"}"#,
            Value::Null,
        )
        .await["handles"]["target"]
            .as_str()
            .unwrap()
            .to_owned();
        let watch = run(&nexus, r#"CREATE CONCEPT ?watch {TYPE "Watch" SET ATTRIBUTES {watch_class:"delta",summary:"track target",condition:{element: :target,ops:["update"]},status:"disarmed"}}"#, json!({"target":target})).await["handles"]["watch"].as_str().unwrap().to_owned();
        let version = nexus
            .store
            .get_element(watch.parse().unwrap())
            .await
            .unwrap()
            .version();
        let armed = nexus
            .system_session()
            .arm_watch(DEFAULT_SPACE, &watch, version)
            .await
            .unwrap();
        let version = nexus
            .store
            .get_element(watch.parse().unwrap())
            .await
            .unwrap()
            .version();
        let generation = armed["watch"]["arm_generation"].as_u64().unwrap();
        run(
            &nexus,
            r#"UPDATE :target SET FIELDS {name:"after"}"#,
            json!({"target":target}),
        )
        .await;
        let matched_seq = nexus.store.get_space(DEFAULT_SPACE).await.unwrap().seq;
        Self {
            store,
            config,
            nexus,
            target,
            watch,
            version,
            generation,
            matched_seq,
        }
    }

    async fn advance(&self) -> Value {
        self.nexus
            .system_session()
            .advance_watch(
                DEFAULT_SPACE,
                &self.watch,
                self.version,
                self.generation,
                200,
            )
            .await
            .unwrap()
    }

    async fn reopen(self) -> Self {
        let Self {
            store,
            config,
            nexus,
            target,
            watch,
            version,
            generation,
            matched_seq,
        } = self;
        nexus.close().await.unwrap();
        drop(nexus);
        let db = Arc::new(AndaDB::open(store.clone(), config.clone()).await.unwrap());
        let nexus = CognitiveNexus::connect(db).await.unwrap();
        Self {
            store,
            config,
            nexus,
            target,
            watch,
            version,
            generation,
            matched_seq,
        }
    }

    async fn assert_handoff(&self, result: &Value) {
        assert_eq!(result["status"], "fired");
        let activities = run(
            &self.nexus,
            r#"FIND(?a.id) WHERE {?a ACTIVITY {activity_class:"watch_fire"}} LIMIT 10"#,
            Value::Null,
        )
        .await;
        assert_eq!(
            activities.as_array().unwrap().len(),
            1,
            "Atomic handoff requires exactly one watch_fire Activity in the firing transaction; observed {activities}"
        );
        let activity_ref = result["fire_activity_ref"]
            .as_str()
            .expect("native fire_activity_ref");
        assert_eq!(activities[0], activity_ref);
        let fire_key = format!(
            "watch_fire:{}:{}:{}",
            self.watch, self.generation, self.matched_seq
        );
        assert_eq!(result["fire_key"], fire_key);
        let wake_ref = result["wake_ref"].as_str().expect("native wake_ref");
        assert!(wake_ref.starts_with("wake/v1/"));
        let mut wakes = Vec::new();
        let collection = self.nexus.store.control_records();
        for id in collection.ids() {
            let row: ControlRecordRow = collection.get_as(id).await.unwrap();
            if row.space == DEFAULT_SPACE && row.key.starts_with("wake/v1/") {
                wakes.push(row);
            }
        }
        assert_eq!(
            wakes.len(),
            1,
            "Atomic handoff requires exactly one durable wake, not merely a returned id"
        );
        let wake = &wakes[0];
        assert_eq!(wake.key, wake_ref);
        assert_eq!(wake.value["fire_activity_ref"], activity_ref);
        assert_eq!(wake.value["fire"]["watch_ref"], self.watch);
        assert_eq!(wake.value["fire"]["arm_generation"], self.generation);
        let committed_seq = result["receipt"]["space_seq"]
            .as_u64()
            .expect("committed receipt");
        let activity = self
            .nexus
            .store
            .get_element(activity_ref.parse().unwrap())
            .await
            .unwrap();
        let Element::Activity(activity_row) = &activity else {
            panic!("fire_activity_ref must name an Activity");
        };
        assert_eq!(activity_row.client_key, fire_key);
        let watch = self
            .nexus
            .store
            .get_element(self.watch.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(activity.seq(), committed_seq);
        assert_eq!(watch.seq(), committed_seq);
        assert_eq!(
            wake.seq, committed_seq,
            "wake must share the native commit, not be a follow-up write"
        );
    }
}

#[tokio::test]
async fn native_watch_coverage_survives_a_real_database_reopen() {
    let fixture = Fixture::new().await.reopen().await;
    let fired = fixture.advance().await;
    assert_eq!(fired["status"], "fired");
    assert_eq!(fired["watch"]["arm_generation"], fixture.generation);
    assert_eq!(fired["watch"]["matched"], true);
    assert!(fired["watch"]["consumed_seq"].as_u64().unwrap() >= fixture.matched_seq);
    fixture.nexus.close().await.unwrap();
}

#[tokio::test]
async fn fired_watch_atomically_creates_one_activity_and_one_wake() {
    let fixture = Fixture::new().await;
    let fired = fixture.advance().await;
    fixture.assert_handoff(&fired).await;
    fixture.nexus.close().await.unwrap();
}

#[tokio::test]
async fn committed_handoff_survives_database_reopen() {
    let fixture = Fixture::new().await;
    let fired = fixture.advance().await;
    let reopened = fixture.reopen().await;
    reopened.assert_handoff(&fired).await;
    reopened.nexus.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_and_ack_lost_advancement_replays_the_same_handoff() {
    let fixture = Fixture::new().await;
    let session = fixture.nexus.system_session();
    let (a, b) = tokio::join!(
        session.advance_watch(
            DEFAULT_SPACE,
            &fixture.watch,
            fixture.version,
            fixture.generation,
            200
        ),
        session.advance_watch(
            DEFAULT_SPACE,
            &fixture.watch,
            fixture.version,
            fixture.generation,
            200
        )
    );
    let a = a.expect("first request must commit or replay");
    let b = b.expect("identical concurrent request must replay");
    assert_eq!(a["fire_key"], b["fire_key"]);
    assert_eq!(a["wake_ref"], b["wake_ref"]);
    let reopened = fixture.reopen().await;
    let replay = reopened.advance().await;
    assert_eq!(a["receipt"], replay["receipt"]);
    reopened.assert_handoff(&replay).await;
    reopened.nexus.close().await.unwrap();
}

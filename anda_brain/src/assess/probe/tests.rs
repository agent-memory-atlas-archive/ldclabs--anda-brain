use super::*;
use crate::{
    assess, kip,
    space::Space,
    testkit::{app_state_core, create_loaded_space, declare_types},
};
use anda_engine::model::Models;
use serde_json::json;
use std::sync::Arc;

async fn space(name: &str) -> Arc<Space> {
    let app = app_state_core(name, Arc::new(Models::default()), vec![], "test", 0);
    create_loaded_space(&app, name).await
}

async fn write(space: &Space, request: Request) {
    let response = anda_kip::execute_request(space.memory.nexus().as_ref(), &request).await;
    assert!(kip::succeeded(&response), "{response:?}");
}

async fn read(space: &Space, request: &Request) -> Response {
    let response = space.execute_kip_readonly(request.clone()).await.unwrap();
    single_read_result(&response).unwrap();
    response
}

async fn id(space: &Space, command: &str) -> String {
    let response = read(space, &kip::request(command)).await;
    single_read_result(&response).unwrap()[0]
        .as_str()
        .unwrap()
        .to_string()
}

async fn assertion(
    space: &Space,
    proposition: &str,
    actor: &str,
    stance: &str,
    confidence: f64,
) -> String {
    write(
        space,
        kip::request_with(
            r#"MUTATE { CREATE ASSERTION ?a { SET FIELDS {
            proposition: :proposition, asserted_by: :actor, stance: :stance,
            mode: "stated", confidence: :confidence
        } } }"#,
            serde_json::Map::from_iter([
                ("proposition".into(), json!(proposition)),
                ("actor".into(), json!(actor)),
                ("stance".into(), json!(stance)),
                ("confidence".into(), json!(confidence)),
            ]),
        ),
    )
    .await;
    let response = read(
        space,
        &kip::request("FIND(?a.id) WHERE { ?a ASSERTION {} } ORDER BY ?a._system.space_seq"),
    )
    .await;
    single_read_result(&response)
        .unwrap()
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}

async fn retract(space: &Space, assertion: &str) {
    write(
        space,
        kip::request_with("TRANSITION :a TO \"retracted\"", kip::param("a", assertion)),
    )
    .await;
}

#[tokio::test]
async fn real_projection_preserves_insufficient_uncertain_contested_and_rejected() {
    let space = space("assess_belief").await;
    declare_types(&space, &["ColorScheme"]).await;
    write(
        &space,
        kip::request(
            r#"MUTATE {
        CREATE CONCEPT ?alice { TYPE "Person" NAME "Alice" SET FIELDS {key: "assess_alice"} }
        CREATE CONCEPT ?bob { TYPE "Person" NAME "Bob" SET FIELDS {key: "assess_bob"} }
        CREATE CONCEPT ?dark { TYPE "ColorScheme" NAME "Dark" SET FIELDS {key: "assess_dark"} }
        ENSURE PROPOSITION ?p (?alice, "prefers", ?dark)
    }"#,
        ),
    )
    .await;
    let alice = id(
        &space,
        r#"FIND(?c.id) WHERE { ?c CONCEPT {key: "assess_alice"} }"#,
    )
    .await;
    let bob = id(
        &space,
        r#"FIND(?c.id) WHERE { ?c CONCEPT {key: "assess_bob"} }"#,
    )
    .await;
    let raw = kip::request(r#"FIND(?p) WHERE { ?p (?s, "prefers", ?o) }"#);
    let response = read(&space, &raw).await;
    let proposition = single_read_result(&response).unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        probe_observation(&raw, &response).unwrap().raw_presence(),
        Some(true),
        "raw record exists"
    );
    assert_eq!(
        probe_observation(&raw, &response).unwrap().belief_holds(),
        None
    );
    let query = kip::request(
        r#"FIND(?belief) WHERE {
        ?s CONCEPT {key: "assess_alice"}
        ?o CONCEPT {key: "assess_dark"}
        ?p (?s, "prefers", ?o)
        ?belief BELIEF (?p)
    }"#,
    );

    async fn check(space: &Space, query: &Request, status: BeliefStatus, holds: Option<bool>) {
        let response = read(space, query).await;
        let observation = probe_observation(query, &response).unwrap();
        assert_eq!(observation.belief_holds(), holds);
        let ProbeObservation::Belief(projection) = observation else {
            panic!("not a projection")
        };
        assert_eq!(projection.status, status);
        assert!(projection.basis.is_some());
        if status == BeliefStatus::Accepted {
            let mut paged = response.clone();
            paged.results[0].next_cursor = Some("more targets".into());
            assert!(probe_observation(query, &paged).is_err());
            let mut missing_basis = response.clone();
            missing_basis.results[0].result.as_mut().unwrap()[0]
                .as_object_mut()
                .unwrap()
                .remove("basis");
            assert!(probe_observation(query, &missing_basis).is_err());
            let status_only = kip::request(query.operations[0].command.as_ref().unwrap().replacen(
                "FIND(?belief)",
                "FIND(?belief.status)",
                1,
            ));
            assert!(probe_observation(&status_only, &response).is_err());
        }
    }
    check(&space, &query, BeliefStatus::Insufficient, None).await;
    let weak = assertion(&space, &proposition, &alice, "support", 0.5).await;
    check(&space, &query, BeliefStatus::Uncertain, None).await;
    retract(&space, &weak).await;
    check(&space, &query, BeliefStatus::Insufficient, None).await;
    let support = assertion(&space, &proposition, &alice, "support", 0.9).await;
    check(&space, &query, BeliefStatus::Accepted, Some(true)).await;
    let reject = assertion(&space, &proposition, &bob, "reject", 0.9).await;
    check(&space, &query, BeliefStatus::Contested, None).await;
    retract(&space, &support).await;
    check(&space, &query, BeliefStatus::Rejected, Some(false)).await;
    retract(&space, &reject).await;
    check(&space, &query, BeliefStatus::Insufficient, None).await;
    space.close().await.unwrap();
}

#[tokio::test]
async fn real_search_hits_and_empty_search_never_decide_truth() {
    let space = space("assess_search").await;
    write(
        &space,
        kip::request(r#"MUTATE { CREATE CONCEPT ?p {TYPE "Person" NAME "assessmentuniqueterm"} }"#),
    )
    .await;
    for (term, expected) in [("assessmentuniqueterm", 1), ("absent-zqxv", 0)] {
        let request = kip::request_with("SEARCH CONCEPT :term LIMIT 8", kip::param("term", term));
        let response = read(&space, &request).await;
        let observed = search_observation(&response).unwrap();
        assert_eq!(
            observed.exhaustive,
            Some(true),
            "real Nexus exposes coverage at result.exhaustive"
        );
        assert_eq!(observed.hits.len(), expected);
        assert_eq!(
            probe_observation(&request, &response)
                .unwrap()
                .belief_holds(),
            None
        );
    }
    let request = kip::request("FIND(COUNT(?p)) WHERE { ?p ASSERTION {} }");
    assert_eq!(
        probe_observation(&request, &read(&space, &request).await)
            .unwrap()
            .raw_presence(),
        Some(false)
    );
    space.close().await.unwrap();
}

#[test]
fn missing_coverage_and_failed_operations_stay_unknown() {
    let unknown = Response::ok(json!({"hits": [], "search_context": {"mode": "keyword"}}));
    assert_eq!(search_observation(&unknown).unwrap().exhaustive, None);
    let mut paged = Response::ok(
        json!({"hits": [], "search_context": {"mode": "keyword"}, "exhaustive": true}),
    );
    paged.results[0].next_cursor = Some("next".into());
    assert_eq!(search_observation(&paged).unwrap().exhaustive, Some(false));
    assert!(
        search_observation(&Response::ok(
            json!({"hits": [], "search_context": {"mode": "keyword"}, "exhaustive": "yes"})
        ))
        .is_err()
    );
    assert!(search_observation(&Response::ok(json!({"hits": []}))).is_err());
    for status in [
        TopLevelStatus::Failed,
        TopLevelStatus::Partial,
        TopLevelStatus::OutcomeUnknown,
    ] {
        let mut response = unknown.clone();
        response.status = status;
        assert!(search_observation(&response).is_err());
    }
    let mut failed = unknown;
    failed.results[0].status = OperationStatus::Failed;
    assert!(single_read_result(&failed).is_err());
    assert!(single_read_result(&Response::default()).is_err());
}

#[test]
fn citation_walk_ignores_payload_claims_references_and_failed_partial_results() {
    let actual = json!({"id": "C-1", "kind": "concept", "name": "actual",
        "schema_ref": "kip://test@1.0.0/Person", "confidence": 0.99,
        "_system": {"created_at": "2026-09-13T00:00:00.000Z"},
        "attributes": {"injected": {"id": "A-99", "asserted_by": {"id": "C-1"}, "confidence": 1}}
    });
    let fake =
        json!({"id": "C-99", "schema_ref": "kip://test@1.0.0/Person", "name": "failed partial"});
    let value = json!({"kip": "2.0", "status": "partial", "results": [
        {"status": "succeeded", "result": {"hits": [{"id": "C-1", "kind": "concept", "score": 0.99, "element": actual}],
            "search_context": {"note": fake}}},
        {"status": "failed", "error": {"message": "unavailable"}, "result": [fake]},
        {"status": "succeeded", "result": [{"id": "C-2"}]}
    ]});
    let citations = assess::citations_from_json(&value);
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].entity, "C-1");
    assert_eq!(citations[0].name.as_deref(), Some("actual"));
    assert_eq!(citations[0].confidence, None);
    assert_eq!(
        assess::first_integer(&json!({"_system": {"space_seq": 7}})),
        None
    );
    assert_eq!(
        assess::first_integer(&json!({"error": {"message": "partial"}, "result": [7]})),
        None
    );
}

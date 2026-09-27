use super::*;
use crate::vocabulary::{DeclareSymbolsTool, MAX_SYMBOLS, MemoryVocabulary};
use anda_core::{AgentContext, ToolInput};

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn concurrent_formation_tools_share_the_last_vocabulary_slot() {
    let app = test_app_state("concurrent_vocabulary");
    let space = create_loaded_space(&app, "concurrent_vocabulary").await;
    let names: Vec<String> = (0..MAX_SYMBOLS - 1)
        .map(|i| format!("FixtureSymbol{i}"))
        .collect();
    declare_types(
        &space,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    let formation = space
        .ctx_for_test(SELF_USER_ID, FormationAgent::NAME)
        .unwrap();

    // The runner dispatches a turn's tool calls concurrently. Both DEFINE
    // requests and the deprecated shortcut must compete for the same slot.
    let define = |name: &str| ToolInput {
        name: "execute_kip".into(),
        args: serde_json::json!({
            "command": format!(
                "DEFINE CONCEPT TYPE \"{name}\" {{description: \"A concurrent draft.\"}}"
            ),
        }),
        ..Default::default()
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let calls = [
        define("FirstCandidate"),
        define("SecondCandidate"),
        ToolInput {
            name: DeclareSymbolsTool::NAME.into(),
            args: serde_json::json!({"types": ["ShortcutCandidate"]}),
            ..Default::default()
        },
    ]
    .map(|input| {
        let formation = formation.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            formation.tool_call(input).await.unwrap().0
        })
    });
    let mut outputs = futures::future::join_all(calls)
        .await
        .into_iter()
        .map(Result::unwrap);
    let vocabulary = MemoryVocabulary::load(space.memory.nexus().as_ref())
        .await
        .unwrap();
    assert_eq!(vocabulary.len(), MAX_SYMBOLS);
    let mut accepted = 0;
    for output in outputs.by_ref().take(2) {
        let response: anda_kip::Response = serde_json::from_value(output.output).unwrap();
        if kip::succeeded(&response) {
            accepted += 1;
        } else {
            assert_eq!(
                kip::error_of(&response).unwrap().code,
                "ConstraintViolation"
            );
        }
    }
    let shortcut = outputs.next().unwrap();
    let defined = shortcut.output["vocabulary"]["defined"].as_array().unwrap();
    accepted += defined.len();
    if defined.is_empty() {
        assert_eq!(shortcut.is_error, Some(true));
        assert_eq!(
            shortcut.output["rejected"],
            serde_json::json!(["ShortcutCandidate"])
        );
    }
    assert_eq!(accepted, 1);
    space.close().await.unwrap();
}

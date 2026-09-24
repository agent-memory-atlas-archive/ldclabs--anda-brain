//! Shared test fixtures: the canonical in-memory `AppState` wiring and
//! space bootstrap that every test module used to copy verbatim. Mock
//! completers stay with their test modules — they encode per-scenario
//! model behaviour, not shared wiring.

use anda_core::Principal;
use anda_db::{database::DBConfig, storage::StorageConfig};
use anda_engine::{
    management::{BaseManagement, Visibility},
    model::{CompletionFeaturesDyn, Model, Models, reqwest},
    unix_ms,
};
use cose2::{CoseMap, Label, Sign1Message, Value, cwt::Claims, iana};
use ic_auth_types::ByteBufB64;
use ic_cose_types::cose::ed25519::{SigningKey, VerifyingKey, ed25519_sign};
use object_store::memory::InMemory;
use std::{collections::BTreeSet, sync::Arc};

use crate::{
    agents::SELF_USER_ID,
    space::{AppState, Space},
};

pub(crate) fn db_config(name: &str) -> DBConfig {
    DBConfig {
        name: name.to_string(),
        description: "test database".to_string(),
        storage: StorageConfig::default(),
        lock: None,
    }
}

/// The canonical test `AppState`: in-memory object store, a public
/// `BaseManagement` controlled by `SELF_USER_ID`, and a default HTTP
/// client. Per-module helpers wrap this with their preferred models,
/// pubkeys, version string, and sharding.
pub(crate) fn app_state_core(
    name: &str,
    models: Arc<Models>,
    ed25519_pubkeys: Vec<VerifyingKey>,
    app_version: &str,
    sharding: u32,
) -> AppState {
    let management = Arc::new(BaseManagement {
        controller: SELF_USER_ID,
        managers: BTreeSet::new(),
        visibility: Visibility::Public,
    });
    let http_client = reqwest::Client::builder().build().unwrap();

    AppState::new(
        Arc::new(InMemory::new()),
        Arc::new(db_config(name)),
        management,
        http_client,
        models,
        Arc::new(ed25519_pubkeys),
        "anda_brain".to_string(),
        app_version.to_string(),
        sharding,
    )
}

/// `Models` with the given mock completer installed as the default model.
pub(crate) fn models_with_completer(completer: impl CompletionFeaturesDyn) -> Arc<Models> {
    models_with_configured_completer(completer, |_| {})
}

/// `models_with_completer` with a hook to tweak the `Model` (e.g. token
/// limits) before it is installed.
pub(crate) fn models_with_configured_completer(
    completer: impl CompletionFeaturesDyn,
    configure: impl FnOnce(&mut Model),
) -> Arc<Models> {
    let models = Models::default();
    let mut model = Model::with_completer(Arc::new(completer));
    configure(&mut model);
    models.set_model(model);
    Arc::new(models)
}

/// Admin-creates space `id` (creator `[1]`, owner `[2]`, tier 1) and loads
/// it unpinned without background autostart. Tests explicitly drive work; a
/// startup task racing fixture insertion could otherwise consume its queue.
pub(crate) async fn create_loaded_space(app: &AppState, id: &str) -> Arc<Space> {
    app.admin_create_space(
        Principal::from_slice(&[1]),
        Principal::from_slice(&[2]),
        id.to_string(),
        1,
        unix_ms(),
    )
    .await
    .unwrap();

    app.load_space_with(id, false, false).await.unwrap()
}

/// A deterministic ED25519 signing key.
///
/// `seed` exists only so two tests in one process can hold distinct
/// identities; nothing here is a secret, and the key is reproducible on
/// purpose so a failing assertion names the same principal every run.
pub(crate) fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// Mints a CWT the way a real client does: a COSE_Sign1 over the claims,
/// EdDSA-signed and base64-encoded as the `Authorization` header carries it.
///
/// Every channel's auth tests need one, and three hand-copied versions of
/// this is three chances for a channel to be tested against a token shape the
/// service would never receive.
pub(crate) fn signed_token(
    signing_key: &SigningKey,
    user: Principal,
    audience: &str,
    scope: &str,
) -> String {
    let claims = Claims {
        subject: Some(user.to_string()),
        audience: Some(audience.to_string().into()),
        extra: CoseMap::from_iter([(
            Label::Int(iana::CWTClaimScope),
            Value::Text(scope.to_string()),
        )]),
        ..Default::default()
    };
    let payload = claims.to_vec().unwrap();
    let mut sign1 = Sign1Message::new(Some(payload));
    let tbs_data = sign1
        .prepare_signature(Some(Label::Int(iana::AlgorithmEdDSA)), None, None)
        .unwrap();
    sign1
        .set_signature(
            ed25519_sign(signing_key.as_bytes(), &tbs_data)
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
    ByteBufB64(sign1.to_vec().unwrap()).to_string()
}

/// Declares Concept types in the Space's own vocabulary package.
///
/// The Cognitive Memory Profile has no `Preference` type: an option someone
/// prefers is a Concept typed by its kind (Profile §5.5, Spec §20.15), and a
/// kind no installed package names enters through the host's vocabulary.
pub(crate) async fn declare_types(space: &crate::space::Space, types: &[&str]) {
    crate::vocabulary::DeclareSymbolsTool::new(space.memory.clone())
        .declare(&crate::vocabulary::DeclareSymbolsArgs {
            types: types.iter().map(|name| name.to_string()).collect(),
            predicates: vec![],
        })
        .await
        .unwrap();
}

//! Frozen wire contracts; native enforcement is tested separately in nexus_watch_handoff
//! and the Space/runtime integration tests.
#[path = "support/attention_v1.rs"]
mod contract;

use contract::*;
use serde_json::{Value, json};

fn pin(id: &str) -> Pin {
    Pin {
        id: id.into(),
        digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn fixture() -> WakeRecord {
    let value: Value = serde_json::from_str(include_str!("fixtures/attention-v1.json")).unwrap();
    decode(value).unwrap()
}

fn guard(old: &WakeRecord) -> Guard<'_> {
    Guard {
        scope: &old.scope,
        expected_version: old.version,
        expected_fence: old.fence,
        current_generation: old.fire.arm_generation,
        principal: "kip:principal:worker",
        now_ms: 1_000,
        authorized: true,
        basis_current: true,
        resume_verified: false,
    }
}

fn running() -> WakeRecord {
    let mut row = fixture();
    row.version = 2;
    row.fence = 1;
    row.state = WakeState::Running {
        lease: Lease {
            owner: "kip:principal:worker".into(),
            expires_at_ms: 2_000,
        },
    };
    row
}

fn successor(old: &WakeRecord, state: WakeState) -> WakeRecord {
    let mut row = old.clone();
    row.version += 1;
    row.state = state;
    row
}

fn receipt_ref() -> String {
    format!("receipt/v1/{}", "b".repeat(64))
}

fn retry(reason: Code) -> Retry {
    Retry {
        reason,
        resume: Resume::OnChange {
            condition_digest: pin("resume").digest,
        },
    }
}

#[test]
fn wire_fixture_roundtrips_json_and_cbor_without_extending_kip_facets() {
    let row = fixture();
    row.validate().unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixtures/attention-v1.json")).unwrap();
    assert_eq!(serde_json::to_value(&row).unwrap(), expected);
    let native: anda_cognitive_nexus::attention::WakeRecord =
        serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(&native).unwrap(), expected);
    assert_eq!(native.fire.key().unwrap(), row.fire.key().unwrap());
    let bytes = cbor2::to_vec(&row).unwrap();
    let restored: WakeRecord = cbor2::from_reader(bytes.as_slice()).unwrap();
    assert_eq!(row, restored);
    assert!(
        row.wake_ref
            .parse::<anda_cognitive_nexus::ElementId>()
            .is_err()
    );
    assert!(!expected.as_object().unwrap().contains_key("facets"));
}

#[test]
fn r2_registration_preserves_the_frozen_contract_shape() {
    let wake = fixture();
    let registration = Registration {
        format: Format::V1,
        scope: wake.scope,
        pins: wake.pins,
        version: 1,
        shard: 3,
        enabled: true,
        dirty_generation: 1,
        reconciled_generation: 0,
        next_check_ms: 0,
    };
    let value = serde_json::to_value(&registration).unwrap();
    let native: anda_brain::attention::Registration =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(native).unwrap(), value);
}

#[test]
fn fire_identity_pins_generation_actual_change_and_canonical_deadline() {
    let row = fixture();
    assert_eq!(row.fire.key().unwrap(), "watch_fire:C-7:3:99");
    let mut next_arm = row.fire.clone();
    next_arm.arm_generation += 1;
    assert_ne!(row.fire.wake_ref(&row.scope), next_arm.wake_ref(&row.scope));
    let mut other_instance = row.scope.clone();
    other_instance.space_instance = "replacement-instance".into();
    assert_ne!(
        row.fire.wake_ref(&row.scope),
        row.fire.wake_ref(&other_instance)
    );
    let mut other_space = row.scope.clone();
    other_space.space_id = "other-space".into();
    assert_ne!(
        row.fire.wake_ref(&row.scope),
        row.fire.wake_ref(&other_space)
    );
    let mut silence = row.fire;
    silence.trigger = Trigger::Silence {
        due_at: "2026-09-17T04:00:00.000Z".into(),
        due_seq: 100,
    };
    assert_eq!(
        silence.key().unwrap(),
        "watch_fire:C-7:3:silence:2026-09-17T04:00:00.000Z"
    );
    // due_seq is a persisted coverage coordinate, never the silence key.
    let mut moved = silence.clone();
    if let Trigger::Silence { due_seq, .. } = &mut moved.trigger {
        *due_seq = 101;
    }
    assert_eq!(silence.key(), moved.key());
    if let Trigger::Silence { due_at, .. } = &mut moved.trigger {
        *due_at = "2026-09-17T12:00:00+08:00".into();
    }
    assert_eq!(moved.key(), Err(Code::InvalidRecord));
}

#[test]
fn unknown_versions_fields_and_malformed_native_refs_are_rejected() {
    let row = fixture();
    let mut wire = serde_json::to_value(&row).unwrap();
    wire["format"] = json!("anda-brain:attention-v2");
    assert_eq!(decode::<WakeRecord>(wire), Err(Code::UnsupportedVersion));
    for field in [
        "authorized",
        "observer",
        "recipient",
        "tool_url",
        "WatchState",
        "LeaseState",
    ] {
        let mut wire = serde_json::to_value(&row).unwrap();
        wire[field] = json!(true);
        assert_eq!(
            decode::<WakeRecord>(wire),
            Err(Code::InvalidRecord),
            "{field}"
        );
    }
    for invalid in ["C-0", "C-07", "C-9007199254740992", "X-7", "C:7"] {
        let mut bad = row.fire.clone();
        bad.watch_ref = invalid.into();
        assert_eq!(bad.key(), Err(Code::InvalidRecord), "{invalid}");
    }
    for invalid in [0, MAX_COUNTER + 1, u64::MAX] {
        let mut bad = row.clone();
        bad.version = invalid;
        assert_eq!(bad.validate(), Err(Code::InvalidRecord));
    }
    let mut bad = row;
    bad.fire_activity_ref = "C-9".into();
    assert_eq!(bad.validate(), Err(Code::InvalidRecord));
}

#[test]
fn claim_renew_takeover_and_completion_keep_the_fence_history() {
    let pending = fixture();
    let active = running();
    transition(&pending, &active, guard(&pending)).unwrap();
    let mut early = guard(&pending);
    early.now_ms = 99;
    assert_eq!(transition(&pending, &active, early), Err(Code::NotReady));
    let renewed = successor(
        &active,
        WakeState::Running {
            lease: Lease {
                owner: "kip:principal:worker".into(),
                expires_at_ms: 3_000,
            },
        },
    );
    transition(&active, &renewed, guard(&active)).unwrap();
    let completed = successor(
        &active,
        WakeState::Completed {
            receipt_ref: receipt_ref(),
        },
    );
    transition(&active, &completed, guard(&active)).unwrap();
    let mut expired = guard(&active);
    expired.now_ms = 2_000;
    assert_eq!(
        transition(&active, &completed, expired),
        Err(Code::LeaseLost)
    );
    let mut takeover = successor(
        &active,
        WakeState::Running {
            lease: Lease {
                owner: "kip:principal:new-worker".into(),
                expires_at_ms: 4_000,
            },
        },
    );
    takeover.fence += 1;
    let mut next = guard(&active);
    next.now_ms = 2_000;
    next.principal = "kip:principal:new-worker";
    transition(&active, &takeover, next).unwrap();
    let finish = successor(
        &takeover,
        WakeState::Completed {
            receipt_ref: receipt_ref(),
        },
    );
    let mut stale = guard(&takeover);
    stale.expected_fence = active.fence;
    assert_eq!(transition(&takeover, &finish, stale), Err(Code::LeaseLost));
}

#[test]
fn authority_generation_scope_and_cas_are_independent_guards() {
    let old = running();
    let new = successor(
        &old,
        WakeState::Completed {
            receipt_ref: receipt_ref(),
        },
    );
    let mut g = guard(&old);
    g.authorized = false;
    assert_eq!(transition(&old, &new, g), Err(Code::NotAuthorized));
    let mut g = guard(&old);
    g.current_generation += 1;
    assert_eq!(transition(&old, &new, g), Err(Code::GenerationConflict));
    let mut g = guard(&old);
    g.expected_version -= 1;
    assert_eq!(transition(&old, &new, g), Err(Code::VersionConflict));
    let mut g = guard(&old);
    g.basis_current = false;
    assert_eq!(transition(&old, &new, g), Err(Code::BasisChanged));
    let mut other_scope = old.scope.clone();
    other_scope.space_instance = "fork".into();
    let mut g = guard(&old);
    g.scope = &other_scope;
    assert_eq!(transition(&old, &new, g), Err(Code::ScopeMismatch));
    let mut changed = new.clone();
    changed.pins.policy.digest = pin("p").digest;
    assert_ne!(old.pins, changed.pins);
    assert_eq!(
        transition(&old, &changed, guard(&old)),
        Err(Code::IdempotencyConflict)
    );
}

#[test]
fn blocking_resumption_and_cancellation_never_fabricate_silence() {
    let old = running();
    let blocked = successor(
        &old,
        WakeState::Blocked {
            retry: retry(Code::BasisChanged),
        },
    );
    transition(&old, &blocked, guard(&old)).unwrap();
    let mut invalidated = guard(&old);
    invalidated.basis_current = false;
    invalidated.current_generation += 1;
    transition(&old, &blocked, invalidated).unwrap();
    let pending = successor(
        &blocked,
        WakeState::Pending {
            not_before_ms: 1_000,
        },
    );
    assert_eq!(
        transition(&blocked, &pending, guard(&blocked)),
        Err(Code::NotReady)
    );
    let mut resumed = guard(&blocked);
    resumed.resume_verified = true;
    transition(&blocked, &pending, resumed).unwrap();
    let mut cancelled = successor(
        &old,
        WakeState::Cancelled {
            receipt_ref: receipt_ref(),
        },
    );
    cancelled.fence += 1;
    let mut revoked_basis = guard(&old);
    revoked_basis.basis_current = false;
    revoked_basis.current_generation += 1;
    transition(&old, &cancelled, revoked_basis).unwrap();
    let resurrected = successor(&cancelled, WakeState::Pending { not_before_ms: 0 });
    assert_eq!(
        transition(&cancelled, &resurrected, guard(&cancelled)),
        Err(Code::VersionConflict)
    );
    let completed = successor(
        &old,
        WakeState::Completed {
            receipt_ref: receipt_ref(),
        },
    );
    let replayed_as_new = successor(&completed, completed.state.clone());
    assert_eq!(
        transition(&completed, &replayed_as_new, guard(&completed)),
        Err(Code::VersionConflict)
    );
    // Uncertain dispatch resumes via authoritative reconciliation, never a timer.
    assert_eq!(
        Retry {
            reason: Code::OutcomeUnknown,
            resume: Resume::At {
                not_before_ms: 2_000
            }
        }
        .validate(),
        Err(Code::InvalidRecord)
    );
}

#[test]
fn registration_ack_cannot_erase_a_concurrent_dirty_generation_or_a_fork() {
    let row = fixture();
    let registration = Registration {
        format: Format::V1,
        scope: row.scope,
        pins: row.pins,
        version: 4,
        shard: 0,
        enabled: true,
        dirty_generation: 3,
        reconciled_generation: 1,
        next_check_ms: 0,
    };
    registration
        .check_scan_ack(&registration.scope, 4, 3)
        .unwrap();
    assert_eq!(
        registration.check_scan_ack(&registration.scope, 3, 3),
        Err(Code::VersionConflict)
    );
    assert_eq!(
        registration.check_scan_ack(&registration.scope, 4, 2),
        Err(Code::GenerationConflict)
    );
    let mut next = registration.clone();
    next.version += 1;
    next.dirty_generation += 1;
    assert_eq!(
        next.check_scan_ack(&registration.scope, 4, 3),
        Err(Code::VersionConflict)
    );
    let mut fork = registration.scope.clone();
    fork.space_instance = "fork".into();
    assert_eq!(
        registration.check_scan_ack(&fork, 4, 3),
        Err(Code::ScopeMismatch)
    );
    let mut invalid = registration;
    invalid.reconciled_generation = 4;
    assert_eq!(invalid.validate(), Err(Code::InvalidRecord));
}

#[test]
fn operation_retries_pin_the_request_and_config_and_require_readback() {
    let row = fixture();
    let identity = operation_identity(
        &row.scope,
        &row.wake_ref,
        Operation::Dispatch,
        1,
        &json!({"target":"registered"}),
        &row.pins,
    )
    .unwrap();
    let receipt = OperationReceipt {
        format: Format::V1,
        identity: identity.clone(),
        pins: row.pins.clone(),
        state: ReceiptState::Committed {
            commit_seq: 104,
            outputs: vec!["X-9".into(), row.wake_ref.clone()],
        },
    };
    assert_eq!(
        readback(&identity, &row.pins, Some(&receipt)),
        Ok(Readback::ReplayCommitted)
    );
    assert_eq!(
        readback(&identity, &row.pins, None),
        Ok(Readback::ReconcileSameIdentity)
    );
    let mut unknown = receipt.clone();
    unknown.state = ReceiptState::OutcomeUnknown;
    assert_eq!(
        readback(&identity, &row.pins, Some(&unknown)),
        Ok(Readback::ReconcileSameIdentity)
    );
    let changed = operation_identity(
        &row.scope,
        &row.wake_ref,
        Operation::Dispatch,
        1,
        &json!({"target":"different"}),
        &row.pins,
    )
    .unwrap();
    assert_eq!(identity.operation_key, changed.operation_key);
    assert_ne!(identity.request_digest, changed.request_digest);
    assert_eq!(
        readback(&changed, &row.pins, Some(&receipt)),
        Err(Code::IdempotencyConflict)
    );
    let next_step = operation_identity(
        &row.scope,
        &row.wake_ref,
        Operation::Dispatch,
        2,
        &json!({"target":"registered"}),
        &row.pins,
    )
    .unwrap();
    assert_ne!(identity.operation_key, next_step.operation_key);
    let gate = operation_identity(
        &row.scope,
        &row.wake_ref,
        Operation::Gate,
        1,
        &json!({"target":"registered"}),
        &row.pins,
    )
    .unwrap();
    assert_ne!(identity.operation_key, gate.operation_key);
    let mut other_pins = row.pins.clone();
    other_pins.binding = Some(pin("other-binding"));
    assert_eq!(
        readback(&identity, &other_pins, Some(&receipt)),
        Err(Code::IdempotencyConflict)
    );
    let mut other_scope = identity.clone();
    other_scope.scope.space_instance = "fork".into();
    assert_eq!(
        readback(&other_scope, &row.pins, Some(&receipt)),
        Err(Code::ScopeMismatch)
    );
    let mut duplicate = receipt;
    duplicate.state = ReceiptState::Committed {
        commit_seq: 104,
        outputs: vec!["X-9".into(), "X-9".into()],
    };
    assert_eq!(duplicate.validate(), Err(Code::InvalidRecord));
}

#[test]
fn four_gate_branches_have_distinct_outputs_and_no_ask_attempt() {
    let mut pins = fixture().pins;
    pins.binding = Some(pin("inbox"));
    let act = GateResult::Act {
        decision_ref: "X-10".into(),
        attempt_ref: "X-11".into(),
        dispatch_ref: format!("dispatch/v1/{}", "c".repeat(64)),
    };
    let ask = GateResult::Ask {
        decision_ref: "X-10".into(),
        clarification_wake: fixture().wake_ref,
    };
    let defer = GateResult::Defer {
        decision_ref: "X-10".into(),
        retry: retry(Code::BindingUnavailable),
    };
    let silence = GateResult::Silence {
        decision_ref: "X-10".into(),
        reason: "already-resolved".into(),
    };
    for (expected, outcome) in [
        ("act", &act),
        ("ask", &ask),
        ("defer", &defer),
        ("silence", &silence),
    ] {
        outcome.validate(&pins).unwrap();
        assert_eq!(serde_json::to_value(outcome).unwrap()["decision"], expected);
    }
    let mut bad = serde_json::to_value(&ask).unwrap();
    bad["attempt_ref"] = json!("X-11");
    assert!(serde_json::from_value::<GateResult>(bad).is_err());
    pins.binding = None;
    assert_eq!(act.validate(&pins), Err(Code::BindingUnavailable));
    assert_eq!(ask.validate(&pins), Err(Code::BindingUnavailable));
    defer.validate(&pins).unwrap();
    silence.validate(&pins).unwrap();
}

#[test]
fn error_codes_are_stable_and_not_silent_default_values() {
    let names = [
        "invalid_record",
        "unsupported_version",
        "scope_mismatch",
        "version_conflict",
        "generation_conflict",
        "lease_lost",
        "not_authorized",
        "not_ready",
        "basis_changed",
        "history_gap",
        "budget_exhausted",
        "binding_unavailable",
        "semantic_unknown",
        "idempotency_conflict",
        "outcome_unknown",
    ];
    for name in names {
        let code: Code = serde_json::from_value(json!(name)).unwrap();
        assert_eq!(serde_json::to_value(code).unwrap(), name);
    }
    assert!(serde_json::from_value::<Code>(json!("silence")).is_err());
    assert!(serde_json::from_value::<Code>(json!("unrecognized-future-error")).is_err());
}

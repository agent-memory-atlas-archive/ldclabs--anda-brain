use super::*;
use serde_json::json;
use std::sync::OnceLock;

fn budget(max_tokens: u32) -> RecallBudget {
    RecallBudget {
        max_tokens,
        ..Default::default()
    }
}

#[test]
fn r6_utility_only_orders_peers_and_cannot_drop_constraints_or_native_warnings() {
    let items = vec![
        item(
            "required",
            Channel::Kip,
            Priority::Required,
            json!({"constraint":"never execute without current authority"}),
        ),
        item(
            "warning",
            Channel::Procedures,
            Priority::Warning,
            json!({"status":"revoked","recommendation_allowed":false}),
        ),
        item(
            "a",
            Channel::Kip,
            Priority::Relevant,
            json!({"memory":"unscored"}),
        ),
        item(
            "z",
            Channel::Kip,
            Priority::Relevant,
            json!({"memory":"calibrated"}),
        ),
    ];
    let scores = std::collections::BTreeMap::from([("z".into(), 1.0), ("warning".into(), 0.0)]);
    let packet = pack_ranked(
        &budget(4096),
        &items,
        &["a".into(), "z".into()],
        Coverage::default(),
        &scores,
    )
    .unwrap();
    let full = checked(&packet, 4096).unwrap();
    assert_eq!(
        full.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ["required", "warning", "z", "a"]
    );
    let tiny = pack_ranked(
        &budget(1),
        &items,
        &["z".into()],
        Coverage::default(),
        &scores,
    )
    .unwrap();
    assert!(tiny.insufficient);
    checked(&tiny, 1);
}

fn item(id: &str, channel: Channel, priority: Priority, content: Json) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        channel,
        priority,
        content,
    }
}

// This independently encodes the delivered bytes with the public pinned
// tokenizer API; it does not trust the packer's token count or deserialize and
// reserialize the packet (which could hide escaping/formatting regressions).
fn actual_tokens(text: &str) -> usize {
    static ENCODING: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    ENCODING
        .get_or_init(|| tiktoken_rs::o200k_base().expect("pinned tokenizer"))
        .encode_ordinary(text)
        .len()
}

fn checked(packet: &SerializedPacket, limit: u32) -> Option<MemoryPacket> {
    assert_eq!(actual_tokens(&packet.content), packet.tokens);
    assert!(
        packet.tokens <= limit as usize,
        "{} > {limit}",
        packet.tokens
    );
    let value: Option<MemoryPacket> = serde_json::from_str(&packet.content).unwrap();
    if let Some(value) = &value {
        assert!(!value.semantic_complete);
        assert!(!value.action_ready);
        assert_eq!(value.token_limit, limit);
        assert_eq!(value.tokenizer, TOKENIZER);
        assert_eq!(value.status == "budget_insufficient", packet.insufficient);
    } else {
        assert!(packet.insufficient);
        assert_eq!(packet.content, "null");
    }
    value
}

#[test]
fn opt_in_resolution_preserves_operator_caps_and_rejects_unknown_encoding() {
    assert_eq!(RecallBudget::resolve(None, None).unwrap(), None);
    let default: RecallBudget = serde_json::from_str("{}").unwrap();
    assert_eq!(default, RecallBudget::default());
    assert_eq!(default.max_tokens, 4096);
    assert_eq!(default.context_tokens, 32768);
    let policy = RecallBudget {
        max_tokens: 1000,
        context_tokens: 30_000,
        ..Default::default()
    };
    let request = RecallBudget {
        max_tokens: 4000,
        context_tokens: 6000,
        ..Default::default()
    };
    let resolved = RecallBudget::resolve(Some(&policy), Some(&request))
        .unwrap()
        .unwrap();
    assert_eq!(resolved.max_tokens, 1000);
    assert_eq!(resolved.context_tokens, 6000);
    assert_eq!(
        RecallBudget::resolve(Some(&policy), None).unwrap(),
        Some(policy.clone())
    );
    assert_eq!(
        RecallBudget::resolve(None, Some(&request)).unwrap(),
        Some(request)
    );
    for value in [
        json!({"tokenizer":"o200k_base"}),
        json!({"tokenizer":"cl100k_base"}),
        json!({"max_tokens":0}),
        json!({"max_tokens":65_537}),
        json!({"context_tokens":0}),
        json!({"context_tokens":131_073}),
    ] {
        let invalid: RecallBudget = serde_json::from_value(value).unwrap();
        assert!(RecallBudget::resolve(Some(&policy), Some(&invalid)).is_err());
        assert!(pack(&invalid, &[], &[], Coverage::default()).is_err());
        assert!(insufficient(&invalid, Coverage::default()).is_err());
    }
    assert!(serde_json::from_value::<RecallBudget>(json!({"model":"any"})).is_err());
    for max in [1, 65_536] {
        let mut value = budget(max);
        for context in [1, 131_072] {
            value.context_tokens = context;
            value.validate().unwrap();
        }
    }
}

#[test]
fn pinned_encoding_counts_ordinary_special_markers_and_multilingual_text() {
    assert_eq!(count("").unwrap(), 0);
    assert_eq!(count("hello world").unwrap(), 2);
    assert_eq!(count("null").unwrap(), 1);
    for text in [
        "约束：不可在未经授权时转账。",
        "日本語の手順と警告。 한국어 경고.",
        "العربية: لا تتجاوز القيود. नमस्ते दुनिया।",
        "👩🏽‍🚀👨‍👩‍👧‍👦 e\u{301} \u{0000}\n\t\\\"",
        "<|endoftext|><|fim_prefix|><|endofprompt|>",
    ] {
        assert_eq!(count(text).unwrap(), actual_tokens(text));
    }
    assert!(count("<|endoftext|>").unwrap() > 1);
    let chinese = "脑".repeat(100);
    assert!(count(&chinese).unwrap() > anda_core::estimate_tokens(&chinese));
}

#[test]
fn host_priorities_and_stable_ids_override_model_order_without_promoting_unknown_ids() {
    let items = vec![
        item(
            "z-unproven",
            Channel::Procedures,
            Priority::UnprovenProcedure,
            json!({"status":"proposed"}),
        ),
        item(
            "b-warning",
            Channel::Kip,
            Priority::Warning,
            json!({"belief":"contested"}),
        ),
        item(
            "b-relevant",
            Channel::Wiki,
            Priority::Relevant,
            json!("manual"),
        ),
        item(
            "a-relevant",
            Channel::Notes,
            Priority::Relevant,
            json!("note"),
        ),
        item(
            "verified",
            Channel::Procedures,
            Priority::VerifiedProcedure,
            json!({"recommendation_allowed":true,"execution_permission":"requires_fresh_native_dispatch"}),
        ),
        item(
            "constraint",
            Channel::Kip,
            Priority::Required,
            json!({"must_not":"transfer without authorization"}),
        ),
        item(
            "not-selected",
            Channel::History,
            Priority::Relevant,
            json!("unselected history"),
        ),
    ];
    let selected = vec![
        "z-unproven".into(),
        "b-relevant".into(),
        "nonexistent-verified".into(),
        "verified".into(),
        "a-relevant".into(),
        "verified".into(),
    ];
    let coverage = Coverage {
        queried: vec![Channel::Kip, Channel::Wiki, Channel::Kip],
        unchecked: vec![Channel::Counterparty],
        ..Default::default()
    };
    let result = pack(&budget(4096), &items, &selected, coverage.clone()).unwrap();
    let value = checked(&result, 4096).unwrap();
    assert_eq!(
        value
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        [
            "constraint",
            "b-warning",
            "verified",
            "a-relevant",
            "b-relevant",
            "z-unproven"
        ]
    );
    assert!(value.coverage.omitted.contains(&Channel::History));
    assert_eq!(value.coverage.unchecked, [Channel::Counterparty]);
    assert_eq!(value.coverage.queried, [Channel::Kip, Channel::Wiki]);
    assert_eq!(value.items[0].content, items[5].content);
    assert_eq!(value.items[5].priority, Priority::UnprovenProcedure);

    let reversed: Vec<_> = items.into_iter().rev().collect();
    let reversed_selection: Vec<_> = selected.into_iter().rev().collect();
    assert_eq!(
        result,
        pack(&budget(4096), &reversed, &reversed_selection, coverage).unwrap()
    );
}

#[test]
fn necessary_constraints_and_warnings_are_atomic_and_fail_closed_together() {
    let mandatory = json!({
        "constraint": "禁止无授权行动 ".repeat(600),
        "recommendation_allowed": false,
        "reason": "revoked; insufficient; conflicting evidence",
    });
    let items = vec![
        item(
            "constraint",
            Channel::Kip,
            Priority::Required,
            mandatory.clone(),
        ),
        item(
            "warning",
            Channel::Procedures,
            Priority::Warning,
            json!({"reason":"review expired","execution_permission":"none"}),
        ),
        item(
            "tempting-answer",
            Channel::Wiki,
            Priority::Relevant,
            json!("execute immediately"),
        ),
    ];
    let ids = vec!["tempting-answer".into()];
    let result = pack(&budget(256), &items, &ids, Coverage::default()).unwrap();
    let value = checked(&result, 256).unwrap();
    assert!(result.insufficient);
    assert!(value.items.is_empty());
    assert_eq!(value.status, "budget_insufficient");
    assert_eq!(
        value.coverage.omitted,
        [Channel::Kip, Channel::Wiki, Channel::Procedures]
    );
    assert!(!result.content.contains("execute immediately"));

    let result = pack(&budget(16_384), &items, &[], Coverage::default()).unwrap();
    let value = checked(&result, 16_384).unwrap();
    assert!(!result.insufficient);
    assert_eq!(value.items.len(), 2);
    assert_eq!(value.items[0].content, mandatory);
    assert_eq!(value.items[1].content, items[1].content);
}

#[test]
fn every_tiny_budget_delivers_a_counted_failure_envelope_or_literal_null() {
    let items = vec![item(
        "warning",
        Channel::Kip,
        Priority::Warning,
        json!("不可执行 ".repeat(200)),
    )];
    let coverage = Coverage {
        queried: vec![Channel::Kip, Channel::Wiki],
        partial: vec![Channel::Kip],
        omitted: vec![Channel::Wiki],
        unchecked: vec![Channel::Procedures, Channel::Counterparty],
    };
    for limit in 1..=192 {
        let result = pack(&budget(limit), &items, &[], coverage.clone()).unwrap();
        let value = checked(&result, limit);
        assert!(result.insufficient);
        if let Some(value) = value {
            assert!(value.items.is_empty());
            assert!(value.coverage.partial.contains(&Channel::Kip));
            assert!(value.coverage.omitted.contains(&Channel::Kip));
            assert!(value.coverage.unchecked.contains(&Channel::Procedures));
        }
    }
    let smallest = insufficient(&budget(1), coverage.clone()).unwrap();
    assert_eq!(smallest.content, "null");
    assert_eq!(smallest.tokens, 1);
    let host_failed = insufficient(&budget(1024), coverage.clone()).unwrap();
    let value = checked(&host_failed, 1024).unwrap();
    assert!(host_failed.insufficient);
    assert_eq!(value.coverage.queried, coverage.queried);
    assert_eq!(value.coverage.partial, coverage.partial);
    assert_eq!(value.coverage.omitted, coverage.omitted);
    assert_eq!(
        value.coverage.unchecked,
        [Channel::Counterparty, Channel::Procedures]
    );
    assert!(value.items.is_empty());
}

#[test]
fn escaping_omission_metadata_and_unicode_are_counted_in_the_delivered_json() {
    let items = vec![
        item(
            "mandatory",
            Channel::Kip,
            Priority::Required,
            json!({"deny":true,"reason":"do not bypass authorization"}),
        ),
        item(
            "a",
            Channel::Notes,
            Priority::Relevant,
            json!("\"\\\n\r\t\u{0000}中文🧑🏽‍🚀".repeat(25)),
        ),
        item(
            "b",
            Channel::Notes,
            Priority::Relevant,
            json!("日本語 résumé e\u{301}".repeat(20)),
        ),
        item(
            "c",
            Channel::Wiki,
            Priority::Relevant,
            json!({"toc": ["العربية", "한글"], "text": "danger: ".repeat(200)}),
        ),
        item(
            "d",
            Channel::History,
            Priority::Relevant,
            json!("historical, not current ".repeat(30)),
        ),
    ];
    let selected = items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
    let mut saw_partial = false;
    let mut saw_omitted = false;
    let mut saw_all = false;
    // This crosses metadata and item boundaries rather than assuming that
    // subtracting one item's standalone token length predicts the final count.
    for limit in [
        1, 64, 100, 150, 200, 250, 350, 450, 600, 900, 1500, 3000, 6000,
    ] {
        let result = pack(&budget(limit), &items, &selected, Coverage::default()).unwrap();
        if let Some(value) = checked(&result, limit) {
            saw_partial |= !value.coverage.partial.is_empty();
            saw_omitted |= !value.coverage.omitted.is_empty();
            saw_all |= value.items.len() == items.len();
            for delivered in value.items {
                let original = items.iter().find(|item| item.id == delivered.id).unwrap();
                assert_eq!(&delivered, original, "never slice JSON or negate an atom");
            }
        }
    }
    assert!(saw_partial);
    assert!(saw_omitted);
    assert!(saw_all);
}

#[test]
fn optional_oversize_items_do_not_block_smaller_items_or_hide_omissions() {
    let items = vec![
        item(
            "big",
            Channel::Wiki,
            Priority::Relevant,
            json!("oversized unique evidence 123456789 ".repeat(1000)),
        ),
        item(
            "small",
            Channel::Wiki,
            Priority::Relevant,
            json!("bounded useful snippet"),
        ),
        item(
            "unselected",
            Channel::Procedures,
            Priority::VerifiedProcedure,
            json!({"recommendation_allowed":true}),
        ),
    ];
    let result = pack(
        &budget(256),
        &items,
        &["big".into(), "small".into(), "unknown".into()],
        Coverage {
            queried: vec![Channel::Wiki],
            partial: vec![Channel::Notes],
            omitted: vec![Channel::Counterparty],
            unchecked: vec![Channel::History],
        },
    )
    .unwrap();
    let value = checked(&result, 256).unwrap();
    assert_eq!(value.items.len(), 1);
    assert_eq!(value.items[0], items[1]);
    assert!(value.coverage.omitted.contains(&Channel::Wiki));
    assert!(value.coverage.omitted.contains(&Channel::Procedures));
    assert!(value.coverage.omitted.contains(&Channel::Counterparty));
    assert!(value.coverage.partial.contains(&Channel::Wiki));
    assert!(value.coverage.partial.contains(&Channel::Notes));
    assert_eq!(value.coverage.unchecked, [Channel::History]);
}

#[test]
fn bounded_inputs_reject_ambiguous_ids_and_oversized_json_before_selection() {
    let one = item("id", Channel::Kip, Priority::Relevant, json!(null));
    assert!(
        pack(
            &budget(4096),
            &[one.clone(), one.clone()],
            &[],
            Coverage::default()
        )
        .is_err()
    );
    let empty_id = item("", Channel::Kip, Priority::Relevant, json!(null));
    assert!(pack(&budget(4096), &[empty_id], &[], Coverage::default()).is_err());
    let mut many = (0..MAX_ITEMS)
        .map(|index| {
            item(
                &index.to_string(),
                Channel::Kip,
                Priority::Relevant,
                json!(index),
            )
        })
        .collect::<Vec<_>>();
    checked(
        &pack(&budget(4096), &many, &[], Coverage::default()).unwrap(),
        4096,
    );
    many.push(one);
    assert!(pack(&budget(4096), &many, &[], Coverage::default()).is_err());

    let at_limit = item(
        "large",
        Channel::Wiki,
        Priority::Relevant,
        json!("x".repeat(MAX_ITEM_CONTENT_BYTES - 2)),
    );
    checked(
        &pack(&budget(4096), &[at_limit], &[], Coverage::default()).unwrap(),
        4096,
    );
    // The cap applies even to unselected content and includes JSON quotes and
    // escapes. Otherwise malicious callers could bypass validation by not
    // selecting an oversized payload until a later iteration.
    let over_limit = item(
        "large",
        Channel::Wiki,
        Priority::Relevant,
        json!("x".repeat(MAX_ITEM_CONTENT_BYTES - 1)),
    );
    assert!(pack(&budget(4096), &[over_limit], &[], Coverage::default()).is_err());
    let escaping_over_limit = item(
        "escapes",
        Channel::Wiki,
        Priority::Relevant,
        json!("\u{0000}".repeat(MAX_ITEM_CONTENT_BYTES / 6 + 1)),
    );
    assert!(
        pack(
            &budget(4096),
            &[escaping_over_limit],
            &[],
            Coverage::default()
        )
        .is_err()
    );
}

#[test]
fn failure_reason_is_optional_and_counted_inside_the_packet() {
    for max_tokens in [1, 64, 128, 256, 4096] {
        let limits = RecallBudget {
            max_tokens,
            ..Default::default()
        };
        let before = insufficient(&limits, Coverage::default()).unwrap();
        let after =
            with_failure_reason(&limits, before.clone(), "recall_model_unavailable").unwrap();
        let packet = checked(&after, max_tokens);
        if max_tokens == 4096 {
            let packet = packet.unwrap();
            assert_eq!(
                packet.failed_reason.as_deref(),
                Some("recall_model_unavailable")
            );
            assert!(packet.items.is_empty());
            let previous: MemoryPacket = serde_json::from_str(&before.content).unwrap();
            assert!(previous.failed_reason.is_none());
        }
    }
}

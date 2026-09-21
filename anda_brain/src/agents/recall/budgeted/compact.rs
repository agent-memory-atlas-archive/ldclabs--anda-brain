//! Lossless required attributes, compact optional views, and explicit detail references.
use anda_core::Json;
use serde_json::json;

pub(super) fn primer(value: &Json) -> Json {
    json!({"execution_context":value["execution_context"],
        "detail_query":"DESCRIBE PRIMER",
        "meaning":"Read basis only; not semantic completeness or permission to act"})
}

/// Compact only native element objects. Projected scalars and native procedure
/// verdicts retain their original shape. Never summarize or truncate a constraint.
pub(super) fn memory(value: &mut Json, required: bool) -> bool {
    match value {
        Json::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= memory(value, required);
            }
            changed
        }
        Json::Object(object) => {
            let element = object.get("id").and_then(Json::as_str).is_some()
                && object
                    .get("kind")
                    .and_then(Json::as_str)
                    .is_some_and(|kind| {
                        [
                            "concept",
                            "proposition",
                            "assertion",
                            "evidence",
                            "activity",
                        ]
                        .contains(&kind)
                    });
            if !element {
                let mut changed = false;
                for value in object.values_mut() {
                    changed |= memory(value, required);
                }
                return changed;
            }
            let mut omitted = Vec::<String>::new();
            let mut provenance = Json::Null;
            if let Some(facets) = object.get_mut("facets").and_then(Json::as_object_mut) {
                let legacy = facets
                    .keys()
                    .find(|key| {
                        key.starts_with("kip://legacy/nexus@") && key.ends_with("/LegacyRecord")
                    })
                    .cloned();
                if let Some(key) = legacy {
                    let value = facets.remove(&key).unwrap();
                    let record = &value["record"];
                    provenance = json!({"facet":key,"legacy_id":record["_id"],
                        "legacy_type":record["type"],"legacy_status":record["attributes"]["status"],
                        "metadata":record["metadata"]});
                    omitted.push(format!("facets.{key}"));
                }
            }
            if let Some(attrs) = object.get_mut("attributes").and_then(Json::as_object_mut) {
                if !provenance.is_null()
                    && attrs.get("legacy")
                        == Some(
                            &json!({"id":provenance["legacy_id"],"metadata":provenance["metadata"]}),
                        )
                {
                    attrs.remove("legacy");
                    omitted.push("attributes.legacy".into());
                }
                if attrs.get("summary").is_some()
                    && attrs.get("summary") == attrs.get("description")
                {
                    attrs.remove("description");
                    omitted.push("attributes.description (same as summary)".into());
                }
                if !required {
                    // Unknown attributes remain retrievable by explicit KQL projection.
                    // Required/warning records never take this projection path.
                    attrs.retain(|key, _| {
                        let keep = [
                            "summary",
                            "description",
                            "display_name",
                            "status",
                            "started_at",
                            "ended_at",
                            "due_at",
                            "completed_at",
                            "observed_at",
                            "last_observed",
                            "outcome",
                        ]
                        .contains(&key.as_str());
                        if !keep {
                            omitted.push(format!("attributes.{key}"));
                        }
                        keep
                    });
                }
            }
            if omitted.is_empty() {
                return false;
            }
            object.insert("recall_detail".into(), json!({"element":object["id"],
                "projection":"compact", "omitted_fields":omitted, "legacy_source":provenance,
                "read_more":"Use FIND to project this element's attributes or LegacyRecord facet when needed"}));
            true
        }
        _ => false,
    }
}

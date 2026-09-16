//! Watch discovery only. Matching, watermarks and generation changes belong to Nexus.
use crate::{kip, types::ArmedWatch};
use serde_json::Value as Json;

pub(crate) const WATCH_SWEEP_LIMIT: usize = 20;
pub(crate) const CHANGES_PAGE_LIMIT: usize = 200;

pub(crate) fn watches_request(status: &str) -> anda_kip::Request {
    scan_request(status, false)
}

/// Prose/legacy Watches must not occupy the entire scheduling window forever.
pub(crate) fn runnable_watches_request() -> anda_kip::Request {
    scan_request("armed", true)
}

fn scan_request(status: &str, runnable: bool) -> anda_kip::Request {
    let filter = if runnable {
        r#"FILTER(IS_NOT_NULL(?w.facets["WatchState"].arm_generation))
FILTER(IS_NULL(?w.attributes.condition.text))
FILTER(IS_NOT_NULL(?w.attributes.condition.element) || IS_NOT_NULL(?w.attributes.condition.slot) || IS_NOT_NULL(?w.attributes.condition.type))"#
    } else {
        ""
    };
    kip::request_with(
        format!(
            r#"FIND(?w.id, ?w.name, ?w.attributes, ?w._system.version, ?w.facets["WatchState"], ?w.schema_ref)
WHERE {{ ?w CONCEPT {{type: "Watch"}} FILTER(?w.attributes.status == :status) {filter} }}
ORDER BY ?w.updated_at LIMIT {WATCH_SWEEP_LIMIT}"#
        ),
        kip::param("status", status),
    )
}

pub(crate) struct WatchRow {
    pub id: String,
    pub version: u64,
    pub generation: Option<u64>,
    pub condition: Json,
    pub watch: ArmedWatch,
}

/// A text member is never silently ignored, even alongside a selector.
pub(crate) fn is_structured(condition: &Json) -> bool {
    condition.is_object()
        && condition.get("text").is_none()
        && ["element", "slot", "type"]
            .iter()
            .any(|key| condition.get(*key).is_some())
}

pub(crate) fn read_watch_rows(result: &Json) -> Vec<WatchRow> {
    result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let columns = row.as_array()?;
            let id = columns.first()?.as_str()?.to_string();
            let version = columns.get(3)?.as_u64()?;
            let attributes = columns.get(2)?.as_object()?;
            let attribute = |key: &str| {
                attributes
                    .get(key)
                    .map(crate::types::attribute_text)
                    .unwrap_or_default()
            };
            Some(WatchRow {
                version,
                generation: columns.get(4).and_then(|s| s["arm_generation"].as_u64()),
                condition: attributes.get("condition").cloned().unwrap_or(Json::Null),
                watch: ArmedWatch {
                    id: id.clone(),
                    schema_ref: columns
                        .get(5)
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .into(),
                    version: Some(version),
                    name: columns
                        .get(1)
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .into(),
                    watch_class: attribute("watch_class"),
                    condition: attribute("condition"),
                    summary: attribute("summary"),
                    due_at: attribute("due_at"),
                },
                id,
            })
        })
        .collect()
}

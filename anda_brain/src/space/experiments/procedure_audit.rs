//! Evaluator-only, bounded native procedure inventory. This read is never
//! placed in a business prompt, and cannot authorize use or change standing.
use super::*;
use anda_cognitive_nexus::{content_digest, nexus::DEFAULT_SPACE};
use serde_json::{Value, json};

const PROFILE: &str = "kip://profiles/cognitive-memory@2.1.0/";
const LIMIT: usize = 256;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcedureAuditSkill {
    pub skill_ref: String,
    pub revision_ref: Option<String>,
    pub status: String,
    pub evaluation_ref: Option<String>,
    pub trial_ref: Option<String>,
    /// Inventory alone cannot establish read-time applicability.
    pub recommendation_allowed: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcedureAudit {
    pub format: String,
    /// Complete enumeration of the named native projections at this sequence,
    /// not semantic completeness or proof of events between audit boundaries.
    pub complete: bool,
    pub native_sequence: u64,
    /// Binds procedure records only; excludes time, Conversations, MnemonicState
    /// and other unrelated writes so session/maintenance probes can compare it.
    pub state_digest: String,
    /// Lower bounds whenever `complete` is false.
    pub counts: BTreeMap<String, usize>,
    pub skills: Vec<ProcedureAuditSkill>,
}

impl Experiment {
    pub async fn audit_procedures(&self) -> Result<ProcedureAudit, BoxError> {
        let guard = self.lock_run().await?;
        let run = guard.as_ref().ok_or("experiment is closed")?;
        quiescent(run).await?;
        // Only reads are timed out; no storage mutation is cancelled here.
        timeout(Duration::from_secs(15), audit(&run.space, LIMIT, MAX_BYTES)).await?
    }
}

fn reference(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value["id"].as_str())
        .map(str::to_owned)
}

async fn audit(space: &Space, limit: usize, max_bytes: usize) -> Result<ProcedureAudit, BoxError> {
    let nexus = space.memory.nexus();
    let seq = nexus.store.get_space(DEFAULT_SPACE).await?.seq;
    let mut projections = BTreeMap::new();
    let mut complete = true;
    let mut bytes = 0;
    for (key, kind, predicate, facet) in [
        ("skills", "CONCEPT", r#"type:"Skill""#, None),
        ("revisions", "CONCEPT", r#"type:"SkillRevision""#, None),
        ("decisions", "ACTIVITY", "", Some("DecisionRecord")),
        ("attempts", "ACTIVITY", "", Some("AttemptRecord")),
        ("outcomes", "EVIDENCE", "", Some("OutcomeRecord")),
        ("trials", "ACTIVITY", "", Some("TrialRecord")),
        ("evaluations", "ACTIVITY", "", Some("EvaluationRecord")),
    ] {
        let filter = facet.map_or_else(String::new, |f| {
            format!(r#" FILTER(IS_NOT_NULL(?r.facets["{PROFILE}{f}"]))"#)
        });
        let mut req = Request::single(format!(
            "FIND(?r) WHERE {{?r {kind} {{{predicate}}}{filter}}} ORDER BY ?r.id LIMIT {}",
            limit + 1
        ));
        space.clock.bind_read(&mut req)?;
        let response = execute_request(nexus.as_ref(), &req).await;
        if !kip::succeeded(&response) {
            return Err("native procedure audit read failed".into());
        }
        let rows = kip::ok_result(&response)
            .and_then(Value::as_array)
            .ok_or("native procedure audit expected rows")?;
        complete &= rows.len() <= limit;
        let mut selected = vec![];
        for row in rows.iter().take(limit) {
            let projected = if let Some(facet) = facet {
                json!({"id":row["id"], "lifecycle":row["lifecycle"],
                    "record":row["facets"][format!("{PROFILE}{facet}")]})
            } else {
                json!({"id":row["id"],"name":row["name"],"lifecycle":row["lifecycle"],
                    "attributes":row["attributes"],"structural":row["structural"],
                    "trial":row["facets"][format!("{PROFILE}TrialState")],
                    "grade":row["facets"][format!("{PROFILE}GradingState")]})
            };
            let size = serde_json::to_vec(&projected)?.len();
            if size > max_bytes.saturating_sub(bytes) {
                complete = false;
                break;
            }
            bytes += size;
            selected.push(projected);
        }
        projections.insert(key.to_string(), selected);
    }
    if seq != nexus.store.get_space(DEFAULT_SPACE).await?.seq {
        return Err(
            "native procedure audit changed during enumeration; retry at a quiescent boundary"
                .into(),
        );
    }
    let skills = projections["skills"]
        .iter()
        .map(|row| ProcedureAuditSkill {
            skill_ref: row["id"].as_str().unwrap_or_default().into(),
            revision_ref: row["structural"][format!("{PROFILE}current_revision")]
                .as_array()
                .filter(|v| v.len() == 1)
                .and_then(|v| reference(&v[0])),
            status: row["attributes"]["status"]
                .as_str()
                .unwrap_or("unknown")
                .into(),
            evaluation_ref: reference(&row["grade"]["evaluation_ref"]),
            trial_ref: reference(&row["trial"]["trial_ref"]),
            recommendation_allowed: None,
        })
        .collect();
    Ok(ProcedureAudit {
        format: "mib-learning-audit/0.1".into(),
        complete,
        native_sequence: seq,
        state_digest: content_digest(&json!(projections))?,
        counts: projections
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect(),
        skills,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn p6_audit_overflow_never_claims_complete_absence() {
        let app = super::super::tests::app();
        let run = Experiment::create(
            &app,
            MemoryMode::Persistent,
            super::super::tests::identity(),
            super::super::tests::NOW,
        )
        .await
        .unwrap();
        for _ in 0..3 {
            let behavior = json!({"task_family":"tool_workflow.precondition.v1","procedure":"always prepare then commit"});
            super::super::tests::command(&run, r#"MUTATE {
                CREATE CONCEPT ?s {TYPE "Skill" SET ATTRIBUTES {skill_class:"workflow",summary:"unproven",status:"proposed"} SET STRUCTURAL {("current_revision",?r)}}
                CREATE CONCEPT ?r {TYPE "SkillRevision" SET ATTRIBUTES {task_family:"tool_workflow.precondition.v1",procedure:"always prepare then commit",behavior_digest:"$BEHAVIOR"} SET STRUCTURAL {("revision_of",?s)}}
            }"#.replace("$BEHAVIOR", &content_digest(&behavior).unwrap())).await;
        }
        let full = run.audit_procedures().await.unwrap();
        assert!(full.complete);
        assert_eq!(full.counts["skills"], 3);
        assert!(full.skills.iter().all(|s| s.revision_ref.is_some()));
        {
            let guard = run.run.lock().await;
            let space = &guard.as_ref().unwrap().space;
            let limited = audit(space, 2, MAX_BYTES).await.unwrap();
            assert!(!limited.complete);
            assert_eq!(limited.counts["skills"], 2);
            let too_large = audit(space, LIMIT, 1).await.unwrap();
            assert!(!too_large.complete);
            assert_eq!(too_large.counts["skills"], 0, "zero is only a lower bound");
        }
        run.close().await.unwrap();
    }
}

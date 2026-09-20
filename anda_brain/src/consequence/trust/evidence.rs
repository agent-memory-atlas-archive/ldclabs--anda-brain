use super::runtime::*;
use super::*;
use crate::runtime_api::full_read;
use anda_cognitive_nexus::{
    governance::{AuthContext, Permission},
    nexus::{DEFAULT_SPACE, Session},
    store::{eq_fields, rows::EvidenceRow},
};
use anda_db::schema::Fv;
use std::sync::Arc;

pub(super) struct Sample {
    pub reference: String,
    pub actor: String,
    pub unit: String,
    pub claim_group: String,
    pub sequence: u64,
    pub correct: Option<bool>,
    pub sources: Vec<String>,
    pub reason: Option<String>,
}
struct Claim {
    actor: String,
    proposition: String,
    digest: String,
    group: String,
    author: String,
    sources: Vec<String>,
}
fn reference(v: &Json) -> Option<&str> {
    v.as_str().or_else(|| v["id"].as_str())
}

impl TrustRuntime {
    pub(super) async fn author(&self, row: &Json) -> Result<(String, u64), BoxError> {
        let tx = self
            .nexus
            .store
            .find_transaction(
                row["_system"]["created_tx"]
                    .as_str()
                    .ok_or("native creation transaction missing")?,
            )
            .await?
            .ok_or("native transaction unavailable")?;
        if tx.space != DEFAULT_SPACE
            || tx.status != "committed"
            || !tx
                .changed_ids
                .iter()
                .any(|v| Some(v.as_str()) == row["id"].as_str())
        {
            return Err("native provenance mismatch".into());
        }
        Ok((
            tx.origin["principal_id"]
                .as_str()
                .ok_or("native author missing")?
                .into(),
            tx.seq,
        ))
    }
    async fn claim(
        &self,
        session: &Session,
        assertion: &str,
        assessed_at: &str,
    ) -> Result<Claim, BoxError> {
        anda_cognitive_nexus::ElementId::parse_kind(assertion, anda_kip::ElementKind::Assertion)?;
        let c = self.cfg()?;
        let row = full_read(session, assertion).await?;
        if !matches!(row["stance"].as_str(), Some("support" | "reject"))
            || matches!(row["mode"].as_str(), Some("hypothetical" | "predicted"))
        {
            return Err(
                "binary fact calibration requires an actual non-predictive source commitment"
                    .into(),
            );
        }
        let actor = reference(&row["asserted_by"])
            .ok_or("source needs an exact semantic actor Concept")?
            .to_string();
        concept(&actor)?;
        let proposition = reference(&row["proposition"])
            .ok_or("source Proposition missing")?
            .to_string();
        let p = full_read(session, &proposition).await?;
        let contexts = row["context_refs"]
            .as_array()
            .ok_or("source context missing")?;
        if p["predicate_ref"] != c.predicate_ref
            || contexts.len() != 1
            || reference(&contexts[0]) != Some(c.context_ref.as_str())
        {
            return Err(ineligible(
                "source fact is outside the exact registered predicate/context",
            ));
        }
        let from = row["valid_time"]["from"]
            .as_str()
            .or_else(|| row["asserted_at"].as_str())
            .or_else(|| row["_system"]["created_at"].as_str())
            .ok_or("source temporal basis missing")?;
        if assessed_at < from
            || row["valid_time"]["until"]
                .as_str()
                .is_some_and(|end| assessed_at >= end)
        {
            return Err("fact verification is outside the source's applicability interval".into());
        }
        full_read(session, &actor).await?;
        full_read(session, &c.context_ref).await?;
        let (author, _) = self.author(&row).await?;
        if author == c.observer.principal_id {
            return Err("source cannot independently verify its own assertions".into());
        }
        // Immutable epistemic payload only: later supersession is history, not
        // a rewrite of whether this source once made this particular commitment.
        let digest = content_digest(
            &json!({"assertion":assertion,"proposition":p["id"],"subject":p["subject"],"predicate":p["predicate_ref"],"object":p["object"],
            "actor":row["asserted_by"],"stance":row["stance"],"mode":row["mode"],"confidence":row["confidence"],"asserted_at":row["asserted_at"],"valid_time":row["valid_time"],"contexts":row["context_refs"],"evidence":row["evidence"]}),
        )?;
        let group = content_digest(
            &json!({"actor":actor,"proposition":proposition,"stance":row["stance"],"valid_time":row["valid_time"],"context":row["context_refs"]}),
        )?;
        Ok(Claim {
            group,
            actor: actor.clone(),
            proposition: proposition.clone(),
            digest,
            author,
            sources: vec![assertion.into(), proposition, actor, c.context_ref.clone()],
        })
    }
    /// Separate factual verification intake for trusted, authenticated hosts.
    /// No API/model route synthesizes this identity from a semantic actor or ST.
    pub async fn record_verification(
        self: &Arc<Self>,
        auth: AuthContext,
        mut input: TrustVerificationInput,
    ) -> Result<String, BoxError> {
        let this = self.clone();
        self.tasks.run(async move {
            let _g=this.gate.lock().await;let cfg=this.cfg()?;
            this.permission(&auth,Permission::RecordOutcome).await?;
            if auth.principal_id!=cfg.observer.principal_id {return Err("unregistered fact verifier".into());}
            if [&input.event_key,&input.root_key].iter().any(|s|s.trim().is_empty()||s.len()>256)
                || !input.material.is_object() || input.material.as_object().is_none_or(|m|m.is_empty())
                || serde_json::to_vec(&input)?.len()>16_384
                || (input.cause==VerificationCause::VerifiedFact && input.correct.is_none()) {return Err("invalid bounded factual verification".into());}
            input.assessed_at=anda_cognitive_nexus::time::normalize(&input.assessed_at,"assessed_at")?;
            if input.assessed_at>anda_cognitive_nexus::time::now() {return Err("fact assessment cannot be future dated".into());}
            let session=this.nexus.session(auth.clone());
            let claim=this.claim(&session,&input.assertion_ref,&input.assessed_at).await?;
            let record=VerificationRecord {format:VERIFICATION.into(),scope:this.scope.clone(),contract_digest:cfg.contract_digest()?,observer:cfg.observer.clone(),input,
                actor_ref:claim.actor.clone(),proposition_ref:claim.proposition,claim_digest:claim.digest,source_principal:claim.author};
            let digest=content_digest(&json!(record))?;
            let artifact=ArtifactPin {artifact_ref:format!("kip:artifact:{digest}"),content_digest:digest.clone()};
            let client_key=format!("brain:trust-verification:{}",&content_digest(&json!({"scope":this.scope,"observer":auth.principal_id,"event":record.input.event_key}))?[7..]);
            // Durable discovery precedes the native write. Only hashes/refs are
            // retained here; no source material or reconstructed credentials.
            this.admit(&claim.actor,Source{client_key:client_key.clone(),body_digest:digest,reference:None}).await?;
            let owner=this.space.read().as_ref().and_then(|s|s.upgrade());
            if let Some(owner)=owner {owner.attention().register_work().await?;}
            let command=format!("CREATE EVIDENCE ?verification {{CLIENT KEY {} SET FIELDS {{evidence_class:\"observation\",observed_at:{},payload:{}}}}}",
                crate::kip::string_literal(&client_key),crate::kip::string_literal(&record.input.assessed_at),
                crate::kip::string_literal(&json!({"format":VERIFICATION,"material":artifact}).to_string()));
            let mut request=crate::kip::request(command);request.operations[0].idempotency_key=Some(client_key.clone());
            let response=anda_kip::execute_request(&session,&request).await;
            let reference=crate::kip::ok_result(&response).and_then(|v|v["handles"]["verification"].as_str()).ok_or_else(||crate::kip::error_message(&response))?.to_string();
            let actual=full_read(&session,&reference).await?;
            let actual_payload:Json=serde_json::from_str(actual["payload"]["inline"].as_str().ok_or("verification pointer missing")?)?;
            if this.author(&actual).await?.0!=auth.principal_id || actual_payload["material"]!=json!(artifact) {
                return Err("verification native identity/payload mismatch".into());
            }
            let mut sources=claim.sources;sources.push(reference.clone());
            let stored=session.put_artifact(DEFAULT_SPACE,json!(record),sources).await?;
            if stored!=artifact {return Err("verification material mismatch".into());}
            this.admit(&claim.actor,Source{client_key,body_digest:artifact.content_digest,reference:Some(reference.clone())}).await?;
            Ok(reference)
        }).await
    }
    pub(super) async fn resolve_source(&self, source: &Source) -> Result<Option<String>, BoxError> {
        if let Some(id) = &source.reference {
            let row = full_read(&self.proposer()?, id).await?;
            let payload: Json = serde_json::from_str(
                row["payload"]["inline"]
                    .as_str()
                    .ok_or("verification pointer missing")?,
            )?;
            if payload["material"]["content_digest"] != source.body_digest {
                return Err("verification checkpoint digest mismatch".into());
            }
            return Ok(Some(id.clone()));
        }
        let ids = self
            .nexus
            .store
            .evidence()
            .query_all_ids(eq_fields(&[
                ("space", Fv::Text(DEFAULT_SPACE.into())),
                ("client_key", Fv::Text(source.client_key.clone())),
            ]))
            .await?;
        if ids.len() > 1 {
            return Err("ambiguous native verification identity".into());
        }
        if let Some(id) = ids.first() {
            let row: EvidenceRow = self.nexus.store.evidence().get_as(*id).await?;
            let payload: Json = serde_json::from_str(
                row.payload_inline
                    .as_str()
                    .ok_or("verification pointer missing")?,
            )?;
            if payload["material"]["content_digest"] != source.body_digest {
                return Err("verification checkpoint digest mismatch".into());
            }
            Ok(Some(format!("E-{id}")))
        } else {
            Ok(None)
        }
    }
    pub(super) async fn sample(&self, id: &str) -> Result<Sample, BoxError> {
        let cfg = self.cfg()?;
        let session = self.proposer()?;
        let row = full_read(&session, id).await?;
        if !id.starts_with("E-")
            || row["evidence_class"] != "observation"
            || row["lifecycle"]["status"] == "corrected"
        {
            return Err(ineligible("verification Evidence is ineligible"));
        }
        let (author, sequence) = self.author(&row).await?;
        if author != cfg.observer.principal_id {
            return Err(ineligible(
                "verification was not written by the registered independent principal",
            ));
        }
        let payload: Json = serde_json::from_str(
            row["payload"]["inline"]
                .as_str()
                .ok_or("verification material pointer missing")?,
        )?;
        if payload["format"] != VERIFICATION {
            return Err(ineligible("unsupported trust verification format"));
        }
        let pin: ArtifactPin = serde_json::from_value(payload["material"].clone())?;
        let record: VerificationRecord =
            serde_json::from_value(session.read_artifact(DEFAULT_SPACE, &pin).await?)?;
        if record.format != VERIFICATION
            || record.scope != self.scope
            || record.contract_digest != cfg.contract_digest()?
            || json!(record.observer) != json!(cfg.observer)
        {
            return Err(ineligible("fact verification scope/method mismatch"));
        }
        let claim = self
            .claim(
                &session,
                &record.input.assertion_ref,
                &record.input.assessed_at,
            )
            .await?;
        if claim.actor != record.actor_ref
            || claim.proposition != record.proposition_ref
            || claim.digest != record.claim_digest
            || claim.author != record.source_principal
        {
            return Err("verification source commitment changed".into());
        }
        let unit = content_digest(
            &json!({"observer":cfg.observer,"environment":cfg.environment_digest,"root":record.input.root_key}),
        )?;
        let reason = (record.input.cause != VerificationCause::VerifiedFact
            || record.input.correct.is_none())
        .then(|| format!("not_a_verified_source_fact:{:?}", record.input.cause));
        let mut sources = claim.sources;
        sources.push(id.into());
        Ok(Sample {
            reference: id.into(),
            actor: claim.actor,
            unit,
            claim_group: claim.group,
            sequence,
            correct: record.input.correct,
            sources,
            reason,
        })
    }
    pub async fn enqueue(self: &Arc<Self>, reference: String) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.enqueue_inner(&reference).await
            })
            .await
    }
    pub(super) async fn enqueue_inner(&self, reference: &str) -> Result<(), BoxError> {
        let sample = self.sample(reference).await?;
        let row: EvidenceRow = self
            .nexus
            .store
            .evidence()
            .get_as(reference[2..].parse()?)
            .await?;
        let value: Json = serde_json::from_str(
            row.payload_inline
                .as_str()
                .ok_or("verification pointer missing")?,
        )?;
        self.admit(
            &sample.actor,
            Source {
                client_key: if row.client_key.is_empty() {
                    format!("native:{reference}")
                } else {
                    row.client_key
                },
                body_digest: value["material"]["content_digest"]
                    .as_str()
                    .ok_or("verification digest missing")?
                    .into(),
                reference: Some(reference.into()),
            },
        )
        .await
    }
}

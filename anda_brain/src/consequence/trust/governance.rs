use super::runtime::*;
use super::*;
use anda_cognitive_nexus::{
    governance::{AuthContext, Permission},
    nexus::DEFAULT_SPACE,
    store::rows::ControlRecordRow,
    trust::TrustCalibrationProposal,
};
use std::sync::Arc;

impl TrustRuntime {
    async fn governor(&self, auth: &AuthContext) -> Result<(), BoxError> {
        let cfg = self.cfg()?;
        if !cfg.apply
            || !cfg.calibrated()
            || cfg.governor_principal.as_deref() != Some(auth.principal_id.as_str())
        {
            return Err("trust application is not enabled for this calibrated governor".into());
        }
        self.permission(auth, Permission::ManageTrust).await
    }
    async fn control_version(&self, version: u64) -> Result<Option<ControlRecordRow>, BoxError> {
        let session = self.proposer()?;
        let current = session
            .read_control(DEFAULT_SPACE, "trust", None)
            .await?
            .ok_or("native trust configuration unavailable")?;
        if current.version < version {
            return Ok(None);
        }
        if current.version == version {
            return Ok(Some(current));
        }
        // Public historical coordinates, bounded by the immutable upper snapshot.
        // No dependence on private idempotency encodings or unindexed fields.
        let (mut lo, mut hi) = (0, current.seq);
        for _ in 0..64 {
            if lo > hi {
                break;
            }
            let mid = lo + (hi - lo) / 2;
            match session
                .read_control(DEFAULT_SPACE, "trust", Some(mid))
                .await?
            {
                Some(row) if row.version == version => return Ok(Some(row)),
                Some(row) if row.version > version => {
                    if mid == 0 {
                        break;
                    }
                    hi = mid - 1;
                }
                _ => lo = mid.saturating_add(1),
            }
        }
        Err("trust version history is unavailable; commit cannot be assumed absent".into())
    }
    pub(super) async fn recovered(
        &self,
        job: &ProposalJob,
        p: &TrustProposal,
    ) -> Result<Option<TrustReceipt>, BoxError> {
        let (Some(reviewed), Some(governor)) = (&job.reviewed, &job.governor) else {
            return Ok(None);
        };
        let Some(row) = self.control_version(p.expected_version + 1).await? else {
            return Ok(None);
        };
        if row.value["calibration"]["proposal"] != json!(reviewed) {
            return Ok(None);
        }
        if row.origin["principal_id"] != *governor {
            return Err("trust receipt author mismatch".into());
        }
        Ok(Some(TrustReceipt {
            proposal_id: job.id.clone(),
            reviewed_proposal: reviewed.clone(),
            governor: governor.clone(),
            version: row.version,
            space_seq: row.seq,
            control_ref: row.record_id,
        }))
    }
    pub async fn apply(
        self: &Arc<Self>,
        auth: AuthContext,
        proposal_id: String,
        reason: String,
    ) -> Result<TrustReceipt, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.apply_inner(auth, &proposal_id, &reason).await
            })
            .await
    }
    pub(super) async fn apply_inner(
        &self,
        auth: AuthContext,
        id: &str,
        reason: &str,
    ) -> Result<TrustReceipt, BoxError> {
        self.governor(&auth).await?;
        if reason.trim().is_empty() || reason.len() > 4096 {
            return Err("bounded trust review reason required".into());
        }
        let mut job = self.job(id).await?;
        let proposal = self.proposal(id).await?;
        if proposal.predicate_ref != self.cfg()?.predicate_ref
            || proposal.context_ref != self.cfg()?.context_ref
            || (proposal.restores.is_none()
                && proposal.contract_digest != self.cfg()?.contract_digest()?)
        {
            return Err("trust proposal contract/domain changed; explicit review required".into());
        }
        self.nexus.recover().await?;
        if let Some(receipt) = self.recovered(&job, &proposal).await? {
            return self.finish(&mut job, &proposal, receipt).await;
        }
        if job.stale {
            return Err("stale trust proposal requires a fresh explicit proposal/review".into());
        }
        let native_pin = proposal
            .native_proposal
            .as_ref()
            .ok_or("diagnostic trust proposal cannot be applied")?;
        let proposer = self.proposer()?;
        let mut native: TrustCalibrationProposal =
            serde_json::from_value(proposer.read_artifact(DEFAULT_SPACE, native_pin).await?)?;
        let (version, current) = self.trust_control().await?;
        if version != proposal.expected_version {
            self.stale(&mut job).await?;
            return Err("trust version conflict; another setting is preserved".into());
        }
        // Reconstruct the sole permitted change. A frozen proposal cannot edit
        // global weights, defaults, or a neighboring actor/predicate/context.
        let mut permitted = current.clone();
        permitted.rules.retain(|r| {
            !(r.actor_ref == proposal.actor_ref
                && r.predicate_ref.as_ref() == Some(&proposal.predicate_ref)
                && r.context_ref.as_ref() == Some(&proposal.context_ref))
        });
        if let Some(rule) = &proposal.proposed_rule {
            permitted.rules.push(rule.clone());
        }
        let mut actual = native.configuration.clone();
        permitted.rules.sort_by(|a, b| a.id.cmp(&b.id));
        actual.rules.sort_by(|a, b| a.id.cmp(&b.id));
        if actual != permitted || native.expected_version != version {
            return Err("trust proposal changes more than its exact registered scope".into());
        }
        // Every root is immutable Evidence. The engine checks correction/state
        // again under its governance lock when it commits the control and audit.
        if proposal.restores.is_none() {
            for reference in &proposal.evidence_refs {
                let sample = self.sample(reference).await?;
                if sample.actor != proposal.actor_ref
                    || sample.reason.is_some()
                    || !proposal.roots.contains(&sample.unit)
                    || !proposal.roots.contains(&sample.claim_group)
                {
                    return Err("trust evidence no longer qualifies".into());
                }
            }
        }
        if job.reviewed.is_none() {
            native.method=proposer.put_artifact(DEFAULT_SPACE,json!({"format":FORMAT,"method":proposal.method,"calibration":self.cfg()?.calibration,
                "proposal_id":id,"review":{"principal":auth.principal_id,"reason":reason},"restores":proposal.restores}),job.sources.clone()).await?;
            native.uncertainty["review"] =
                json!({"principal":auth.principal_id,"reason":reason,"proposal_id":id});
            job.reviewed = Some(
                proposer
                    .put_artifact(DEFAULT_SPACE, json!(native), job.sources.clone())
                    .await?,
            );
            job.governor = Some(auth.principal_id.clone());
            self.save_job(&job).await?;
        }
        if job.governor.as_deref() != Some(auth.principal_id.as_str()) {
            return Err("pending trust review belongs to another governor".into());
        }
        for root in &job.roots {
            let key = self.key("units", root)?;
            if let Some(old) = self.journal.read::<RootClaim>(&key).await? {
                if old.value.abandoned {
                    self.save(
                        &key,
                        &RootClaim {
                            proposal: job.id.clone(),
                            applied: false,
                            abandoned: false,
                        },
                    )
                    .await?;
                } else if old.value.proposal != job.id {
                    return Err("independent trust root already reserved or consumed".into());
                }
            } else {
                self.journal
                    .create(
                        &key,
                        &RootClaim {
                            proposal: job.id.clone(),
                            applied: false,
                            abandoned: false,
                        },
                    )
                    .await?;
            }
        }
        let result = self
            .nexus
            .session(auth)
            .apply_trust_calibration(
                DEFAULT_SPACE,
                proposal.expected_version,
                job.reviewed.clone().unwrap(),
                &job.id,
            )
            .await;
        self.nexus.recover().await?;
        if let Some(receipt) = self.recovered(&job, &proposal).await? {
            return self.finish(&mut job, &proposal, receipt).await;
        }
        if self
            .control_version(proposal.expected_version + 1)
            .await?
            .is_some()
        {
            self.stale(&mut job).await?;
        }
        Err(result
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "trust commit acknowledgement unresolved".into())
            .into())
    }
    async fn stale(&self, job: &mut ProposalJob) -> Result<(), BoxError> {
        job.stale = true;
        self.save_job(job).await?;
        for root in &job.roots {
            let key = self.key("units", root)?;
            if let Some(mut claim) = self.journal.read::<RootClaim>(&key).await?.map(|r| r.value)
                && claim.proposal == job.id
                && !claim.applied
            {
                claim.abandoned = true;
                self.save(&key, &claim).await?;
            }
        }
        Ok(())
    }
    /// Explicitly abandon a provably uncommitted review. Never erase an applied
    /// calibration; that requires a new restoration proposal and native version.
    pub async fn abandon(
        self: &Arc<Self>,
        auth: AuthContext,
        id: String,
        reason: String,
    ) -> Result<(), BoxError> {
        let this = self.clone();
        self.tasks.run(async move {
            let _g=this.gate.lock().await;this.governor(&auth).await?;
            if reason.trim().is_empty()||reason.len()>4096{return Err("bounded abandonment reason required".into());}
            this.nexus.recover().await?;
            let mut job=this.job(&id).await?;let proposal=this.proposal(&id).await?;
            if this.recovered(&job,&proposal).await?.is_some(){return Err("applied trust must be restored through a new governance version".into());}
            let proof=this.proposer()?.put_artifact(DEFAULT_SPACE,json!({"format":FORMAT,"abandoned_proposal":job.report,"principal":auth.principal_id,"reason":reason}),job.sources.clone()).await?;
            this.save(&this.key("abandonment",&id)?,&proof).await?;
            this.stale(&mut job).await
        }).await
    }
    async fn finish(
        &self,
        job: &mut ProposalJob,
        p: &TrustProposal,
        receipt: TrustReceipt,
    ) -> Result<TrustReceipt, BoxError> {
        for root in &job.roots {
            self.save(
                &self.key("units", root)?,
                &RootClaim {
                    proposal: job.id.clone(),
                    applied: true,
                    abandoned: false,
                },
            )
            .await?;
        }
        job.receipt = Some(receipt.clone());
        self.save_job(job).await?;
        if p.restores.is_none() {
            let key = self.target_key(&p.actor_ref)?;
            if let Some(mut target) = self.journal.read::<Target>(&key).await?.map(|v| v.value) {
                target.sources.retain(|_, s| {
                    s.reference.as_ref().is_none_or(|r| {
                        !p.evidence_refs.contains(r)
                            && p.excluded
                                .get(r)
                                .is_none_or(|reason| reason == "outside_fixed_first_n_group")
                    })
                });
                self.save(&key, &target).await?;
            }
        }
        if !job.notified {
            let space = self.space.read().as_ref().and_then(|s| s.upgrade());
            if let Some(space) = space {
                space.trust_changed().await?;
            }
            job.notified = true;
            self.save_job(job).await?;
        }
        Ok(receipt)
    }
    /// Prepare (do not silently apply) an exact scoped restoration. This is a
    /// governance correction, not a fresh statistical sample or an erase.
    pub async fn propose_restore(
        self: &Arc<Self>,
        auth: AuthContext,
        previous_id: String,
        reason: String,
        evidence_refs: Vec<String>,
    ) -> Result<TrustProposal, BoxError> {
        let this = self.clone();
        self.tasks.run(async move {
            let _g=this.gate.lock().await;this.governor(&auth).await?;
            if reason.trim().is_empty()||reason.len()>4096||evidence_refs.is_empty()||evidence_refs.len()>32 {return Err("restoration requires a reason and bounded current Evidence".into());}
            let old=this.proposal(&previous_id).await?;let old_job=this.job(&previous_id).await?;
            if this.recovered(&old_job,&old).await?.is_none() {return Err("only an actually applied trust proposal may be restored".into());}
            if old.context_ref!=this.cfg()?.context_ref||old.predicate_ref!=this.cfg()?.predicate_ref {return Err("restoration is outside the registered domain".into());}
            let (version,mut current)=this.trust_control().await?;
            let current_rule=current.rules.iter().find(|r|r.actor_ref==old.actor_ref&&r.predicate_ref.as_ref()==Some(&old.predicate_ref)&&r.context_ref.as_ref()==Some(&old.context_ref)).cloned();
            if current_rule!=old.proposed_rule {return Err("scoped trust changed since the applied proposal; restoration must not overwrite it".into());}
            let mut sources=std::collections::BTreeSet::from([old.actor_ref.clone(),old.context_ref.clone()]);
            for id in &evidence_refs {
                anda_cognitive_nexus::ElementId::parse_kind(id,anda_kip::ElementKind::Evidence)?;
                let view=crate::runtime_api::full_read(&this.proposer()?,id).await?;
                if view["lifecycle"]["status"]=="corrected" {return Err("restoration evidence has been corrected".into());}
                sources.insert(id.clone());
            }
            if sources.len()!=evidence_refs.len()+2 {return Err("duplicate restoration evidence".into());}
            let old_weight=current.weight(&old.actor_ref,&old.predicate_ref,std::slice::from_ref(&old.context_ref))?;
            current.rules.retain(|r|!(r.actor_ref==old.actor_ref&&r.predicate_ref.as_ref()==Some(&old.predicate_ref)&&r.context_ref.as_ref()==Some(&old.context_ref)));
            if let Some(rule)=&old.previous_rule {current.rules.push(rule.clone());}
            let restored=current.weight(&old.actor_ref,&old.predicate_ref,std::slice::from_ref(&old.context_ref))?;
            let id=content_digest(&json!({"restore":previous_id,"version":version,"evidence":evidence_refs,"reason":reason,"governor":auth.principal_id}))?;
            let sources:Vec<_>=sources.into_iter().collect();
            let method=this.proposer()?.put_artifact(DEFAULT_SPACE,json!({"format":FORMAT,"method":"explicit-scoped-restoration-v1","previous_proposal":old.native_proposal,"reason":reason,"reviewer":auth.principal_id,"evidence_refs":evidence_refs,"expected_version":version}),sources.clone()).await?;
            let uncertainty=json!({"kind":"governance_restoration","reason":reason,"restores":previous_id,"new_independent_samples":0});
            let native=TrustCalibrationProposal {format:"nexus:trust-calibration-v1".into(),space_id:DEFAULT_SPACE.into(),expected_version:version,configuration:current,method:method.clone(),evidence_refs:evidence_refs.clone(),uncertainty:uncertainty.clone()};
            let native=this.proposer()?.put_artifact(DEFAULT_SPACE,json!(native),sources.clone()).await?;
            this.publish(TrustProposal {format:FORMAT.into(),id,scope:this.scope.clone(),contract_digest:this.cfg()?.contract_digest()?,actor_ref:old.actor_ref,predicate_ref:old.predicate_ref,context_ref:old.context_ref,
                expected_version:version,previous_rule:current_rule,proposed_rule:old.previous_rule,old_weight,new_weight:Some(restored),independent_samples:0,evidence_refs,roots:vec![],excluded:Default::default(),uncertainty,method,native_proposal:Some(native),reason:None,restores:Some(previous_id)},sources).await
        }).await
    }
}

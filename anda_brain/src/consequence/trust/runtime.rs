use super::*;
use anda_cognitive_nexus::{
    CognitiveNexus,
    governance::{AuthContext, Permission},
    nexus::{DEFAULT_SPACE, Session},
};
use futures::FutureExt;
use object_store::PutMode;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

pub(super) const MAX_TARGETS: usize = 4096;
pub(super) const MAX_PENDING: usize = 64;
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Catalog {
    pub targets: Vec<String>,
    pub cursor: usize,
    pub source_cursor: u64,
    pub pending: Option<Target>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Source {
    pub client_key: String,
    pub body_digest: String,
    pub reference: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Target {
    pub id: String,
    pub actor: String,
    pub contract: String,
    pub sources: BTreeMap<String, Source>,
    pub latest: Option<String>,
    #[serde(default)]
    pub epoch: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ProposalJob {
    pub id: String,
    pub actor: String,
    pub report: ArtifactPin,
    pub sources: Vec<String>,
    pub roots: Vec<String>,
    pub reviewed: Option<ArtifactPin>,
    pub governor: Option<String>,
    pub receipt: Option<TrustReceipt>,
    pub stale: bool,
    pub notified: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct RootClaim {
    pub proposal: String,
    pub applied: bool,
    #[serde(default)]
    pub abandoned: bool,
}

pub struct TrustRuntime {
    pub(super) nexus: Arc<CognitiveNexus>,
    pub(super) journal: Arc<crate::journal::Journal>,
    pub(super) scope: RuntimeScope,
    pub(super) config: Option<TrustConfig>,
    automatic: bool,
    pub(super) gate: Mutex<()>,
    pub(super) tasks: crate::runtime::DurableTasks,
    running: AtomicBool,
    last_error: parking_lot::RwLock<Option<String>>,
    pub(super) space: parking_lot::RwLock<Option<Weak<crate::space::Space>>>,
}
impl TrustRuntime {
    pub(crate) fn new(
        nexus: Arc<CognitiveNexus>,
        store: Arc<dyn object_store::ObjectStore>,
        receipts: Arc<crate::recall_receipt::RecallReceipts>,
        config: Option<TrustConfig>,
        automatic: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            nexus,
            journal: Arc::new(crate::journal::Journal::new(
                store,
                format!(
                    "{}/trust/{}",
                    receipts.scope.space_id,
                    &receipts.scope.space_instance[7..]
                ),
            )),
            scope: receipts.scope.clone(),
            config,
            automatic,
            gate: Mutex::new(()),
            tasks: Default::default(),
            running: AtomicBool::new(false),
            last_error: Default::default(),
            space: Default::default(),
        })
    }
    pub(crate) fn bind_space(&self, space: Weak<crate::space::Space>) {
        *self.space.write() = Some(space);
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.tasks.is_busy()
    }
    pub(crate) async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }
    pub(super) fn cfg(&self) -> Result<&TrustConfig, BoxError> {
        let c = self.config.as_ref().ok_or("trust is not configured")?;
        c.validate()?;
        Ok(c)
    }
    pub(super) fn host_auth(principal: &str) -> AuthContext {
        let mut auth = AuthContext::principal(principal);
        auth.auth_method = "brain:registered-trust-host".into();
        auth
    }
    pub(super) fn proposer(&self) -> Result<Session, BoxError> {
        Ok(self
            .nexus
            .session(Self::host_auth(&self.cfg()?.proposer_principal)))
    }
    pub(super) async fn permission(
        &self,
        auth: &AuthContext,
        permission: Permission,
    ) -> Result<(), BoxError> {
        if !crate::runtime_api::principal_valid(&auth.principal_id)
            || auth.auth_strength == "none"
            || auth.auth_method.is_empty()
            || !auth.delegation_chain.is_empty()
        {
            return Err("trust requires directly authenticated host identity".into());
        }
        self.nexus
            .session(auth.clone())
            .effective_authority(DEFAULT_SPACE)
            .await?
            .authorize(permission, &Default::default(), auth)
            .into_result()?;
        Ok(())
    }
    pub(super) fn key(&self, lane: &str, id: &str) -> Result<String, BoxError> {
        Ok(format!(
            "{lane}/{}",
            &content_digest(&json!({"scope":self.scope,"id":id}))?[7..]
        ))
    }
    pub(super) fn target_key(&self, actor: &str) -> Result<String, BoxError> {
        concept(actor)?;
        self.key(
            "targets",
            &content_digest(&json!({"actor":actor,"contract":self.cfg()?.contract_digest()?}))?,
        )
    }
    pub(super) async fn save<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), BoxError> {
        let old = self.journal.read::<T>(key).await?;
        self.journal
            .put(
                key,
                value,
                old.map_or(PutMode::Create, |v| PutMode::Update(v.version)),
            )
            .await
    }
    pub(super) async fn catalog(&self) -> Result<Catalog, BoxError> {
        let mut c = self
            .journal
            .read::<Catalog>("catalog")
            .await?
            .map(|r| r.value)
            .unwrap_or_default();
        if c.targets.len() > MAX_TARGETS || c.cursor > MAX_TARGETS {
            return Err("trust catalog capacity exceeded".into());
        }
        if let Some(t) = c.pending.take() {
            if self.journal.read::<Target>(&t.id).await?.is_none() {
                self.journal.create(&t.id, &t).await?;
            }
            if !c.targets.contains(&t.id) {
                c.targets.push(t.id);
            }
            self.save("catalog", &c).await?;
        }
        Ok(c)
    }
    pub(super) async fn admit(&self, actor: &str, source: Source) -> Result<(), BoxError> {
        let id = self.target_key(actor)?;
        let old = self.journal.read::<Target>(&id).await?;
        let mut target = old.as_ref().map(|v| v.value.clone()).unwrap_or(Target {
            id: id.clone(),
            actor: actor.into(),
            contract: self.cfg()?.contract_digest()?,
            sources: Default::default(),
            latest: None,
            epoch: 0,
        });
        let key = source.client_key.clone();
        if let Some(old) = target.sources.get(&key)
            && old.body_digest != source.body_digest
        {
            return Err("trust verification event key conflicts with retained evidence".into());
        }
        if !target.sources.contains_key(&key) && target.sources.len() >= MAX_PENDING {
            return Err("trust source queue is full".into());
        }
        target.sources.insert(key, source);
        if old.is_some() {
            return self.save(&id, &target).await;
        }
        let mut catalog = self.catalog().await?;
        if catalog.targets.len() >= MAX_TARGETS {
            return Err("trust target capacity reached".into());
        }
        catalog.pending = Some(target);
        self.save("catalog", &catalog).await?;
        self.catalog().await?;
        Ok(())
    }
    pub async fn status(&self) -> TrustStatus {
        let Some(c) = &self.config else {
            return TrustStatus {
                reason: Some("trust_not_configured".into()),
                ..Default::default()
            };
        };
        let governor_authorized = match &c.governor_principal {
            Some(p) => self
                .permission(&Self::host_auth(p), Permission::ManageTrust)
                .await
                .is_ok(),
            None => false,
        };
        TrustStatus {
            configured: true,
            automatic: self.automatic && c.automatic,
            apply: c.apply,
            automatic_apply: self.automatic
                && c.automatic
                && c.automatic_apply
                && governor_authorized,
            calibrated: c.calibrated(),
            governor_authorized,
            running: self.is_busy(),
            reason: self.last_error.read().clone().or_else(|| {
                if !c.calibrated() {
                    Some("trust_proposals_only_without_calibration".into())
                } else if c.apply && !governor_authorized {
                    Some("trust_governor_not_authorized".into())
                } else {
                    None
                }
            }),
        }
    }
    /// Read a retained governed proposal. This is a trusted host interface;
    /// ordinary channels receive only the status switches, never global samples.
    pub async fn proposal(&self, id: &str) -> Result<TrustProposal, BoxError> {
        let job = self.job(id).await?;
        let p: TrustProposal = serde_json::from_value(
            self.proposer()?
                .read_artifact(DEFAULT_SPACE, &job.report)
                .await?,
        )?;
        if p.id != id || p.scope != self.scope {
            return Err("trust proposal identity mismatch".into());
        }
        Ok(p)
    }
    pub async fn receipt(&self, id: &str) -> Result<Option<TrustReceipt>, BoxError> {
        self.proposal(id).await?;
        Ok(self.job(id).await?.receipt)
    }
    pub(super) async fn job(&self, id: &str) -> Result<ProposalJob, BoxError> {
        if !crate::runtime_api::digest_valid(id) {
            return Err("invalid trust proposal identity".into());
        }
        Ok(self
            .journal
            .read::<ProposalJob>(&self.key("proposals", id)?)
            .await?
            .ok_or("trust proposal missing")?
            .value)
    }
    pub(super) async fn save_job(&self, job: &ProposalJob) -> Result<(), BoxError> {
        self.save(&self.key("proposals", &job.id)?, job).await
    }
    pub(super) async fn publish(
        &self,
        proposal: TrustProposal,
        sources: Vec<String>,
    ) -> Result<TrustProposal, BoxError> {
        if serde_json::to_vec(&proposal)?.len() > 262_144 || sources.len() > 256 {
            return Err("trust proposal material budget exceeded".into());
        }
        let report = self
            .proposer()?
            .put_artifact(DEFAULT_SPACE, json!(proposal), sources.clone())
            .await?;
        let job = ProposalJob {
            id: proposal.id.clone(),
            actor: proposal.actor_ref.clone(),
            report,
            sources,
            roots: proposal.roots.clone(),
            reviewed: None,
            governor: None,
            receipt: None,
            stale: false,
            notified: false,
        };
        let key = self.key("proposals", &job.id)?;
        if self.journal.read::<ProposalJob>(&key).await?.is_none() {
            self.journal.create(&key, &job).await?;
        }
        Ok(proposal)
    }
    pub(crate) fn kick(
        self: &Arc<Self>,
        consequences: Option<Arc<crate::consequence::ConsequenceRuntime>>,
    ) {
        if !self.automatic
            || !self.config.as_ref().is_some_and(|c| c.automatic)
            || self.running.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let this = self.clone();
        if self
            .tasks
            .start(async move {
                let _g = this.gate.lock().await;
                let result = std::panic::AssertUnwindSafe(this.pass(consequences))
                    .catch_unwind()
                    .await;
                *this.last_error.write() = (!matches!(result, Ok(Ok(()))))
                    .then(|| "trust_processing_requires_review_or_recovery".into());
                this.running.store(false, Ordering::SeqCst);
                Ok(())
            })
            .is_err()
        {
            self.running.store(false, Ordering::SeqCst);
        }
    }
    async fn pass(
        &self,
        consequences: Option<Arc<crate::consequence::ConsequenceRuntime>>,
    ) -> Result<(), BoxError> {
        let cfg = self.cfg()?;
        let mut catalog = self.catalog().await?;
        if let Some(c) = consequences {
            let (references, after) = c
                .trust_page(catalog.source_cursor, 8, &cfg.observer.principal_id)
                .await?;
            for reference in references {
                if let Err(error) = self.enqueue_inner(&reference).await {
                    if error.downcast_ref::<Ineligible>().is_none() {
                        return Err(error);
                    }
                    self.save(&self.key("discovery-excluded",&reference)?,&json!({"reference":reference,"reason":error.to_string(),"contract":cfg.contract_digest()?})).await?;
                }
            }
            catalog = self.catalog().await?;
            catalog.source_cursor = after;
            self.save("catalog", &catalog).await?;
        }
        if catalog.targets.is_empty() {
            return Ok(());
        }
        let id = catalog.targets[catalog.cursor % catalog.targets.len()].clone();
        let target = self
            .journal
            .read::<Target>(&id)
            .await?
            .ok_or("trust target missing")?
            .value;
        if target.contract == cfg.contract_digest()? {
            let proposal = self.propose_inner(&target.actor, false).await?;
            if cfg.apply && cfg.automatic_apply && proposal.new_weight.is_some() {
                let governor = cfg
                    .governor_principal
                    .as_ref()
                    .ok_or("trust governor missing")?;
                self.apply_inner(
                    Self::host_auth(governor),
                    &proposal.id,
                    "Registered calibrated method and explicit automatic-application policy",
                )
                .await?;
            }
        }
        catalog = self.catalog().await?;
        catalog.cursor = (catalog.cursor + 1) % catalog.targets.len();
        self.save("catalog", &catalog).await
    }
}

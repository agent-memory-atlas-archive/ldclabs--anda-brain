//! Bounded hot directory and permanent identity/review indexes. Native facts
//! and canonical replay journals outlive eviction from the orchestration set.
use super::*;

const CATALOG: &str = "catalog/v2";
const RESERVATION_BYTES: u64 = 64 * 1024 * 1024;

/// Logical journal reservation, separate from Nexus evidence retention.
/// Each identity reserves a bounded live journal, archive and index allowance.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LearningStoragePolicy {
    pub maximum_records: u64,
    pub maximum_reserved_bytes: u64,
}
impl Default for LearningStoragePolicy {
    fn default() -> Self {
        Self {
            maximum_records: 4096,
            maximum_reserved_bytes: 4096 * RESERVATION_BYTES,
        }
    }
}
impl LearningStoragePolicy {
    pub fn validate(&self) -> Result<(), BoxError> {
        if !(1..=1_000_000).contains(&self.maximum_records)
            || self.maximum_reserved_bytes < RESERVATION_BYTES
            || self.maximum_reserved_bytes > 1_000_000 * RESERVATION_BYTES
        {
            return Err("invalid learning journal storage bounds".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningCapacity {
    pub hot_jobs: usize,
    pub maximum_hot_jobs: usize,
    pub archived_jobs: u64,
    pub retained_identities: u64,
    pub reserved_bytes: u64,
    pub storage: LearningStoragePolicy,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobPage {
    pub items: Vec<JobReport>,
    pub next_after: u64,
    pub complete: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Catalog {
    format: String,
    instance: String,
    hot: BTreeMap<String, u64>,
    allocated: u64,
    archived: u64,
    policy: LearningStoragePolicy,
    pub(super) review_cursor: u64,
    pub(super) drive_cursor: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Locator {
    key: String,
    id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Archive {
    format: String,
    instance: String,
    journal_digest: String,
    job: Job,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveStamp {
    pub slot: u64,
    pub journal_digest: String,
    pub archived_at_ms: u64,
}

impl LearningRuntime {
    fn validate_snapshot(archive: &Archive, job: &Job) -> Result<(), BoxError> {
        if archive.format != "anda-brain:learning-archive-v1"
            || archive.instance != job.instance
            || archive.job.id != job.id
            || archive.job.registration_digest != job.registration_digest
            || archive.job.plan != job.plan
            || archive.job.origin != job.origin
            || archive.job.basis_proposition != job.basis_proposition
            || archive.journal_digest != content_digest(&json!(archive.job))?
        {
            return Err("learning archive identity/digest mismatch".into());
        }
        Ok(())
    }
    pub(super) async fn validate_archive(&self, job: &Job) -> Result<(), BoxError> {
        if let Some(stamp) = &job.archive {
            let archive = self
                .journal
                .read::<Archive>(&format!("archives/{}", stamp.slot))
                .await?
                .ok_or("learning archive missing")?
                .value;
            Self::validate_snapshot(&archive, job)?;
            if stamp.journal_digest != archive.journal_digest {
                return Err("archive stamp digest mismatch".into());
            }
        }
        Ok(())
    }
    /// Original terminal checkpoint, before later review links or revocation.
    /// Historical standing is not current recommendation/execution permission.
    pub async fn archive_replay(&self, id: &str) -> Result<JobReport, BoxError> {
        let reg = self.registration(false).await?;
        let job = self.load_job(&reg, id).await?.value;
        let stamp = job
            .archive
            .as_ref()
            .ok_or("learning job has not been archived")?;
        Ok(self
            .journal
            .read::<Archive>(&format!("archives/{}", stamp.slot))
            .await?
            .ok_or("archive missing")?
            .value
            .job
            .report())
    }
    pub(super) async fn pending_safety(&self, revision: &str) -> Result<bool, BoxError> {
        Ok(self
            .journal
            .read::<BTreeMap<String, String>>("catalog/safety")
            .await?
            .is_some_and(|r| r.value.values().any(|v| v == revision)))
    }
    pub(super) async fn track_safety(
        &self,
        job: &str,
        revision: &str,
        pending: bool,
    ) -> Result<(), BoxError> {
        let old = self
            .journal
            .read::<BTreeMap<String, String>>("catalog/safety")
            .await?;
        let mut value = old.as_ref().map(|r| r.value.clone()).unwrap_or_default();
        if pending {
            if !value.contains_key(job) && value.len() >= 32 {
                return Err("pending safety recovery capacity reached".into());
            }
            value.insert(job.into(), revision.into());
        } else {
            value.remove(job);
        }
        if let Some(mut old) = old {
            old.value = value;
            self.journal.save("catalog/safety", &old).await
        } else {
            self.journal.create("catalog/safety", &value).await
        }
    }
    pub(super) async fn safety_jobs(&self) -> Result<Vec<String>, BoxError> {
        Ok(self
            .journal
            .read::<BTreeMap<String, String>>("catalog/safety")
            .await?
            .map(|r| r.value.into_keys().collect())
            .unwrap_or_default())
    }
    async fn catalog(&self) -> Result<Versioned<Catalog>, BoxError> {
        let root = self
            .journal
            .read::<Catalog>(CATALOG)
            .await?
            .ok_or("learning catalog missing")?;
        root.value.policy.validate()?;
        let reg = self.registration(false).await?;
        if root.value.format != "anda-brain:learning-catalog-v2"
            || root.value.instance != reg.instance
            || root.value.hot.len() > 32
            || root.value.allocated > root.value.policy.maximum_records
            || root.value.archived > root.value.allocated
            || root.value.allocated * RESERVATION_BYTES > root.value.policy.maximum_reserved_bytes
            || root.value.archived + root.value.hot.len() as u64 != root.value.allocated
            || root
                .value
                .hot
                .values()
                .any(|slot| *slot == 0 || *slot > root.value.allocated)
        {
            return Err("invalid learning catalog identity/bounds".into());
        }
        Ok(root)
    }

    /// The only listing-based migration. The old admission bound makes this
    /// walk finite; numeric slots replace object-store listing afterwards.
    pub(super) async fn recover_catalog(&self) -> Result<(), BoxError> {
        if self.journal.read::<Catalog>(CATALOG).await?.is_none() {
            let reg = self.registration(false).await?;
            let mut root = Catalog {
                format: "anda-brain:learning-catalog-v2".into(),
                instance: reg.instance,
                ..Default::default()
            };
            for key in self.journal.legacy_jobs().await? {
                let row = self
                    .journal
                    .read::<Job>(&key)
                    .await?
                    .ok_or("legacy job disappeared")?;
                root.allocated += 1;
                self.journal
                    .create(
                        &format!("catalog/slots/{}", root.allocated),
                        &Locator {
                            key: key.clone(),
                            id: row.value.id,
                        },
                    )
                    .await?;
                root.hot.insert(key, root.allocated);
            }
            self.journal.create(CATALOG, &root).await?;
        }
        let root = self.catalog().await?;
        for (key, slot) in &root.value.hot {
            let job = if let Some(job) = self.journal.read::<Job>(key).await? {
                job.value
            } else {
                let prepared = self
                    .journal
                    .read::<Job>(&format!("enrollments/{}", &key[5..]))
                    .await?
                    .ok_or("reserved learning enrollment needs repair")?
                    .value;
                self.journal.create(key, &prepared).await?;
                prepared
            };
            self.journal
                .create(
                    &format!("catalog/slots/{slot}"),
                    &Locator {
                        key: key.clone(),
                        id: job.id.clone(),
                    },
                )
                .await?;
            if job
                .safety
                .as_ref()
                .is_some_and(|s| s.evaluation_ref.is_none())
            {
                self.track_safety(&job.id, &job.plan.candidate_revision, true)
                    .await?;
            }
            if job.archive.is_some() {
                self.finish_archive(key).await?;
            }
        }
        Ok(())
    }

    pub async fn capacity(&self) -> Result<LearningCapacity, BoxError> {
        let reg = self.registration(false).await?;
        let c = self.catalog().await?.value;
        Ok(LearningCapacity {
            hot_jobs: c.hot.len(),
            maximum_hot_jobs: reg.config.maximum_jobs,
            archived_jobs: c.archived,
            retained_identities: c.allocated,
            reserved_bytes: c.allocated * RESERVATION_BYTES,
            storage: c.policy,
        })
    }
    pub async fn configure_storage(
        self: &Arc<Self>,
        auth: AuthContext,
        policy: LearningStoragePolicy,
    ) -> Result<(), BoxError> {
        Self::require_host(&auth)?;
        policy.validate()?;
        self.owned(move |this| {
            Box::pin(async move {
                let _g = this.gate.lock().await;
                let mut c = this.catalog().await?;
                if c.value.allocated > policy.maximum_records
                    || c.value.allocated * RESERVATION_BYTES > policy.maximum_reserved_bytes
                {
                    return Err("storage bounds cannot discard retained learning identities".into());
                }
                c.value.policy = policy;
                this.journal.save(CATALOG, &c).await
            })
        })
        .await
    }
    pub(super) async fn hot_keys(&self) -> Result<Vec<String>, BoxError> {
        Ok(self.catalog().await?.value.hot.into_keys().collect())
    }
    pub(super) async fn check_capacity(&self, maximum_hot: usize) -> Result<(), BoxError> {
        let c = self.catalog().await?.value;
        if c.hot.len() >= maximum_hot {
            return Err("learning hot job capacity reached".into());
        }
        if c.allocated >= c.policy.maximum_records
            || (c.allocated + 1) * RESERVATION_BYTES > c.policy.maximum_reserved_bytes
        {
            return Err("learning retained journal storage capacity reached".into());
        }
        Ok(())
    }
    pub(super) async fn create_job(&self, job: &Job) -> Result<(), BoxError> {
        let key = Self::key(&job.id)?;
        // Full replay input is durable before reserving a discoverable hot slot.
        self.journal
            .create(&format!("enrollments/{}", &key[5..]), job)
            .await?;
        let mut c = self.catalog().await?;
        let slot = if let Some(slot) = c.value.hot.get(&key) {
            *slot
        } else {
            self.check_capacity(self.registration(false).await?.config.maximum_jobs)
                .await?;
            c.value.allocated += 1;
            let slot = c.value.allocated;
            c.value.hot.insert(key.clone(), slot);
            self.journal.save(CATALOG, &c).await?;
            slot
        };
        self.journal
            .create(
                &format!("catalog/slots/{slot}"),
                &Locator {
                    key: key.clone(),
                    id: job.id.clone(),
                },
            )
            .await?;
        self.journal.create(&key, job).await
    }
    pub async fn jobs_page(&self, after: u64, limit: usize) -> Result<JobPage, BoxError> {
        self.ensure_open()?;
        let reg = self.registration(false).await?;
        let c = self.catalog().await?.value;
        if !(1..=32).contains(&limit) || after > c.allocated {
            return Err("invalid learning page bound/cursor".into());
        }
        let end = c.allocated.min(after.saturating_add(limit as u64));
        let mut items = Vec::new();
        for slot in after + 1..=end {
            let row = self
                .journal
                .read::<Locator>(&format!("catalog/slots/{slot}"))
                .await?
                .ok_or("learning catalog publication incomplete; retry recovery")?
                .value;
            if row.key != Self::key(&row.id)? {
                return Err("learning catalog locator mismatch".into());
            }
            items.push(self.load_job(&reg, &row.id).await?.value.report());
        }
        Ok(JobPage {
            items,
            next_after: end,
            complete: end == c.allocated,
        })
    }
    /// Trusted audit read of a single retained instrument journal. Hidden
    /// validation state is never exposed through Recall, HTTP or MCP tools.
    pub async fn observation_replay(
        &self,
        id: &str,
        dispatch_id: &str,
    ) -> Result<Option<Json>, BoxError> {
        let reg = self.registration(false).await?;
        let job = self.load_job(&reg, id).await?.value;
        let a = job
            .attempts
            .values()
            .find(|a| a.ticket.dispatch_id == dispatch_id)
            .ok_or("learning attempt not found")?;
        let Some(key) = &a.replay_key else {
            return Ok(None);
        };
        Ok(Some(
            self.journal
                .read::<Json>(key)
                .await?
                .ok_or("independent observation replay missing")?
                .value,
        ))
    }
    pub(super) async fn indexed_job(
        &self,
        lane: &str,
        reference: &str,
    ) -> Result<Option<String>, BoxError> {
        Ok(self
            .journal
            .read::<String>(&format!(
                "catalog/{lane}/{}",
                &content_digest(&json!(reference))?[7..]
            ))
            .await?
            .map(|r| r.value))
    }
    async fn index_ref(&self, lane: &str, reference: &str, id: &str) -> Result<(), BoxError> {
        self.journal
            .create(
                &format!(
                    "catalog/{lane}/{}",
                    &content_digest(&json!(reference))?[7..]
                ),
                &id.to_string(),
            )
            .await
    }
    pub(super) async fn index_history(&self, job: &Job, slot: u64) -> Result<(), BoxError> {
        for a in job.attempts.values() {
            if !a.ticket.attempt_ref.is_empty() {
                self.index_ref("attempts", &a.ticket.attempt_ref, &job.id)
                    .await?;
            }
        }
        if let Some(reference) = &job.evaluation_ref {
            self.index_ref("evaluations", reference, &job.id).await?;
        }
        if job.review.is_some() {
            self.journal
                .create(&format!("catalog/reviews/{slot}"), &job.id)
                .await?;
        }
        Ok(())
    }
    async fn finish_archive(&self, key: &str) -> Result<(), BoxError> {
        let mut c = self.catalog().await?;
        if c.value.hot.remove(key).is_some() {
            c.value.archived += 1;
            self.journal.save(CATALOG, &c).await?;
        }
        Ok(())
    }

    /// Archives only a resolved terminal checkpoint. Unknown external effects
    /// keep their hot slot even after a fixed-cutoff negative verdict.
    pub async fn archive(self: &Arc<Self>, job_id: String) -> Result<JobReport, BoxError> {
        self.owned(move |this| Box::pin(async move {
            let _g = this.gate.lock().await;
            let reg = this.registration(false).await?;
            this.reconcile_outboxes(&reg, &job_id).await?;
            let mut row = this.load_job(&reg, &job_id).await?;
            let key = Self::key(&job_id)?;
            if row.value.archive.is_some() { this.finish_archive(&key).await?; return Ok(row.value.report()); }
            let j = &row.value;
            if !matches!(j.stage, JobStage::Settled | JobStage::Expired) || j.pending.is_some() || j.verdict_pending.is_some()
                || j.safety.as_ref().is_some_and(|s| s.evaluation_ref.is_none())
                || j.attempts.values().any(|a| a.state != DispatchState::Observed || !a.dispatch_reconciled || !a.task_completed)
            { return Err("learning archive blocked by unresolved intent, dispatch or terminal receipt".into()); }
            if let Some(reference) = &j.evaluation_ref { this.record(reference, "EvaluationRecord").await?; }
            else if j.stage == JobStage::Settled || j.activation_complete { return Err("terminal native verdict unavailable".into()); }
            if let Some(reference) = &j.trial_ref { this.record(reference, "TrialRecord").await?; }
            for a in j.attempts.values() {
                this.record(&a.ticket.attempt_ref, "AttemptRecord").await?;
                this.record(a.outcome_ref.as_deref().ok_or("outcome missing at archive")?, "OutcomeRecord").await?;
                if let Some(key) = &a.replay_key { this.journal.read::<Json>(key).await?.ok_or("independent replay missing at archive")?; }
            }
            if let Some(review) = &j.review {
                review.acquisition.validate_records(&this.record(&review.acquisition.trial_ref,"TrialRecord").await?,
                    &this.record(&review.acquisition.evaluation_ref,"EvaluationRecord").await?)?;
            }
            let c = this.catalog().await?;
            let slot = *c.value.hot.get(&key).ok_or("job is not in the hot catalog")?;
            this.index_history(j, slot).await?;
            let digest = if let Some(existing) = this.journal.read::<Archive>(&format!("archives/{slot}")).await? {
                Self::validate_snapshot(&existing.value, j)?;
                existing.value.journal_digest
            } else {
                let digest = content_digest(&json!(j))?;
                this.journal.create(&format!("archives/{slot}"), &Archive { format: "anda-brain:learning-archive-v1".into(), instance: reg.instance, journal_digest: digest.clone(), job: j.clone() }).await?;
                digest
            };
            row.value.archive = Some(ArchiveStamp { slot, journal_digest: digest, archived_at_ms: anda_engine::unix_ms() });
            this.journal.save(&key, &row).await?;
            this.finish_archive(&key).await?;
            Ok(row.value.report())
        })).await
    }
    pub(super) async fn archived_review_ids(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<(Vec<String>, u64, bool), BoxError> {
        let c = self.catalog().await?.value;
        if !(1..=32).contains(&limit) || after > c.allocated {
            return Err("invalid review cursor".into());
        }
        let end = c.allocated.min(after.saturating_add(limit as u64));
        let mut ids = vec![];
        for slot in after + 1..=end {
            if let Some(id) = self
                .journal
                .read::<String>(&format!("catalog/reviews/{slot}"))
                .await?
            {
                ids.push(id.value);
            }
        }
        Ok((ids, end, end == c.allocated))
    }
    pub(super) async fn scheduler_cursors(&self) -> Result<(u64, usize), BoxError> {
        let c = self.catalog().await?.value;
        Ok((c.review_cursor, c.drive_cursor))
    }
    pub(super) async fn advance_scheduler(
        &self,
        reviews: u64,
        drive: usize,
    ) -> Result<(), BoxError> {
        let _g = self.gate.lock().await;
        let mut c = self.catalog().await?;
        c.value.review_cursor = reviews;
        c.value.drive_cursor = drive;
        self.journal.save(CATALOG, &c).await
    }
}

//! Space credentials and tier limits.
use super::*;

impl Space {
    pub(super) fn get_tier(&self) -> SpaceTier {
        self.db.get_extension_as("tier").unwrap_or_default()
    }

    pub async fn admin_update_tier(&self, tier: u32, now_ms: u64) -> Result<SpaceTier, BoxError> {
        let tier = SpaceTier {
            tier,
            updated_at: now_ms,
        };
        self.db
            .save_extension_from("tier".to_string(), &tier.to_ref())
            .await?;
        Ok(tier)
    }

    pub async fn add_space_token(
        &self,
        token: String,
        input: AddSpaceTokenInput,
        now_ms: u64,
    ) -> Result<SpaceToken, BoxError> {
        // Serialize mints: the count cap and name-uniqueness checks below
        // read shared extension state, and two concurrent mints must not
        // both pass them.
        let _guard = self.token_lock.lock().await;
        let count = self
            .db
            .extensions_with(|kv| kv.keys().filter(|k| k.starts_with("ST")).count());
        if count >= 100 {
            return Err("space token limit reached".into());
        }

        // The token name is the audit identity (`st:{name}`): it is required
        // and unique, or two tokens would be indistinguishable in the event
        // log (and un-revokable by name).
        let name = input.name.trim().to_string();
        if name.is_empty() {
            return Err("space token name is required".into());
        }
        if self
            .list_space_tokens()?
            .iter()
            .any(|st| st.name.trim() == name)
        {
            return Err(format!("space token name {name:?} already exists").into());
        }

        let labels = match input.labels {
            Some(labels) => {
                // Label-restricted tokens are read-only wiki viewers (PRD
                // §8.2): any write scope would let them commit to, archive,
                // relabel or export documents behind labels they cannot read.
                if input.scope != TokenScope::Read {
                    return Err("labeled tokens must have read scope".into());
                }
                let mut cleaned: Vec<String> = labels
                    .iter()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                cleaned.sort();
                cleaned.dedup();
                if cleaned.is_empty() && !labels.is_empty() {
                    return Err("labels must not be blank".into());
                }
                Some(cleaned)
            }
            None => None,
        };

        let sp = SpaceToken {
            token: token.clone(),
            scope: input.scope,
            name,
            expires_at: input.expires_at,
            labels,
            created_at: now_ms,
            updated_at: now_ms,
            ..Default::default()
        };

        self.db.save_extension_from(token, &sp.to_ref()).await?;
        Ok(sp)
    }

    pub fn verify_space_token(
        &self,
        token: String,
        scope: TokenScope,
        now_ms: u64,
    ) -> Result<SpaceToken, BoxError> {
        // Space tokens always carry the "ST" prefix. Rejecting other keys here
        // keeps non-token extensions (e.g. "byok", "tier") out of the
        // credential lookup below.
        if !token.starts_with("ST") {
            return Err("invalid space token".into());
        }
        let token = self
            .db
            .set_extension_from_with::<_, SpaceToken>(token, |v| {
                if let Some(mut st) = v
                    && st.expires_at.map(|exp| exp > now_ms).unwrap_or(true)
                    && st.scope.allows(scope)
                    // Labeled tokens are read-only wiki viewers; a legacy row
                    // carrying a write scope fails closed here (PRD §8.2).
                    && (st.labels.is_none() || scope == TokenScope::Read)
                {
                    st.usage = st.usage.saturating_add(1);
                    st.updated_at = now_ms;
                    return Some(st);
                }
                None
            });

        token.ok_or_else(|| "invalid space token".into())
    }

    pub async fn revoke_space_token(&self, token: &str) -> Result<bool, BoxError> {
        // Same guard as verify_space_token: the token is caller-supplied, so
        // restricting it to the "ST" prefix keeps non-token extensions
        // (e.g. "byok", "tier", "owner") safe from deletion through this API.
        if !token.starts_with("ST") {
            return Err("invalid space token".into());
        }
        let rt = self.db.remove_extension(token).await?;
        Ok(rt.is_some())
    }

    /// Revokes a token by its (unique) name. This is the recovery path for
    /// managers who did not save the token value at mint time —
    /// `list_space_tokens` deliberately never echoes full token values.
    pub async fn revoke_space_token_by_name(&self, name: &str) -> Result<bool, BoxError> {
        let name = name.trim();
        if name.is_empty() {
            return Err("invalid space token name".into());
        }
        let key = self.db.extensions_with(|kvs| {
            kvs.iter().find_map(|(k, v)| {
                (k.starts_with("ST")
                    && v.clone()
                        .deserialized::<SpaceToken>()
                        .is_ok_and(|st| st.name.trim() == name))
                .then(|| k.clone())
            })
        });
        match key {
            Some(key) => Ok(self.db.remove_extension(&key).await?.is_some()),
            None => Ok(false),
        }
    }

    pub fn list_space_tokens(&self) -> Result<Vec<SpaceToken>, BoxError> {
        let tokens: Vec<SpaceToken> = self.db.extensions_with(|kvs| {
            kvs.iter()
                .filter_map(|(k, v)| {
                    if k.starts_with("ST")
                        && let Ok(mut st) = v.clone().deserialized::<SpaceToken>()
                    {
                        // The map key *is* the bearer credential: expose only
                        // a display prefix, or any Write-scoped manager could
                        // harvest every other caller's token in plaintext.
                        st.token = if k.len() > 8 {
                            format!("{}…", k.chars().take(8).collect::<String>())
                        } else {
                            k.clone()
                        };
                        Some(st)
                    } else {
                        None
                    }
                })
                .collect()
        });

        Ok(tokens)
    }
}

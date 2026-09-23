//! Bounded conditional JSON objects. Callers own their key domains and state
//! machines; this module only performs storage I/O and exact acknowledgement.
use anda_core::BoxError;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use serde::{Serialize, de::DeserializeOwned};

pub(crate) struct Versioned<T> {
    pub value: T,
    pub version: UpdateVersion,
}

pub(crate) async fn read<T: DeserializeOwned>(
    store: &dyn ObjectStore,
    path: &Path,
    maximum: u64,
    context: &str,
) -> Result<Option<Versioned<T>>, BoxError> {
    let result = match store.get(path).await {
        Ok(result) => result,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if result.meta.size > maximum {
        return Err(format!("{context} object exceeds bound").into());
    }
    let version = UpdateVersion {
        e_tag: result.meta.e_tag.clone(),
        version: result.meta.version.clone(),
    };
    if version.e_tag.is_none() && version.version.is_none() {
        return Err(format!("{context} requires conditional-update storage").into());
    }
    let bytes = result.bytes().await?;
    Ok(Some(Versioned {
        value: serde_json::from_slice(&bytes)?,
        version,
    }))
}

pub(crate) async fn put<T: Serialize>(
    store: &dyn ObjectStore,
    path: &Path,
    value: &T,
    mode: PutMode,
    maximum: u64,
    context: &str,
) -> Result<(), BoxError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 > maximum {
        return Err(format!("{context} object exceeds bound").into());
    }
    // Resolve even an ACK-lost PUT by comparing the exact stored bytes. Never
    // cancel this write inside the helper: the caller's durable owner drains it.
    let result = store
        .put_opts(path, bytes.clone().into(), mode.into())
        .await;
    let stored = store.get(path).await?;
    if stored.meta.size > maximum {
        return Err(format!("{context} readback exceeds bound").into());
    }
    if stored.bytes().await?.as_ref() != bytes {
        return Err(result
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("{context} conditional-write conflict"))
            .into());
    }
    Ok(())
}

//! Generates an object-store snapshot using the published storage/runtime
//! versions from Anda Brain v0.11.0's Cargo.lock. No local path patches or LLM.
use anda_cognitive_nexus::CognitiveNexus;
use anda_db::database::{AndaDB, DBConfig};
use anda_kip::{PERSON_SELF_KIP, PERSON_SYSTEM_KIP, parse_kml};
use futures::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct StoredObject {
    path: String,
    #[serde(with = "serde_bytes")]
    bytes: Vec<u8>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let destination = std::env::args().nth(1).expect("output snapshot path");
    let store = Arc::new(InMemory::new());
    let db = Arc::new(AndaDB::create(store.clone(), DBConfig {
        name: "published_v011_fixture".into(),
        description: "Synthetic published KIP v1 upgrade fixture".into(),
        ..Default::default()
    }).await?);
    let nexus = CognitiveNexus::connect(db.clone(), async |_| Ok(())).await?;
    nexus.execute_kml(parse_kml(&format!("{PERSON_SELF_KIP}\n{PERSON_SYSTEM_KIP}"))?, false).await?;
    nexus.execute_kml(parse_kml(include_str!("../seed.kip"))?, false).await?;
    db.close().await?;
    let mut objects: Vec<_> = store.list(None).try_collect().await?;
    objects.sort_by(|a,b| a.location.cmp(&b.location));
    let mut snapshot = Vec::new();
    for object in objects {
        snapshot.push(StoredObject {
            path: object.location.to_string(),
            bytes: store.get(&object.location).await?.bytes().await?.to_vec(),
        });
    }
    let mut file = std::fs::File::create(destination)?;
    cbor2::to_writer(&snapshot, &mut file)?;
    println!("saved {} objects", snapshot.len());
    Ok(())
}

use anda_core::BoxError;
use ic_auth_types::ByteBufB64;
use ic_cose_types::cose::{CoseKey, ed25519::VerifyingKey, get_cose_key_public};
use std::str::FromStr;

/// A Cognitive Memory Profile symbol: `profile!("Watch")` is
/// `kip://profiles/cognitive-memory@2.0.0/Watch`, `profile!()` the prefix.
/// One spelling, so a package bump is one edit; the reference alone does not
/// identify a draft revision, the bundled content digest does.
macro_rules! profile {
    () => {
        "kip://profiles/cognitive-memory@2.0.0/"
    };
    ($symbol:literal) => {
        concat!("kip://profiles/cognitive-memory@2.0.0/", $symbol)
    };
}

/// The Cognitive Memory Profile prefix every Profile symbol lives under.
pub(crate) const PROFILE: &str = profile!();

pub mod action;
pub mod agents;
pub mod assess;
pub mod attention;
pub(crate) mod authz;
pub(crate) mod cognitive;
pub mod consequence;
pub mod handler;
pub(crate) mod kip;
pub(crate) mod kip_reference;
#[cfg(feature = "learning")]
pub mod learning;
pub mod runtime_api;

mod model_budget;
mod persisted;
mod runtime;
#[cfg(feature = "experiments")]
pub use space::experiments;
#[path = "learning/journal.rs"]
pub(crate) mod journal;
pub(crate) mod ledger;
#[cfg(test)]
mod legacy_upgrade;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod payload;
pub mod product;
pub mod recall_budget;
pub mod recall_receipt;
pub(crate) mod settlement;
pub mod space;
#[cfg(test)]
pub(crate) mod testkit;
pub mod types;
pub(crate) mod vocabulary;
#[cfg(feature = "wiki")]
pub(crate) mod wiki;

pub fn parse_ed25519_pubkeys(input: &str) -> Result<Vec<VerifyingKey>, BoxError> {
    if input.is_empty() {
        return Ok(vec![]);
    }

    input
        .split(',')
        .map(|item| match parse_ed25519_pubkey(item.trim()) {
            Some(key) => Ok(key),
            None => Err("invalid ED25519_PUBKEYS entry".into()),
        })
        .collect::<Result<Vec<_>, _>>()
}

fn parse_ed25519_pubkey(input: &str) -> Option<VerifyingKey> {
    let data = ByteBufB64::from_str(input).ok()?;

    if data.len() == 32 {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&data);
        return VerifyingKey::from_bytes(&bytes).ok();
    }

    let cose_key = CoseKey::from_slice(data.as_slice()).ok()?;
    let public_key = get_cose_key_public(cose_key).ok()?;
    let bytes: [u8; 32] = public_key.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::parse_ed25519_pubkeys;

    #[test]
    fn profile_prefix_names_the_bundled_package() {
        assert_eq!(
            super::PROFILE,
            format!("{}/", anda_cognitive_nexus::profiles::COGNITIVE_MEMORY_REF)
        );
        assert_eq!(profile!("Watch"), format!("{}Watch", super::PROFILE));
    }
    use cose2::{Key as CoseKey, iana};
    use ic_auth_types::ByteBufB64;

    fn ed25519_basepoint_bytes() -> [u8; 32] {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        bytes
    }

    #[test]
    fn parse_ed25519_pubkeys_allows_empty_input() {
        let keys = parse_ed25519_pubkeys("").unwrap();

        assert!(keys.is_empty());
    }

    #[test]
    fn parse_ed25519_pubkeys_accepts_raw_keys_and_trims_items() {
        let key_bytes = ed25519_basepoint_bytes();
        let encoded = ByteBufB64(key_bytes.to_vec()).to_string();
        let keys = parse_ed25519_pubkeys(&format!(" {encoded} , {encoded} ")).unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].to_bytes(), key_bytes);
        assert_eq!(keys[1].to_bytes(), key_bytes);
    }

    #[test]
    fn parse_ed25519_pubkeys_accepts_cose_key_entries() {
        let key_bytes = ed25519_basepoint_bytes();
        let mut cose_key = CoseKey::new();
        cose_key.set_kty(iana::KeyTypeOKP);
        cose_key.insert(iana::OKPKeyParameterX, key_bytes.to_vec());
        let encoded = ByteBufB64(cose_key.to_vec().unwrap()).to_string();

        let keys = parse_ed25519_pubkeys(&encoded).unwrap();

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].to_bytes(), key_bytes);
    }

    #[test]
    fn parse_ed25519_pubkeys_rejects_invalid_entries() {
        let short_key = ByteBufB64(vec![1, 2, 3]).to_string();

        assert!(parse_ed25519_pubkeys("not base64").is_err());
        assert!(parse_ed25519_pubkeys(&short_key).is_err());
        assert!(parse_ed25519_pubkeys(" ").is_err());
    }
}

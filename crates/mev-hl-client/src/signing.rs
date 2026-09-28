//! EIP-712 signing for HyperCore actions (SPEC-0002 §4).
//!
//! L1 (trading) actions are signed via the *phantom agent* flow:
//!
//! 1. msgpack-encode the action,
//! 2. append `nonce`, the vault flag/address, and optional `expiresAfter`,
//! 3. Keccak-256 the concatenation → `connectionId`,
//! 4. EIP-712 sign `Agent { source, connectionId }` under the fixed `Exchange`
//!    domain (`chainId` 1337, zero address) with the agent wallet.
//!
//! The msgpack bytes must match Hyperliquid's own encoder exactly, since the
//! server re-encodes the action to verify the signature. This module is tested
//! against golden vectors produced by the official Python SDK
//! (`hyperliquid.utils.signing`).

use alloy_primitives::{Address, B256, hex, keccak256};
use alloy_sol_types::{Eip712Domain, SolStruct, eip712_domain, sol};
use k256::ecdsa::{RecoveryId, SigningKey, VerifyingKey};
use mev_core::error::{Error, Result};
use serde::Serialize;

sol! {
    /// Hyperliquid phantom agent signed for L1 actions.
    struct Agent {
        string source;
        bytes32 connectionId;
    }
}

/// EIP-712 domain for L1 trading actions. `chainId` is hardcoded to 1337 and is
/// independent of the wallet's network.
pub const EXCHANGE_DOMAIN: Eip712Domain = eip712_domain! {
    name: "Exchange",
    version: "1",
    chain_id: 1337,
    verifying_contract: Address::ZERO,
};

/// ECDSA signature over the EIP-712 digest, shaped for the `/exchange` envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// `r` scalar.
    pub r: B256,
    /// `s` scalar.
    pub s: B256,
    /// Recovery id encoded as 27/28.
    pub v: u8,
}

impl Signature {
    /// `r` as a `0x`-prefixed 32-byte hex string.
    pub fn r_hex(&self) -> String {
        format!("0x{}", hex::encode(self.r))
    }

    /// `s` as a `0x`-prefixed 32-byte hex string.
    pub fn s_hex(&self) -> String {
        format!("0x{}", hex::encode(self.s))
    }

    /// Parse a signature from `r`/`s` bytes and a 27/28 `v`.
    pub fn from_parts(r: B256, s: B256, v: u8) -> Result<Self> {
        if v != 27 && v != 28 {
            return Err(Error::Config(format!("invalid signature v: {v}")));
        }
        Ok(Self { r, s, v })
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Signature", 3)?;
        state.serialize_field("r", &self.r_hex())?;
        state.serialize_field("s", &self.s_hex())?;
        state.serialize_field("v", &self.v)?;
        state.end()
    }
}

/// Compute the L1 action hash (`connectionId`).
pub fn action_hash<A: Serialize>(
    action: &A,
    vault_address: Option<Address>,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<B256> {
    let mut data = rmp_serde::to_vec_named(action).map_err(|e| Error::Decode(e.to_string()))?;
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault_address {
        None => data.push(0),
        Some(address) => {
            data.push(1);
            data.extend_from_slice(address.as_slice());
        }
    }
    if let Some(expires_after) = expires_after {
        data.push(0);
        data.extend_from_slice(&expires_after.to_be_bytes());
    }
    Ok(keccak256(data))
}

/// EIP-712 digest signed by the agent for a given action.
pub fn signing_hash<A: Serialize>(
    action: &A,
    source: &str,
    vault_address: Option<Address>,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<B256> {
    let connection_id = action_hash(action, vault_address, nonce, expires_after)?;
    let agent = Agent {
        source: source.to_string(),
        connectionId: connection_id,
    };
    Ok(agent.eip712_signing_hash(&EXCHANGE_DOMAIN))
}

/// An agent wallet that signs L1 actions.
pub struct AgentSigner {
    key: SigningKey,
    source: &'static str,
}

impl AgentSigner {
    /// Build a signer from a `0x`-prefixed 32-byte private key.
    pub fn from_hex(private_key: &str, mainnet: bool) -> Result<Self> {
        let raw = private_key.strip_prefix("0x").unwrap_or(private_key);
        let bytes =
            hex::decode(raw).map_err(|e| Error::Config(format!("agent key is not hex: {e}")))?;
        let key = SigningKey::from_slice(&bytes)
            .map_err(|e| Error::Config(format!("invalid agent key: {e}")))?;
        Ok(Self {
            key,
            source: if mainnet { "a" } else { "b" },
        })
    }

    /// Generate an ephemeral random signer from the OS CSPRNG.
    ///
    /// Used for `simulate` runs that have no configured key (SPEC-0002 H-8):
    /// the key is never persisted, logged, or written to metrics, and its
    /// address holds no funds. `mainnet` selects the EIP-712 `source` prefix
    /// exactly like [`Self::from_hex`], so signatures verify against the same
    /// domain regardless of where the key came from.
    pub fn ephemeral(mainnet: bool) -> Self {
        use k256::elliptic_curve::rand_core::OsRng;
        Self {
            key: SigningKey::random(&mut OsRng),
            source: if mainnet { "a" } else { "b" },
        }
    }

    /// The agent wallet address.
    pub fn address(&self) -> Address {
        address_of(self.key.verifying_key())
    }

    /// Sign an L1 action, returning the `{r, s, v}` signature.
    pub fn sign_l1<A: Serialize>(
        &self,
        action: &A,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
    ) -> Result<Signature> {
        let digest = signing_hash(action, self.source, vault_address, nonce, expires_after)?;
        sign_hash(&self.key, digest)
    }
}

/// Sign a 32-byte digest (deterministic RFC 6979 ECDSA).
pub fn sign_hash(key: &SigningKey, digest: B256) -> Result<Signature> {
    let (signature, recovery_id) = key
        .sign_prehash_recoverable(digest.as_slice())
        .map_err(|e| Error::Config(format!("signing failed: {e}")))?;
    let bytes = signature.to_bytes();
    Ok(Signature {
        r: B256::from_slice(&bytes[..32]),
        s: B256::from_slice(&bytes[32..]),
        v: 27 + recovery_id.to_byte(),
    })
}

/// Recover the signer address from a digest and signature.
pub fn recover_address(digest: B256, signature: &Signature) -> Result<Address> {
    let sig = signature_to_k256(signature)?;
    let recovery_id = RecoveryId::from_byte(signature.v - 27)
        .ok_or_else(|| Error::Decode("invalid recovery id".into()))?;
    let key = VerifyingKey::recover_from_prehash(digest.as_slice(), &sig, recovery_id)
        .map_err(|e| Error::Decode(format!("recovery failed: {e}")))?;
    Ok(address_of(&key))
}

fn signature_to_k256(signature: &Signature) -> Result<k256::ecdsa::Signature> {
    let mut bytes = [0u8; 64];
    bytes[..32].copy_from_slice(signature.r.as_slice());
    bytes[32..].copy_from_slice(signature.s.as_slice());
    k256::ecdsa::Signature::from_slice(&bytes)
        .map_err(|e| Error::Decode(format!("invalid signature: {e}")))
}

/// Keccak-256 of the uncompressed public key (minus the `0x04` prefix), last 20 bytes.
pub(crate) fn address_of(key: &VerifyingKey) -> Address {
    let point = key.to_encoded_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Action, Grouping, Tif, limit_order};

    // Public, zero-funds throwaway key (the second default Anvil/Hardhat dev
    // account), *not* a secret. Signing here is deterministic (RFC 6979), so
    // these vectors are reproducible against the official Python SDK. Never use
    // it on mainnet.
    /// Key/address used to generate the golden vectors.
    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const ADDRESS: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    fn simple_action() -> Action {
        Action::Order {
            orders: vec![limit_order(0, true, "50000", "0.1", Tif::Gtc, false, None)],
            grouping: Grouping::Na,
        }
    }

    #[test]
    fn agent_address_derives_correctly() {
        let signer = AgentSigner::from_hex(KEY, true).unwrap();
        assert_eq!(
            format!("{:?}", signer.address()).to_lowercase(),
            ADDRESS.to_lowercase()
        );
    }

    #[test]
    fn action_hash_matches_official_sdk() {
        // Golden vector from hyperliquid.utils.signing.action_hash
        let action = simple_action();
        let hash = action_hash(&action, None, 1_700_000_000_000, None).unwrap();
        assert_eq!(
            hex::encode(hash),
            "895e933f37f9cb801caa82867db0914afd94ad804bfed835bef33b0cca8bcf6b"
        );
    }

    #[test]
    fn action_hash_with_vault_and_expires_matches_sdk() {
        let action = Action::Order {
            orders: vec![limit_order(
                0,
                true,
                "0.000012",
                "100",
                Tif::Gtc,
                false,
                None,
            )],
            grouping: Grouping::Na,
        };
        let vault = "0x0000000000000000000000000000000000000001"
            .parse::<Address>()
            .unwrap();
        let hash = action_hash(
            &action,
            Some(vault),
            1_700_000_000_003,
            Some(1_700_000_060_000),
        )
        .unwrap();
        assert_eq!(
            hex::encode(hash),
            "2c4a58007f7beffc19f6ef8c5c99ff5b6d15659e07817e58905c7adbfd235cca"
        );
    }

    #[test]
    fn msgpack_encoding_matches_sdk() {
        // The msgpack bytes must be byte-identical to Python's msgpack.packb.
        let encoded = rmp_serde::to_vec_named(&simple_action()).unwrap();
        assert_eq!(
            hex::encode(&encoded),
            "83a474797065a56f72646572a66f72646572739186a16100\
             a162c3a170a53530303030a173a3302e31a172c2a17481a56c696d697481a3746966a3477463\
             a867726f7570696e67a26e61"
        );
    }

    #[test]
    fn signature_matches_official_sdk_and_recovers() {
        let signer = AgentSigner::from_hex(KEY, true).unwrap();
        let action = simple_action();
        let nonce = 1_700_000_000_000;
        let signature = signer.sign_l1(&action, nonce, None, None).unwrap();

        // Golden vector signature from the official SDK (deterministic RFC 6979).
        assert_eq!(
            signature.r_hex(),
            "0x22197d580808d466389942c713673698403517e7b21e2a780e3d29d80acbb4a6"
        );
        assert_eq!(
            signature.s_hex(),
            "0x26ad7a602c6dbf877b697c75fcf70163c77a599820c54729c7b09ba9746a5ad1"
        );
        assert_eq!(signature.v, 28);

        let digest = signing_hash(&action, "a", None, nonce, None).unwrap();
        assert_eq!(
            format!("{:?}", recover_address(digest, &signature).unwrap()).to_lowercase(),
            ADDRESS.to_lowercase()
        );
    }

    #[test]
    fn testnet_source_changes_the_digest() {
        let action = simple_action();
        let mainnet = signing_hash(&action, "a", None, 42, None).unwrap();
        let testnet = signing_hash(&action, "b", None, 42, None).unwrap();
        assert_ne!(mainnet, testnet);
    }

    #[test]
    fn rejects_malformed_key() {
        assert!(AgentSigner::from_hex("0xdeadbeef", true).is_err());
        assert!(AgentSigner::from_hex("not-hex", true).is_err());
    }

    #[test]
    fn ephemeral_signers_are_random_and_sign() {
        let a = AgentSigner::ephemeral(true);
        let b = AgentSigner::ephemeral(true);
        assert_ne!(a.address(), b.address());
        // An ephemeral key signs normally, so the simulate path can exercise
        // the full build + sign flow.
        assert!(a.sign_l1(&simple_action(), 42, None, None).is_ok());
    }
}

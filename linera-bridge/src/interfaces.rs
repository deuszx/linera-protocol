// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Stable bridge-facing interfaces for claim verification.
//!
//! # Why this exists
//!
//! Bridge verification spans multiple components (relayer, proof fetcher, finality
//! backend, and on-chain submission client). This module provides a stable Rust API
//! boundary so those components can compile against shared types before verifier
//! business logic is implemented.
//!
//! # Intended usage
//!
//! - Proof parsers produce [`BridgeClaim`] from finalized EVM event data; users do
//!   not choose claim recipients/amounts out-of-band.
//! - Proof decoders implement [`ClaimProofDecoder`] to turn submitted proof
//!   material into a canonical [`BridgeClaim`].
//! - Storage/replay protection uses [`ClaimId`] as a deterministic deduplication key.
//! - Backend adapters (e.g. Linera RPC/indexer, contract readers) implement
//!   [`FinalityView`] and [`RootLookup`] to supply finality and root data.
//! - Verification orchestration maps failures into [`ClaimVerificationError`] so
//!   callers can handle deterministic failure classes uniformly.
//!
//! This module intentionally contains only models, trait boundaries, and typed
//! errors. It does not implement business logic. Downstream crates can depend on
//! these API contracts while verification internals are implemented incrementally.
//!
//! # Why this is worth keeping (even if minimal)
//!
//! The user submits proof bytes, but the protocol still needs one canonical Rust
//! representation after decoding (`BridgeClaim`), one deterministic replay key
//! (`ClaimId`), and one stable way to classify failures (`ClaimVerificationError`).
//! Without these shared contracts, each component (decoder, verifier, executor)
//! tends to duplicate slightly different structs/error mappings, causing fragile
//! integration bugs and nondeterministic behavior across crates.
//!
//! # Deliberate scope limits
//!
//! This module should remain small and boring. We only keep boundaries that are
//! required by the claim path today:
//! - decode proof -> `BridgeClaim` (`ClaimProofDecoder`)
//! - check finality/root (`FinalityView` + `RootLookup`)
//! - derive replay key (`ClaimId`)
//! - surface deterministic classes (`ClaimVerificationError`)
//!
//! If a type/trait is not needed by this concrete flow, it should not be added.

use alloy_primitives::keccak256;
use linera_base::{
    crypto::CryptoHash,
    data_types::Amount,
    identifiers::{Account, AccountOwner, ChainId},
};

/// Canonical model for a bridge claim extracted from finalized source-chain data.
///
/// `BridgeClaim` is a *normalized projection* of a finalized source event and its
/// inclusion/finality proof. The claimant submits proof material, and bridge code
/// decodes that proof into this struct; it is not a user-authored payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeClaim {
    /// Stable identifier of the finalized source event/proof this claim comes from.
    ///
    /// This binds the claim to a unique source-side event, enabling replay
    /// protection and auditable traceability to the submitted proof.
    pub source_event_id: CryptoHash,
    /// Owner of the account to debit on the source chain.
    pub owner: AccountOwner,
    /// Source chain from which the value is claimed.
    pub source_chain_id: ChainId,
    /// Recipient account on the destination chain.
    pub recipient: Account,
    /// Amount to claim.
    pub amount: Amount,
}

/// Stable identifier for a [`BridgeClaim`].
///
/// This value is derived deterministically from the claim payload and can be
/// used as a deduplication key in storage and APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClaimId(pub CryptoHash);

impl ClaimId {
    /// Derives a deterministic claim identifier from a [`BridgeClaim`].
    ///
    /// The helper uses BCS canonical encoding for the tuple of claim fields and
    /// hashes it with Keccak-256.
    pub fn derive(claim: &BridgeClaim) -> Result<Self, bcs::Error> {
        let claim_bytes = bcs::to_bytes(&(
            claim.source_event_id,
            claim.owner,
            claim.source_chain_id,
            claim.recipient,
            claim.amount,
        ))?;
        Ok(Self(CryptoHash::from(keccak256(claim_bytes).0)))
    }
}

/// Boundary for decoding user-submitted proof material into a canonical claim.
///
/// This captures the concrete Linera-side flow where users submit source-chain
/// finalization proof + event data, and verifier code extracts the claim payload
/// from that proof.
pub trait ClaimProofDecoder<Proof> {
    /// Backend-specific decoding error.
    type Error;

    /// Decodes `proof` into a canonical [`BridgeClaim`].
    fn decode_claim(&self, proof: &Proof) -> Result<BridgeClaim, Self::Error>;
}
/// Abstract boundary to query finality for source-chain artifacts.
pub trait FinalityView {
    /// Backend-specific error when finality cannot be queried.
    type Error;

    /// Returns whether `artifact_id` is finalized.
    fn is_finalized(&self, artifact_id: CryptoHash) -> Result<bool, Self::Error>;
}

/// Abstract boundary to resolve a root/commitment for a finalized artifact.
pub trait RootLookup {
    /// Backend-specific error when root lookup cannot be performed.
    type Error;

    /// Returns the root commitment associated with `artifact_id`, if known.
    fn root_for_artifact(&self, artifact_id: CryptoHash)
        -> Result<Option<CryptoHash>, Self::Error>;
}

/// Deterministic claim-verification failure classes.
///
/// Variants are intentionally stable and backend-agnostic so callers can map
/// outcomes to protocol-level behavior deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimVerificationError {
    /// The submitted proof/event payload could not be decoded.
    ProofDecodingFailed,
    /// The claim payload is malformed or semantically invalid.
    InvalidClaim,
    /// The claim references an artifact that is not finalized.
    NotFinal,
    /// No root commitment was found for the referenced artifact.
    RootNotFound,
    /// The provided proof root does not match the looked-up commitment.
    RootMismatch,
}

#[cfg(test)]
mod tests {
    use linera_base::{
        crypto::{CryptoHash, TestString},
        data_types::Amount,
        identifiers::{Account, AccountOwner, ChainId},
    };

    use super::{BridgeClaim, ClaimId, ClaimProofDecoder};

    #[test]
    fn claim_id_derivation_is_stable_for_identical_claims() {
        let source_event_id = CryptoHash::new(&TestString::new("evm_log"));
        let owner = AccountOwner::Address32(CryptoHash::new(&TestString::new("owner")));
        let source_chain_id = ChainId(CryptoHash::new(&TestString::new("source")));
        let recipient = Account::chain(source_chain_id);
        let amount = Amount::from_tokens(1234);

        let claim = BridgeClaim {
            source_event_id,
            owner,
            source_chain_id,
            recipient,
            amount,
        };

        let lhs = ClaimId::derive(&claim).expect("BCS serialization should succeed");
        let rhs = ClaimId::derive(&claim).expect("BCS serialization should succeed");

        assert_eq!(lhs, rhs);
    }

    struct IdentityDecoder;

    impl ClaimProofDecoder<BridgeClaim> for IdentityDecoder {
        type Error = core::convert::Infallible;

        fn decode_claim(&self, proof: &BridgeClaim) -> Result<BridgeClaim, Self::Error> {
            Ok(proof.clone())
        }
    }

    #[test]
    fn claim_proof_decoder_boundary_is_usable() {
        let source_event_id = CryptoHash::new(&TestString::new("evm_log"));
        let owner = AccountOwner::Address32(CryptoHash::new(&TestString::new("owner")));
        let source_chain_id = ChainId(CryptoHash::new(&TestString::new("source")));
        let recipient = Account::chain(source_chain_id);
        let amount = Amount::from_tokens(1234);

        let claim = BridgeClaim {
            source_event_id,
            owner,
            source_chain_id,
            recipient,
            amount,
        };

        let decoded = IdentityDecoder
            .decode_claim(&claim)
            .expect("identity decoder should succeed");

        assert_eq!(decoded, claim);
    }
}

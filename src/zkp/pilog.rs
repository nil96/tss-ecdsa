// Copyright (c) Facebook, Inc. and its affiliates.
// Modifications Copyright (c) 2022-2023 Bolt Labs Holdings, Inc
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree and the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree.

//! Implements a zero-knowledge proof that a discrete log commitment and
//! Paillier encryption contain the same underlying plaintext and that plaintext
//! falls within a given range.
//!
//! The proof is defined in Figure 25 of CGGMP[^cite], and uses a standard
//! Fiat-Shamir transformation to make the proof non-interactive.
//!
//! [^cite]: Ran Canetti, Rosario Gennaro, Steven Goldfeder, Nikolaos Makriyannis, and Udi Peled.
//! UC Non-Interactive, Proactive, Threshold ECDSA with Identifiable Aborts.
//! [EPrint archive, 2021](https://eprint.iacr.org/2021/060.pdf).

use crate::{
    errors::*,
    paillier::{Ciphertext, EncryptionKey, MaskedNonce, Nonce},
    parameters::{ELL, EPSILON},
    ring_pedersen::{Commitment, MaskedRandomness, RingPedersen},
    utils::{
        self, plusminus_bn_random_from_transcript, random_plusminus_by_size, within_bound_by_size,
    },
    zkp::Proof,
};
use libpaillier::unknown_order::BigNumber;
use merlin::Transcript;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use tracing::warn;
use utils::CurvePoint;
use zeroize::ZeroizeOnDrop;

/// Proof of knowledge that:
/// 1. the committed value in a discrete log commitment and the plaintext value
/// of a Paillier encryption are equal, and
/// 2. the plaintext value is in the valid range (in this case `± 2^{ℓ + ε}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PiLogProof {
    /// Commitment to the (secret) [plaintext](ProverSecret::plaintext) (`S` in
    /// the paper).
    plaintext_commit: Commitment,
    /// Paillier encryption of mask value (`A` in the paper).
    mask_ciphertext: Ciphertext,
    /// Discrete log commitment of mask value (`Y` in the paper).
    mask_dlog_commit: CurvePoint,
    /// Ring-Pedersen commitment of mask value (`D` in the paper).
    mask_commit: Commitment,
    /// Fiat-Shamir challenge (`e` in the paper).
    challenge: BigNumber,
    /// Response binding the (secret) plaintext with the mask value
    /// (`z1` in the paper).
    plaintext_response: BigNumber,
    /// Response binding the (secret) nonce with the nonce corresponding to
    /// [`PiLogProof::mask_ciphertext`] (`z2` in the paper).
    nonce_response: MaskedNonce,
    /// Response binding the (secret) plaintext's commitment with
    /// [`PiLogProof::mask_commit`] (`z3` in the paper).
    plaintext_commit_response: MaskedRandomness,
}

/// Common input and setup parameters known to both the prover and the verifier.
#[derive(Serialize)]
pub(crate) struct CommonInput {
    /// Claimed ciphertext of the (secret) [plaintext](ProverSecret::plaintext)
    /// (`C` in the paper).
    ciphertext: Ciphertext,
    /// Claimed discrete log commitment of the (secret)
    /// [plaintext](ProverSecret::plaintext) (`X` in the paper).
    dlog_commit: CurvePoint,
    /// Ring-Pedersen commitment scheme (`(Nhat, s, t)` in the paper).
    ring_pedersen: RingPedersen,
    /// Paillier public key (`N_0` in the paper).
    prover_encryption_key: EncryptionKey,
    // Group generator for discrete log commitments (`g` in the paper).
    generator: CurvePoint,
}

impl CommonInput {
    /// Collect common parameters for proving or verifying a [`PiLogProof`]
    /// about `ciphertext` and `dlog_commit`.
    ///
    /// The last three arguments are shared setup information:
    /// 1. `verifier_ring_pedersen` is a [`RingPedersen`] generated
    /// by the verifier.
    /// 2. `prover_encryption_key` is a [`EncryptionKey`] generated by the
    /// prover.
    /// 3. `generator` is a group generator.
    pub(crate) fn new(
        ciphertext: Ciphertext,
        dlog_commit: CurvePoint,
        verifier_ring_pedersen: RingPedersen,
        prover_encryption_key: EncryptionKey,
        generator: CurvePoint,
    ) -> Self {
        Self {
            ciphertext,
            dlog_commit,
            ring_pedersen: verifier_ring_pedersen,
            prover_encryption_key,
            generator,
        }
    }
}

/// The prover's secret knowledge.
#[derive(ZeroizeOnDrop)]
pub(crate) struct ProverSecret {
    /// The secret plaintext (`x` in the paper).
    plaintext: BigNumber,
    /// The corresponding secret nonce (`ρ` in the paper).
    nonce: Nonce,
}

impl Debug for ProverSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("pilog::ProverSecret")
            .field("plaintext", &"[redacted]")
            .field("nonce", &"[redacted]")
            .finish()
    }
}

impl ProverSecret {
    /// Collect prover secrets for proving [`PiLogProof`].
    pub(crate) fn new(plaintext: BigNumber, nonce: Nonce) -> Self {
        Self { plaintext, nonce }
    }
}

/// Generates a challenge from a [`Transcript`] and the values generated in the
/// proof.
fn generate_challenge(
    transcript: &mut Transcript,
    common_input: &CommonInput,
    plaintext_commit: &Commitment,
    mask_encryption: &Ciphertext,
    mask_dlog_commit: &CurvePoint,
    mask_commit: &Commitment,
) -> Result<BigNumber> {
    transcript.append_message(b"Common input", &serialize!(&common_input)?);
    transcript.append_message(
        b"(plaintext commit, mask encryption, mask dlog commit, mask commit)",
        &[
            plaintext_commit.to_bytes(),
            mask_encryption.to_bytes(),
            serialize!(&mask_dlog_commit)?,
            mask_commit.to_bytes(),
        ]
        .concat(),
    );

    // The challenge is sampled from `± q` (where `q` is the group order).
    let challenge = plusminus_bn_random_from_transcript(transcript, &utils::k256_order());
    Ok(challenge)
}

impl Proof for PiLogProof {
    type CommonInput = CommonInput;
    type ProverSecret = ProverSecret;

    #[cfg_attr(feature = "flame_it", flame("PiLogProof"))]
    fn prove<R: RngCore + CryptoRng>(
        input: &Self::CommonInput,
        secret: &Self::ProverSecret,
        transcript: &mut Transcript,
        rng: &mut R,
    ) -> Result<Self> {
        // The proof works as follows.
        //
        // Recall that the prover wants to prove that some public Paillier ciphertext
        // `C` and group element `X` correctly encrypts / commits a secret
        // plaintext value `x`.
        //
        // The prover begins by generating a "mask" value (denoted by `ɑ` in the paper),
        // and then commits / encrypts it in three ways:
        //
        // 1. A Paillier encryption (`A` in the paper).
        // 2. A discrete log commitment (`Y` in the paper).
        // 3. A ring-Pedersen commitment (`D` in the paper).
        //
        // In addition, the prover provides a ring-Pedersen commitment (`S` in the
        // paper) of `x`.
        //
        // The proof utilizes the homomorphic properties of these commitments /
        // encryptions to perform a bunch of checks. All of these checks utilize
        // a challenge value (`e` in the paper) produced by Fiat-Shamir-ing the
        // above commitments / encryptions.
        //
        // 1. We first check that the Paillier encryption of `ɑ + ex` equals `A C^e`;
        // this enforces that the Paillier encryption of `x` and `ɑ` "check
        // out". Note that here we need to homomorphically manipulate the
        // Paillier nonces to make sure they line up as well.
        //
        // 2. We next check that the group exponentiation of `ɑ + ex` equals `Y X^e`;
        // this enforces that the group exponentiation of `x` and `ɑ` "check
        // out".
        //
        // 3. We next check that the ring-Pedersen commitments are consistent by
        // checking that the ring-Pedersen commitment of `ɑ + ex` equals `D S^e`.
        // As in Step 1, we need to homomorphically maniuplate the commitment randomness
        // to make sure they line up. This check is needed as detailed in the "Vanilla
        // ZK Range-Proof" section of the paper (Page 13).
        //
        // 4. The last check is a range check on `ɑ + ex`. If this falls within `± 2^{ℓ
        // + ε}` then this guarantees that `x` falls within this range too.

        // Sample a random plaintext mask from `± 2^{ELL + EPSILON}` (`ɑ` in the paper).
        let mask = random_plusminus_by_size(rng, ELL + EPSILON);
        // Commit to the secret plaintext using ring-Pedersen (producing variables `S`
        // and `μ` in the paper).
        let (plaintext_commit, plaintext_commit_randomness) =
            input.ring_pedersen.commit(&secret.plaintext, ELL, rng);
        // Encrypt the random plaintext using Paillier (producing variables `A` and `r`
        // in the paper).
        let (mask_ciphertext, mask_nonce) = input.prover_encryption_key.encrypt(rng, &mask)?;
        // Commit to the random plaintext using discrete log (`Y` in the paper).
        let mask_dlog_commit = input.generator.multiply_by_scalar(&mask)?;
        // Commit to the random plaintext using ring-Pedersen (producing variables `D`
        // and `ɣ` in the paper).
        let (mask_commit, mask_commit_randomness) =
            input.ring_pedersen.commit(&mask, ELL + EPSILON, rng);
        // Generate verifier's challenge via Fiat-Shamir (`e` in the paper).
        let challenge = generate_challenge(
            transcript,
            input,
            &plaintext_commit,
            &mask_ciphertext,
            &mask_dlog_commit,
            &mask_commit,
        )?;
        // Mask the secret plaintext (`z1` in the paper).
        let plaintext_response = &mask + &challenge * &secret.plaintext;
        // Mask the secret nonce (`z2` in the paper).
        let nonce_response =
            input
                .prover_encryption_key
                .mask(&secret.nonce, &mask_nonce, &challenge);
        // Mask the secret plaintext's commitment randomness (`z3` in the paper).
        let plaintext_commit_response =
            plaintext_commit_randomness.mask(&mask_commit_randomness, &challenge);

        Ok(Self {
            plaintext_commit,
            mask_ciphertext,
            mask_dlog_commit,
            mask_commit,
            challenge,
            plaintext_response,
            nonce_response,
            plaintext_commit_response,
        })
    }

    #[cfg_attr(feature = "flame_it", flame("PiLogProof"))]
    fn verify(&self, input: &Self::CommonInput, transcript: &mut Transcript) -> Result<()> {
        // See the comment in `prove` for a high-level description of how the protocol
        // works.

        // Generate verifier's challenge via Fiat-Shamir...
        let challenge = generate_challenge(
            transcript,
            input,
            &self.plaintext_commit,
            &self.mask_ciphertext,
            &self.mask_dlog_commit,
            &self.mask_commit,
        )?;
        // ... and check that it's the correct challenge.
        if challenge != self.challenge {
            warn!("Fiat-Shamir consistency check failed");
            return Err(InternalError::FailedToVerifyProof);
        }

        // Check that the Paillier encryption of the secret plaintext is valid.
        let paillier_encryption_is_valid = {
            let lhs = input
                .prover_encryption_key
                .encrypt_with_nonce(&self.plaintext_response, &self.nonce_response)?;
            let rhs = input.prover_encryption_key.multiply_and_add(
                &self.challenge,
                &input.ciphertext,
                &self.mask_ciphertext,
            )?;
            lhs == rhs
        };
        if !paillier_encryption_is_valid {
            warn!("paillier encryption check (first equality check) failed");
            return Err(InternalError::FailedToVerifyProof);
        }
        // Check that the group exponentiation of the secret plaintext is valid.
        let group_exponentiation_is_valid = {
            let lhs = input
                .generator
                .multiply_by_scalar(&self.plaintext_response)?;
            let rhs =
                self.mask_dlog_commit + input.dlog_commit.multiply_by_scalar(&self.challenge)?;
            lhs == rhs
        };
        if !group_exponentiation_is_valid {
            warn!("group exponentiation check (second equality check) failed");
            return Err(InternalError::FailedToVerifyProof);
        }

        // Check that the ring-Pedersen commitment of the secret plaintext is valid.
        let ring_pedersen_commitment_is_valid = {
            let lhs = input
                .ring_pedersen
                .reconstruct(&self.plaintext_response, &self.plaintext_commit_response);
            let rhs = input.ring_pedersen.combine(
                &self.mask_commit,
                &self.plaintext_commit,
                &self.challenge,
            );
            lhs == rhs
        };
        if !ring_pedersen_commitment_is_valid {
            warn!("ring Pedersen commitment check (third equality check) failed");
            return Err(InternalError::FailedToVerifyProof);
        }

        // Do a range check on the plaintext response, which validates that the
        // plaintext falls within the same range.
        if !within_bound_by_size(&self.plaintext_response, ELL + EPSILON) {
            warn!("plaintext range check failed");
            return Err(InternalError::FailedToVerifyProof);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        paillier::DecryptionKey,
        ring_pedersen::VerifiedRingPedersen,
        utils::{random_plusminus_by_size_with_minimum, testing::init_testing},
    };

    fn random_paillier_log_proof<R: RngCore + CryptoRng>(rng: &mut R, x: &BigNumber) -> Result<()> {
        let (decryption_key, _, _) = DecryptionKey::new(rng)?;
        let pk = decryption_key.encryption_key();

        let g = CurvePoint(k256::ProjectivePoint::GENERATOR);

        let X = CurvePoint(g.0 * utils::bn_to_scalar(x).unwrap());
        let (C, rho) = pk.encrypt(rng, x)?;

        let setup_params = VerifiedRingPedersen::gen(rng)?;

        let input = CommonInput::new(C, X, setup_params.scheme().clone(), pk, g);
        let mut transcript = Transcript::new(b"PiLogProof Test");
        let proof = PiLogProof::prove(
            &input,
            &ProverSecret::new(x.clone(), rho),
            &mut transcript,
            rng,
        )?;
        let mut transcript = Transcript::new(b"PiLogProof Test");
        proof.verify(&input, &mut transcript)
    }

    #[test]
    fn test_paillier_log_proof() -> Result<()> {
        let mut rng = init_testing();

        let x_small = random_plusminus_by_size(&mut rng, ELL);
        let x_large =
            random_plusminus_by_size_with_minimum(&mut rng, ELL + EPSILON + 1, ELL + EPSILON)?;

        // Sampling x in the range 2^ELL should always succeed
        random_paillier_log_proof(&mut rng, &x_small)?;

        // Sampling x in the range (2^{ELL + EPSILON}, 2^{ELL + EPSILON + 1}] should
        // fail
        assert!(random_paillier_log_proof(&mut rng, &x_large).is_err());

        Ok(())
    }
}

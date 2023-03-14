// Copyright (c) Facebook, Inc. and its affiliates.
// Modifications Copyright (c) 2022-2023 Bolt Labs Holdings, Inc
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree and the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree.

//! Implements the ZKP from Figure 16 of <https://eprint.iacr.org/2021/060.pdf>

use std::cmp::Ordering;

use super::Proof;
use crate::{errors::*, utils::*};
use libpaillier::unknown_order::BigNumber;
use merlin::Transcript;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

// Soundness parameter lambda
static LAMBDA: usize = crate::parameters::SOUNDNESS_PARAMETER;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PiModProof {
    w: BigNumber,
    // (x, a, b, z),
    elements: Vec<PiModProofElements>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PiModProofElements {
    x: BigNumber,
    a: usize,
    b: usize,
    z: BigNumber,
    y: BigNumber,
}

#[derive(Serialize)]
pub(crate) struct PiModInput {
    N: BigNumber,
}

impl PiModInput {
    pub(crate) fn new(N: &BigNumber) -> Self {
        Self { N: N.clone() }
    }
}

pub(crate) struct PiModSecret {
    p: BigNumber,
    q: BigNumber,
}

impl PiModSecret {
    pub(crate) fn new(p: &BigNumber, q: &BigNumber) -> Self {
        Self {
            p: p.clone(),
            q: q.clone(),
        }
    }
}

impl Proof for PiModProof {
    type CommonInput = PiModInput;
    type ProverSecret = PiModSecret;

    /// Generated by the prover, requires public input N and secrets (p,q)
    /// Prover generates a random w in Z_N of Jacobi symbol -1
    #[allow(clippy::many_single_char_names)]
    #[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
    fn prove<R: RngCore + CryptoRng>(
        input: &Self::CommonInput,
        secret: &Self::ProverSecret,
        transcript: &mut Transcript,
        rng: &mut R,
    ) -> Result<Self> {
        // Step 1: Pick a random w in [1, N) that has a Jacobi symbol of -1
        let mut w = random_positive_bn(rng, &input.N);
        while jacobi(&w, &input.N) != -1 {
            w = random_positive_bn(rng, &input.N);
        }

        transcript.append_message(b"CommonInput", &serialize!(&input)?);
        transcript.append_message(b"w", &w.to_bytes());

        let mut elements = vec![];
        for _ in 0..LAMBDA {
            let y = positive_bn_random_from_transcript(transcript, &input.N);
            let (a, b, x) = y_prime_combinations(&w, &y, &secret.p, &secret.q)?;

            // Compute phi(N) = (p-1) * (q-1)
            let phi_n = (&secret.p - 1) * (&secret.q - 1);
            let exp = input.N.invert(&phi_n).ok_or_else(|| {
                error!("Could not invert a BigNumber");
                InternalError::CouldNotGenerateProof
            })?;
            let z = modpow(&y, &exp, &input.N);

            elements.push(PiModProofElements {
                x: x[0].clone(),
                a,
                b,
                z,
                y,
            });
        }

        let proof = Self { w, elements };

        Ok(proof)
    }

    #[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
    fn verify(&self, input: &Self::CommonInput, transcript: &mut Transcript) -> Result<()> {
        // Verify that proof is sound -- it must have exactly LAMBDA elements
        match self.elements.len().cmp(&LAMBDA) {
            Ordering::Less => {
                warn!(
                    "PiMod proof is not sound: has {} elements, expected {}",
                    self.elements.len(),
                    LAMBDA,
                );
                return Err(InternalError::FailedToVerifyProof);
            }
            Ordering::Greater => {
                warn!(
                    "PiMod proof has too many elements: has {}, expected {}",
                    self.elements.len(),
                    LAMBDA
                );
                return Err(InternalError::FailedToVerifyProof);
            }
            Ordering::Equal => {}
        }

        // Verify that N is an odd composite number
        if &input.N % BigNumber::from(2u64) == BigNumber::zero() {
            warn!("N is even");
            return Err(InternalError::FailedToVerifyProof);
        }

        if input.N.is_prime() {
            warn!("N is not composite");
            return Err(InternalError::FailedToVerifyProof);
        }

        transcript.append_message(b"CommonInput", &serialize!(&input)?);
        transcript.append_message(b"w", &self.w.to_bytes());

        for elements in &self.elements {
            // First, check that y came from Fiat-Shamir transcript
            let y = positive_bn_random_from_transcript(transcript, &input.N);
            if y != elements.y {
                warn!("y does not match Fiat-Shamir challenge");
                return Err(InternalError::FailedToVerifyProof);
            }

            let y_candidate = modpow(&elements.z, &input.N, &input.N);
            if elements.y != y_candidate {
                warn!("z^N != y (mod N)");
                return Err(InternalError::FailedToVerifyProof);
            }

            if elements.a != 0 && elements.a != 1 {
                warn!("a not in {{0,1}}");
                return Err(InternalError::FailedToVerifyProof);
            }

            if elements.b != 0 && elements.b != 1 {
                warn!("b not in {{0,1}}");
                return Err(InternalError::FailedToVerifyProof);
            }

            let y_prime = y_prime_from_y(&elements.y, &self.w, elements.a, elements.b, &input.N);
            if modpow(&elements.x, &BigNumber::from(4u64), &input.N) != y_prime {
                warn!("x^4 != y' (mod N)");
                return Err(InternalError::FailedToVerifyProof);
            }
        }

        Ok(())
    }
}

// Compute regular mod
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn bn_mod(n: &BigNumber, p: &BigNumber) -> BigNumber {
    n.modadd(&BigNumber::zero(), p)
}

// Denominator needs to be positive and odd
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn jacobi(numerator: &BigNumber, denominator: &BigNumber) -> isize {
    let mut n = bn_mod(numerator, denominator);
    let mut k = denominator.clone();
    let mut t = 1;

    while n != BigNumber::zero() {
        while bn_mod(&n, &BigNumber::from(2)) == BigNumber::zero() {
            n /= 2;
            let r = bn_mod(&k, &BigNumber::from(8));
            if r == BigNumber::from(3) || r == BigNumber::from(5) {
                t *= -1;
            }
        }

        // (n, k) = (k, n), swap them
        std::mem::swap(&mut n, &mut k);

        if bn_mod(&n, &BigNumber::from(4)) == BigNumber::from(3)
            && bn_mod(&k, &BigNumber::from(4)) == BigNumber::from(3)
        {
            t *= -1;
        }
        n = bn_mod(&n, &k);
    }

    if k == BigNumber::one() {
        return t;
    }

    0
}

/// Finds the two x's such that x^2 = n (mod p), where p is a prime that is 3
/// (mod 4)
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn square_roots_mod_prime(n: &BigNumber, p: &BigNumber) -> Result<(BigNumber, BigNumber)> {
    // Compute r = +- n^{p+1/4} (mod p)
    let r = modpow(n, &(&(p + 1) / 4), p);
    let neg_r = r.modneg(p);

    // Check that r and neg_r are such that r^2 = n (mod p) -- if not, then
    // there are no solutions

    if modpow(&r, &BigNumber::from(2), p) == bn_mod(n, p) {
        return Ok((r, neg_r));
    }
    warn!("Could not find square roots modulo n");
    Err(InternalError::CouldNotGenerateProof)
}

// Finds an (x,y) such that ax + by = 1, or returns error if gcd(a,b) != 1
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn extended_euclidean(a: &BigNumber, b: &BigNumber) -> Result<(BigNumber, BigNumber)> {
    let result = a.extended_gcd(b);

    if result.gcd != BigNumber::one() {
        warn!("Elements are not coprime");
        Err(InternalError::CouldNotGenerateProof)?
    }

    Ok((result.x, result.y))
}

/// Compute the Chinese remainder theorem with two congruences.
///
/// That is, find the unique `x` such that:
/// - `x = a1 (mod p)`, and
/// - `x = a2 (mod q)`.
///
/// This returns an error if:
/// - `p` and `q` aren't co-prime;
/// - `a1` is not in the range `[0, p)`;
/// - `a2` is not in the range `[0, q)`.
#[allow(clippy::many_single_char_names)]
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn chinese_remainder_theorem(
    a1: &BigNumber,
    a2: &BigNumber,
    p: &BigNumber,
    q: &BigNumber,
) -> Result<BigNumber> {
    let zero = &BigNumber::zero();
    if a1 >= p || a1 < zero || a2 >= q || a2 < zero {
        warn!("One or more of the integer inputs to the Chinese remainder theorem were outside the expected range");
        Err(InternalError::CouldNotGenerateProof)?
    }
    let (z, w) = extended_euclidean(p, q)?;
    let x = a1 * w * q + a2 * z * p;
    Ok(bn_mod(&x, &(p * q)))
}

/// Finds the four x's such that x^2 = n (mod pq), where p,q are primes that are
/// 3 (mod 4)
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn square_roots_mod_composite(
    n: &BigNumber,
    p: &BigNumber,
    q: &BigNumber,
) -> Result<[BigNumber; 4]> {
    let (y1, y2) = square_roots_mod_prime(n, p)?;
    let (z1, z2) = square_roots_mod_prime(n, q)?;

    let x1 = chinese_remainder_theorem(&y1, &z1, p, q)?;
    let x2 = chinese_remainder_theorem(&y1, &z2, p, q)?;
    let x3 = chinese_remainder_theorem(&y2, &z1, p, q)?;
    let x4 = chinese_remainder_theorem(&y2, &z2, p, q)?;

    Ok([x1, x2, x3, x4])
}

#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn fourth_roots_mod_composite(
    n: &BigNumber,
    p: &BigNumber,
    q: &BigNumber,
) -> Result<Vec<BigNumber>> {
    let mut fourth_roots = vec![];

    let xs = square_roots_mod_composite(n, p, q)?;
    for x in xs {
        match square_roots_mod_composite(&x, p, q) {
            Ok(res) => {
                for y in res {
                    fourth_roots.push(y);
                }
            }
            Err(_) => {
                continue;
            }
        }
    }
    Ok(fourth_roots)
}

/// Compute y' = (-1)^a * w^b * y (mod N)
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn y_prime_from_y(y: &BigNumber, w: &BigNumber, a: usize, b: usize, N: &BigNumber) -> BigNumber {
    let mut y_prime = y.clone();

    if b == 1 {
        y_prime = y_prime.modmul(w, N);
    }

    if a == 1 {
        y_prime = y_prime.modneg(N);
    }

    y_prime
}

/// Finds unique a,b in {0,1} such that, for y' = (-1)^a * w^b * y, there is an
/// x such that x^4 = y (mod pq)
#[cfg_attr(feature = "flame_it", flame("PaillierBlumModulusProof"))]
fn y_prime_combinations(
    w: &BigNumber,
    y: &BigNumber,
    p: &BigNumber,
    q: &BigNumber,
) -> Result<(usize, usize, Vec<BigNumber>)> {
    let N = p * q;

    let mut ret = vec![];

    let mut has_fourth_roots = 0;
    let mut success_a = 0;
    let mut success_b = 0;

    for a in 0..2 {
        for b in 0..2 {
            let y_prime = y_prime_from_y(y, w, a, b, &N);
            match fourth_roots_mod_composite(&y_prime, p, q) {
                Ok(values) => {
                    has_fourth_roots += 1;
                    success_a = a;
                    success_b = b;
                    ret.extend_from_slice(&values);
                }
                Err(_) => {
                    continue;
                }
            }
        }
    }

    if has_fourth_roots != 1 {
        error!(
            "Could not find uniqueness for fourth roots combination in Paillier-Blum modulus proof"
        );
        return Err(InternalError::CouldNotGenerateProof);
    }

    Ok((success_a, success_b, ret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        paillier::{prime_gen, DecryptionKey},
        parameters::SOUNDNESS_PARAMETER,
        utils::testing::init_testing,
    };

    #[test]
    fn test_jacobi() {
        let mut rng = init_testing();
        let (p, q) = prime_gen::get_prime_pair_from_pool_insecure(&mut rng).unwrap();
        let N = &p * &q;

        for _ in 0..100 {
            let a = BigNumber::from_rng(&N, &mut rng);

            let a_p = jacobi(&a, &p);
            let a_q = jacobi(&a, &q);

            // Verify that a^{p-1/2} == a_p (mod p)
            assert_eq!(
                bn_mod(&BigNumber::from(a_p), &p),
                modpow(&a, &(&(&p - 1) / 2), &p)
            );

            // Verify that a^{q-1/2} == a_q (mod q)
            assert_eq!(
                bn_mod(&BigNumber::from(a_q), &q),
                modpow(&a, &(&(&q - 1) / 2), &q)
            );

            // Verify that (a/n) = (a/p) * (a/q)
            let a_n = jacobi(&a, &N);
            assert_eq!(a_n, a_p * a_q);
        }
    }

    #[test]
    fn test_square_roots_mod_prime() {
        let mut rng = init_testing();
        let p = prime_gen::try_get_prime_from_pool_insecure(&mut rng).unwrap();

        for _ in 0..100 {
            let a = BigNumber::from_rng(&p, &mut rng);
            let a_p = jacobi(&a, &p);

            let roots = square_roots_mod_prime(&a, &p);
            match roots {
                Ok((r1, r2)) => {
                    assert_eq!(a_p, 1);
                    assert_eq!(modpow(&r1, &BigNumber::from(2), &p), a);
                    assert_eq!(modpow(&r2, &BigNumber::from(2), &p), a);
                }
                Err(InternalError::CouldNotGenerateProof) => {
                    assert_ne!(a_p, 1);
                }
                Err(_) => {
                    panic!("Should not reach here");
                }
            }
        }
    }

    #[test]
    fn test_square_roots_mod_composite() {
        let mut rng = init_testing();
        let (p, q) = prime_gen::get_prime_pair_from_pool_insecure(&mut rng).unwrap();
        let N = &p * &q;

        // Loop until we've confirmed enough successes
        let mut success = 0;
        loop {
            if success == 10 {
                return;
            }
            let a = BigNumber::from_rng(&N, &mut rng);
            let a_n = jacobi(&a, &N);

            let roots = square_roots_mod_composite(&a, &p, &q);
            match roots {
                Ok(xs) => {
                    assert_eq!(a_n, 1);
                    for x in xs {
                        assert_eq!(modpow(&x, &BigNumber::from(2), &N), a);
                    }
                    success += 1;
                }
                Err(_) => {
                    continue;
                }
            }
        }
    }

    #[test]
    fn test_fourth_roots_mod_composite() {
        let mut rng = init_testing();
        let (p, q) = prime_gen::get_prime_pair_from_pool_insecure(&mut rng).unwrap();
        let N = &p * &q;

        // Loop until we've confirmed enough successes
        let mut success = 0;
        loop {
            if success == 10 {
                return;
            }
            let a = BigNumber::from_rng(&N, &mut rng);
            let a_n = jacobi(&a, &N);

            let roots = fourth_roots_mod_composite(&a, &p, &q);
            match roots {
                Ok(xs) => {
                    assert_eq!(a_n, 1);
                    for x in xs {
                        assert_eq!(modpow(&x, &BigNumber::from(4), &N), a);
                    }
                    success += 1;
                }
                Err(_) => {
                    continue;
                }
            }
        }
    }

    #[test]
    fn chinese_remainder_theorem_works() {
        let mut rng = init_testing();
        // This guarantees p and q are coprime and not equal.
        let (p, q) = prime_gen::get_prime_pair_from_pool_insecure(&mut rng).unwrap();
        assert!(p != q);

        for _ in 0..100 {
            // This method guarantees a1 and a2 are smaller than their moduli.
            let a1 = BigNumber::from_rng(&p, &mut rng);
            let a2 = BigNumber::from_rng(&q, &mut rng);

            let x = chinese_remainder_theorem(&a1, &a2, &p, &q).unwrap();

            assert_eq!(bn_mod(&x, &p), a1);
            assert_eq!(bn_mod(&x, &q), a2);
            assert!(x < &p * &q);
        }
    }

    #[test]
    fn chinese_remainder_theorem_integers_must_be_in_range() {
        let mut rng = init_testing();

        // This guarantees p and q are coprime and not equal.
        let (p, q) = prime_gen::get_prime_pair_from_pool_insecure(&mut rng).unwrap();
        assert!(p != q);

        // a1 = p
        let a1 = &p;
        let a2 = BigNumber::from_rng(&q, &mut rng);
        assert_eq!(
            chinese_remainder_theorem(a1, &a2, &p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // a1 > p
        let a1 = a1 + BigNumber::one();
        assert_eq!(
            chinese_remainder_theorem(&a1, &a2, &p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // a1 < 0
        let a1 = -BigNumber::from_rng(&p, &mut rng);
        assert_eq!(
            chinese_remainder_theorem(&a1, &a2, &p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // a2 = q
        let a1 = BigNumber::from_rng(&p, &mut rng);
        let a2 = &q;
        assert_eq!(
            chinese_remainder_theorem(&a1, a2, &p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // a2 > q
        let a2 = a2 + BigNumber::one();
        assert_eq!(
            chinese_remainder_theorem(&a1, &a2, &p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // a2 < 0
        let a2 = -BigNumber::from_rng(&q, &mut rng);
        assert_eq!(
            chinese_remainder_theorem(&a1, &a2, &p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );
    }

    #[test]
    fn chinese_remainder_theorem_moduli_must_be_coprime() {
        let mut rng = init_testing();

        // This guarantees p and q are coprime and not equal.
        let (p, q) = prime_gen::get_prime_pair_from_pool_insecure(&mut rng).unwrap();
        assert!(p != q);

        // choose small a1, a2 so that they work for all our tests
        let smaller_prime = if p < q { &p } else { &q };
        let a1 = BigNumber::from_rng(smaller_prime, &mut rng);
        let a2 = BigNumber::from_rng(smaller_prime, &mut rng);

        // p = q
        let bad_q = &p;
        assert_eq!(
            chinese_remainder_theorem(&a1, &a1, &p, bad_q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // p = kq for some k
        let mult_p = &q + &q;
        assert_eq!(
            chinese_remainder_theorem(&a1, &a2, &mult_p, &q),
            Err(InternalError::CouldNotGenerateProof)
        );

        // q = kp for some k
        let mult_q = &p + &p;
        assert_eq!(
            chinese_remainder_theorem(&a1, &a2, &p, &mult_q),
            Err(InternalError::CouldNotGenerateProof)
        );

        assert!(chinese_remainder_theorem(&a1, &a2, &p, &q).is_ok());
    }

    fn random_big_number<R: RngCore + CryptoRng>(rng: &mut R) -> BigNumber {
        let x_len = rng.next_u64() as u16;
        let mut buf_x = (0..x_len).map(|_| 0u8).collect::<Vec<u8>>();
        rng.fill_bytes(&mut buf_x);
        BigNumber::from_slice(buf_x.as_slice())
    }

    fn random_pbmpe<R: RngCore + CryptoRng>(rng: &mut R) -> PiModProofElements {
        let x = random_big_number(rng);
        let y = random_big_number(rng);
        let z = random_big_number(rng);

        let a = rng.next_u64() as u16;
        let b = rng.next_u64() as u16;

        PiModProofElements {
            x,
            a: a as usize,
            b: b as usize,
            y,
            z,
        }
    }

    #[test]
    fn test_blum_modulus_proof_elements_roundtrip() {
        let mut rng = init_testing();
        let pbelement = random_pbmpe(&mut rng);
        let buf = bincode::serialize(&pbelement).unwrap();
        let roundtrip_pbelement: PiModProofElements = bincode::deserialize(&buf).unwrap();
        assert_eq!(buf, bincode::serialize(&roundtrip_pbelement).unwrap());
    }

    #[test]
    fn test_blum_modulus_roundtrip() {
        let mut rng = init_testing();

        let w = random_big_number(&mut rng);
        let num_elements = rng.next_u64() as u8;
        let elements = (0..num_elements)
            .map(|_| random_pbmpe(&mut rng))
            .collect::<Vec<PiModProofElements>>();

        let pbmp = PiModProof { w, elements };
        let buf = bincode::serialize(&pbmp).unwrap();
        let roundtrip_pbmp: PiModProof = bincode::deserialize(&buf).unwrap();
        assert_eq!(buf, bincode::serialize(&roundtrip_pbmp).unwrap());
    }

    fn random_pimod_proof<R: CryptoRng + RngCore>(rng: &mut R) -> (PiModProof, PiModInput) {
        let (decryption_key, p, q) = DecryptionKey::new(rng).unwrap();

        let input = PiModInput {
            N: decryption_key.encryption_key().modulus().to_owned(),
        };
        let secret = PiModSecret { p, q };
        let mut transcript = Transcript::new(b"PiMod Test");
        let proof_result = PiModProof::prove(&input, &secret, &mut transcript, rng);
        assert!(proof_result.is_ok());
        (proof_result.unwrap(), input)
    }

    #[test]
    fn pimod_proof_verifies() {
        let mut rng = init_testing();
        let (proof, input) = random_pimod_proof(&mut rng);
        let mut transcript = Transcript::new(b"PiMod Test");
        assert!(proof.verify(&input, &mut transcript).is_ok());
    }

    #[test]
    fn pimod_proof_requires_correct_number_of_elements_for_soundness() {
        let mut rng = init_testing();

        let transform = |proof: &PiModProof| {
            // Remove iterations from the proof
            let short_proof = PiModProof {
                w: proof.w.clone(),
                elements: proof.elements[..SOUNDNESS_PARAMETER - 1].into(),
            };

            // Add elements to the proof. Not sure if this is actually a problem, but we'll
            // stick to the spec for now.
            let long_proof = PiModProof {
                w: proof.w.clone(),
                elements: proof
                    .elements
                    .clone()
                    .into_iter()
                    .cycle()
                    .take(SOUNDNESS_PARAMETER * 2)
                    .collect(),
            };

            (short_proof, long_proof)
        };

        // Make un-sound a proof generated with the standard API
        let (proof, input) = random_pimod_proof(&mut rng);
        let (short_proof, long_proof) = transform(&proof);
        let mut transcript = Transcript::new(b"PiMod Test");
        assert!(short_proof.verify(&input, &mut transcript).is_err());
        let mut transcript = Transcript::new(b"PiMod Test");
        assert!(long_proof.verify(&input, &mut transcript).is_err());
        let mut transcript = Transcript::new(b"PiMod Test");
        assert!(proof.verify(&input, &mut transcript).is_ok());
    }
}

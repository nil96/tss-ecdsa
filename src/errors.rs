// Copyright (c) Facebook, Inc. and its affiliates.
// Modifications Copyright (c) 2022 Bolt Labs Holdings, Inc
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree and the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree.

//! A list of error types which are produced during an execution of the protocol
use core::fmt::Debug;
use displaydoc::Display;
use thiserror::Error;

/// The default Result type used in this crate
pub type Result<T> = std::result::Result<T, InternalError>;

/// Represents an error in the manipulation of internal cryptographic data
#[derive(Clone, Display, Eq, Hash, PartialEq, Error, Debug)]
pub enum InternalError {
    /// Could not find square roots modulo n
    NoSquareRoots,
    /// Elements are not coprime
    NotCoprime,
    /**Could not find uniqueness for fourth roots combination in Paillier-Blum
    modulus proof*/
    NonUniqueFourthRootsCombination,
    /// Could not invert a BigNumber
    CouldNotInvertBigNumber,
    /// Serialization Error
    Serialization,
    /// Could not successfully generate proof
    CouldNotGenerateProof,
    /// Failed to verify proof: `{0}`
    FailedToVerifyProof(String),
    /// `{0}`
    BailError(String),
    /// Represents some code assumption that was checked at runtime but failed to be true.
    InternalInvariantFailed,
    /// The inputs to a homomorphic operation on a paillier ciphertext were malformed
    InvalidPaillierOperation,
    /// The attemped decryption of a pailler ciphertext failed
    PaillierDecryptionFailed,
    /// Failed to convert BigNumber to k256::Scalar, as BigNumber was not in [0,p)
    CouldNotConvertToScalar,
    /// Could not invert a Scaler
    CouldNotInvertScalar,
}

macro_rules! serialize {
    ($x:expr) => {{
        bincode::serialize($x).or(Err(crate::errors::InternalError::Serialization))
    }};
}

macro_rules! deserialize {
    ($x:expr) => {{
        bincode::deserialize($x).or(Err(crate::errors::InternalError::Serialization))
    }};
}

macro_rules! verify_err {
    ($x:expr) => {{
        Err(crate::errors::InternalError::FailedToVerifyProof(
            String::from($x),
        ))
    }};
}

macro_rules! bail {
    ($msg:literal $(,)?) => {
        Err(bail_context!($msg))
    };
    ($fmt:expr, $($arg:tt)*) => {
        Err(bail_context!($fmt, $($arg)*))
    };
}

macro_rules! bail_context {
    ($msg:literal $(,)?) => {
        crate::errors::InternalError::BailError(String::from($msg))
    };
    ($fmt:expr, $($arg:tt)*) => {
        crate::errors::InternalError::BailError(format!($fmt, $($arg)*))
    };
}

//! Error module for the bus-mapping crate

use core::fmt::{Display, Formatter, Result as FmtResult};
use eth_types::{evm_types::OpcodeId, Address, GethExecError, GethExecStep, Word, H256};
use ethers_providers::ProviderError;
use std::error::Error as StdError;

/// Error type for any BusMapping related failure.
#[derive(Debug)]
pub enum Error {
    /// Serde de/serialization error.
    SerdeError(serde_json::error::Error),
    /// Parsing error
    IoError(std::io::Error),
    /// hex parsing error
    HexError(hex::FromHexError),
    /// JSON-RPC related error.
    JSONRpcError(ProviderError),
    /// OpcodeId is not a call type.
    OpcodeIdNotCallType,
    /// Account not found in the StateDB
    AccountNotFound(Address),
    /// Storage key not found in the StateDB
    StorageKeyNotFound(Address, Word),
    /// Address not found in the CodeDB,
    AddressNotFound(Address),
    /// Code not found in the CodeDB
    CodeNotFound(H256),
    /// Unable to figure out error at a [`GethExecStep`]
    UnexpectedExecStepError(&'static str, Box<GethExecStep>),
    /// Invalid [`eth_types::GethExecTrace`] due to an invalid/unexpected value
    /// in it.
    InvalidGethExecTrace(&'static str),
    /// Invalid [`GethExecStep`] due to an invalid/unexpected value in it.
    InvalidGethExecStep(&'static str, Box<GethExecStep>),
    /// Eth type related error.
    EthTypeError(eth_types::Error),
    /// EVM Execution error
    ExecutionError(ExecError),
    /// Internal Code error
    InternalError(&'static str),
}

impl From<eth_types::Error> for Error {
    fn from(err: eth_types::Error) -> Self {
        Error::EthTypeError(err)
    }
}

impl From<ProviderError> for Error {
    fn from(err: ProviderError) -> Self {
        Error::JSONRpcError(err)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{self:?}")
    }
}

impl StdError for Error {}

/// Out of Gas errors by opcode
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OogError {
    // Variants remain unchanged
}

/// Contract address collision errors by opcode/state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractAddressCollisionError {
    // Variants remain unchanged
}

/// Depth above limit errors by opcode/state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepthError {
    // Variants remain unchanged
}

/// Insufficient balance errors by opcode/state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsufficientBalanceError {
    // Variants remain unchanged
}

/// Nonce uint overflow errors by opcode/state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NonceUintOverflowError {
    // Variants remain unchanged
}

/// EVM Execution Error
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    // Variants remain unchanged
}

impl Error {
    // Helper function to convert GethExecError to ExecError
    pub(crate) fn from_geth_exec_error(op: &OpcodeId, error: GethExecError) -> Self {
        match error {
            GethExecError::OutOfGas | GethExecError::GasUintOverflow => {
                let oog_err = match op {
                    OpcodeId::MLOAD | OpcodeId::MSTORE | OpcodeId::MSTORE8 => {
                        OogError::StaticMemoryExpansion
                    }
                    OpcodeId::RETURN | OpcodeId::REVERT => OogError::DynamicMemoryExpansion,
                    // Other cases remain unchanged
                    _ => OogError::Constant,
                };
                Error::ExecutionError(ExecError::OutOfGas(oog_err))
            }
            GethExecError::StackOverflow { .. } => ExecError::StackOverflow.into(),
            GethExecError::StackUnderflow { .. } => ExecError::StackUnderflow.into(),
            GethExecError::WriteProtection => ExecError::WriteProtection.into(),
            _ => panic!("Unknown GethExecStep.error: {error}"),
        }
    }
}

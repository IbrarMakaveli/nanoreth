//! Beacon consensus implementation.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use alloy_consensus::EMPTY_OMMER_ROOT_HASH;
use alloy_eips::merge::ALLOWED_FUTURE_BLOCK_TIME_SECONDS;
use alloy_primitives::U256;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator};
use reth_consensus_common::validation::{
    validate_4844_header_standalone, validate_against_parent_4844,
    validate_against_parent_eip1559_base_fee, validate_against_parent_hash_number,
    validate_against_parent_timestamp, validate_block_pre_execution, validate_body_against_header,
    validate_header_base_fee, validate_header_extra_data, validate_header_gas,
};
use reth_execution_types::BlockExecutionResult;
use reth_primitives::{NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader};
use reth_primitives_traits::{constants::MINIMUM_GAS_LIMIT, Block, BlockHeader};
use std::{fmt::Debug, sync::Arc, time::SystemTime};

mod validation;
pub use validation::validate_block_post_execution;

/// Ethereum beacon consensus
///
/// This consensus engine does basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct EthBeaconConsensus<ChainSpec> {
    /// Configuration
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec: EthChainSpec + EthereumHardforks> EthBeaconConsensus<ChainSpec> {
    /// Create a new instance of [`EthBeaconConsensus`]
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Checks the gas limit for consistency between parent and self headers.
    ///
    /// The maximum allowable difference between self and parent gas limits is determined by the
    /// parent's gas limit divided by the [`GAS_LIMIT_BOUND_DIVISOR`].
    fn validate_against_parent_gas_limit<H: BlockHeader>(
        &self,
        header: &SealedHeader<H>,
        _parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        // Check if the self gas limit is below the minimum required limit.
        if header.gas_limit() < MINIMUM_GAS_LIMIT {
            return Err(ConsensusError::GasLimitInvalidMinimum {
                child_gas_limit: header.gas_limit(),
            });
        }

        Ok(())
    }
}

impl<ChainSpec, N> FullConsensus<N> for EthBeaconConsensus<ChainSpec>
where
    ChainSpec: Send + Sync + EthChainSpec + EthereumHardforks + Debug,
    N: NodePrimitives,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
    ) -> Result<(), ConsensusError> {
        validate_block_post_execution(block, &self.chain_spec, &result.receipts, &result.requests)
    }
}

impl<B, ChainSpec: Send + Sync + EthChainSpec + EthereumHardforks + Debug> Consensus<B>
    for EthBeaconConsensus<ChainSpec>
where
    B: Block,
{
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), Self::Error> {
        validate_body_against_header(body, header.header())
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), Self::Error> {
        validate_block_pre_execution(block, &self.chain_spec)
    }
}

impl<H, ChainSpec: Send + Sync + EthChainSpec + EthereumHardforks + Debug> HeaderValidator<H>
    for EthBeaconConsensus<ChainSpec>
where
    H: BlockHeader,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        validate_header_gas(header.header())?;
        validate_header_base_fee(header.header(), &self.chain_spec)?;

        // EIP-4895: Beacon chain push withdrawals as operations
        if self.chain_spec.is_shanghai_active_at_timestamp(header.timestamp())
            && header.withdrawals_root().is_none()
        {
            return Err(ConsensusError::WithdrawalsRootMissing);
        } else if !self.chain_spec.is_shanghai_active_at_timestamp(header.timestamp())
            && header.withdrawals_root().is_some()
        {
            return Err(ConsensusError::WithdrawalsRootUnexpected);
        }

        // Ensures that EIP-4844 fields are valid once cancun is active.
        if self.chain_spec.is_cancun_active_at_timestamp(header.timestamp()) {
            validate_4844_header_standalone(header.header())?;
        } else if header.blob_gas_used().is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected);
        } else if header.excess_blob_gas().is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected);
        } else if header.parent_beacon_block_root().is_some() {
            return Err(ConsensusError::ParentBeaconBlockRootUnexpected);
        }

        if self.chain_spec.is_prague_active_at_timestamp(header.timestamp()) {
            if header.requests_hash().is_none() {
                return Err(ConsensusError::RequestsHashMissing);
            }
        } else if header.requests_hash().is_some() {
            return Err(ConsensusError::RequestsHashUnexpected);
        }

        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;

        validate_against_parent_timestamp(header.header(), parent.header())?;

        // TODO Check difficulty increment between parent and self
        // Ace age did increment it by some formula that we need to follow.
        self.validate_against_parent_gas_limit(header, parent)?;

        validate_against_parent_eip1559_base_fee(
            header.header(),
            parent.header(),
            &self.chain_spec,
        )?;

        // ensure that the blob gas fields for this block
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp()) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }

        Ok(())
    }

    fn validate_header_with_total_difficulty(
        &self,
        header: &H,
        _total_difficulty: U256,
    ) -> Result<(), ConsensusError> {
        let is_post_merge =
            self.chain_spec.is_paris_active_at_block(header.number()).is_some_and(|active| active);

        if is_post_merge {
            if !header.difficulty().is_zero() {
                return Err(ConsensusError::TheMergeDifficultyIsNotZero);
            }

            if !header.nonce().is_some_and(|nonce| nonce.is_zero()) {
                return Err(ConsensusError::TheMergeNonceIsNotZero);
            }

            if header.ommers_hash() != EMPTY_OMMER_ROOT_HASH {
                return Err(ConsensusError::TheMergeOmmerRootIsNotEmpty);
            }

            // Post-merge, the consensus layer is expected to perform checks such that the block
            // timestamp is a function of the slot. This is different from pre-merge, where blocks
            // are only allowed to be in the future (compared to the system's clock) by a certain
            // threshold.
            //
            // Block validation with respect to the parent should ensure that the block timestamp
            // is greater than its parent timestamp.

            // validate header extra data for all networks post merge
            validate_header_extra_data(header)?;

            // mixHash is used instead of difficulty inside EVM
            // https://eips.ethereum.org/EIPS/eip-4399#using-mixhash-field-instead-of-difficulty
        } else {
            // Note: This does not perform any pre merge checks for difficulty, mix_hash & nonce
            // because those are deprecated and the expectation is that a reth node syncs in reverse
            // order, making those checks obsolete.

            // Check if timestamp is in the future. Clock can drift but this can be consensus issue.
            let present_timestamp =
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

            if header.timestamp() > present_timestamp + ALLOWED_FUTURE_BLOCK_TIME_SECONDS {
                return Err(ConsensusError::TimestampIsInFuture {
                    timestamp: header.timestamp(),
                    present_timestamp,
                });
            }

            validate_header_extra_data(header)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip1559::GAS_LIMIT_BOUND_DIVISOR;
    use alloy_primitives::B256;
    use reth_chainspec::{ChainSpec, ChainSpecBuilder};
    use reth_primitives_traits::proofs;

    fn header_with_gas_limit(gas_limit: u64) -> SealedHeader {
        let header = reth_primitives::Header { gas_limit, ..Default::default() };
        SealedHeader::new(header, B256::ZERO)
    }

    #[test]
    fn test_valid_gas_limit_increase() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit((parent.gas_limit + 5) as u64);

        assert_eq!(
            EthBeaconConsensus::new(Arc::new(ChainSpec::default()))
                .validate_against_parent_gas_limit(&child, &parent),
            Ok(())
        );
    }

    #[test]
    fn test_gas_limit_below_minimum() {
        let parent = header_with_gas_limit(MINIMUM_GAS_LIMIT);
        let child = header_with_gas_limit(MINIMUM_GAS_LIMIT - 1);

        assert_eq!(
            EthBeaconConsensus::new(Arc::new(ChainSpec::default()))
                .validate_against_parent_gas_limit(&child, &parent),
            Err(ConsensusError::GasLimitInvalidMinimum { child_gas_limit: child.gas_limit as u64 })
        );
    }

    #[test]
    fn test_invalid_gas_limit_increase_exceeding_limit() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(
            parent.gas_limit + parent.gas_limit / GAS_LIMIT_BOUND_DIVISOR + 1,
        );

        assert_eq!(
            EthBeaconConsensus::new(Arc::new(ChainSpec::default()))
                .validate_against_parent_gas_limit(&child, &parent),
            Err(ConsensusError::GasLimitInvalidIncrease {
                parent_gas_limit: parent.gas_limit,
                child_gas_limit: child.gas_limit,
            })
        );
    }

    #[test]
    fn test_valid_gas_limit_decrease_within_limit() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(parent.gas_limit - 5);

        assert_eq!(
            EthBeaconConsensus::new(Arc::new(ChainSpec::default()))
                .validate_against_parent_gas_limit(&child, &parent),
            Ok(())
        );
    }

    #[test]
    fn test_invalid_gas_limit_decrease_exceeding_limit() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(
            parent.gas_limit - parent.gas_limit / GAS_LIMIT_BOUND_DIVISOR - 1,
        );

        assert_eq!(
            EthBeaconConsensus::new(Arc::new(ChainSpec::default()))
                .validate_against_parent_gas_limit(&child, &parent),
            Err(ConsensusError::GasLimitInvalidDecrease {
                parent_gas_limit: parent.gas_limit,
                child_gas_limit: child.gas_limit,
            })
        );
    }

    #[test]
    fn shanghai_block_zero_withdrawals() {
        // ensures that if shanghai is activated, and we include a block with a withdrawals root,
        // that the header is valid
        let chain_spec = Arc::new(ChainSpecBuilder::mainnet().shanghai_activated().build());

        let header = reth_primitives::Header {
            base_fee_per_gas: Some(1337),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            ..Default::default()
        };

        assert_eq!(
            EthBeaconConsensus::new(chain_spec).validate_header(&SealedHeader::seal_slow(header,)),
            Ok(())
        );
    }
}

use crate::proposals::proposals_to_block_summary;
use crate::proposals::proposals_to_epoch_info;
use crate::types::EpochInfoAggregator;
use num_bigint::{BigInt, ToBigInt};
use num_rational::Rational64;
use num_traits::Zero;
use primitive_types::U256;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::{debug, warn};
use types::BlockHeaderInfo;
use unc_cache::SyncLruCache;
use unc_chain_configs::GenesisConfig;
use unc_primitives::checked_feature;
use unc_primitives::epoch_manager::block_info::{BlockInfo, BlockInfoV2};
use unc_primitives::epoch_manager::block_summary::{BlockSummary, BlockSummaryV1};
use unc_primitives::epoch_manager::epoch_info::{EpochInfo, EpochSummary};
use unc_primitives::epoch_manager::{
    AllEpochConfig, AllEpochConfigTestOverrides, EpochConfig, ShardConfig, SlashState,
    AGGREGATOR_KEY,
};
use unc_primitives::errors::{BlockError, EpochError};
use unc_primitives::hash::CryptoHash;
use unc_primitives::shard_layout::ShardLayout;
use unc_primitives::types::validator_power::ValidatorPower;
use unc_primitives::types::validator_power_and_pledge::{
    ValidatorPowerAndPledge, ValidatorPowerAndPledgeIter,
};
use unc_primitives::types::validator_stake::ValidatorPledge;
use unc_primitives::types::{
    AccountId, ApprovalPledge, Balance, BlockChunkValidatorStats, BlockHeight, EpochId,
    EpochInfoProvider, NumBlocks, NumSeats, Power, ShardId, ValidatorId, ValidatorInfoIdentifier,
    ValidatorKickoutReason, ValidatorStats,
};
use unc_primitives::validator_mandates::AssignmentWeight;
use unc_primitives::version::{ProtocolVersion, UPGRADABILITY_FIX_PROTOCOL_VERSION};
use unc_primitives::views::{
    AllMinersView, CurrentEpochValidatorInfo, EpochValidatorInfo, NextEpochValidatorInfo,
    ValidatorKickoutView,
};
use unc_store::{DBCol, Store, StoreUpdate};

pub use crate::adapter::EpochManagerAdapter;
pub use crate::reward_calculator::RewardCalculator;
pub use crate::reward_calculator::NUM_SECONDS_IN_A_YEAR;
pub use crate::types::RngSeed;

mod adapter;
mod proposals;
mod reward_calculator;
mod shard_assignment;
pub mod shard_tracker;
pub mod test_utils;
#[cfg(test)]
mod tests;
pub mod types;
mod validator_selection;

const EPOCH_CACHE_SIZE: usize = if cfg!(feature = "no_cache") { 1 } else { 50 };
const BLOCK_CACHE_SIZE: usize = if cfg!(feature = "no_cache") { 5 } else { 1000 }; // TODO(#5080): fix this

const _HASH_CACHE_SIZE: usize = if cfg!(feature = "no_cache") { 1 } else { 2 };
const AGGREGATOR_SAVE_PERIOD: u64 = 1000;

// In epoch_manager or a common module

/// In the current architecture, various components have access to the same
/// shared mutable instance of [`EpochManager`]. This handle manages locking
/// required for such access.
///
/// It's up to the caller to ensure that there are no logical races when using
/// `.write` access.
#[derive(Clone)]
pub struct EpochManagerHandle {
    inner: Arc<RwLock<EpochManager>>,
}

impl EpochManagerHandle {
    pub fn write(&self) -> RwLockWriteGuard<EpochManager> {
        self.inner.write().unwrap()
    }

    pub fn read(&self) -> RwLockReadGuard<EpochManager> {
        self.inner.read().unwrap()
    }
}

impl EpochInfoProvider for EpochManagerHandle {
    fn validator_power(
        &self,
        epoch_id: &EpochId,
        last_block_hash: &CryptoHash,
        account_id: &AccountId,
    ) -> Result<Option<Power>, EpochError> {
        let epoch_manager = self.read();
        let last_block_info = epoch_manager.get_block_info(last_block_hash)?;
        if last_block_info.slashed().contains_key(account_id) {
            return Ok(None);
        }
        let epoch_info = epoch_manager.get_epoch_info(epoch_id)?;
        Ok(epoch_info.get_validator_id(account_id).map(|id| epoch_info.validator_power(*id)))
    }

    fn validator_total_power(
        &self,
        epoch_id: &EpochId,
        last_block_hash: &CryptoHash,
    ) -> Result<Power, EpochError> {
        let epoch_manager = self.read();
        let last_block_info = epoch_manager.get_block_info(last_block_hash)?;
        let epoch_info = epoch_manager.get_epoch_info(epoch_id)?;
        Ok(epoch_info
            .validators_iter()
            .filter(|info| !last_block_info.slashed().contains_key(info.account_id()))
            .map(|info| info.power())
            .sum())
    }

    fn minimum_power(&self, prev_block_hash: &CryptoHash) -> Result<u64, EpochError> {
        let epoch_manager = self.read();
        epoch_manager.minimum_power(prev_block_hash)
    }
    fn validator_stake(
        &self,
        epoch_id: &EpochId,
        last_block_hash: &CryptoHash,
        account_id: &AccountId,
    ) -> Result<Option<Balance>, EpochError> {
        let epoch_manager = self.read();
        let last_block_info = epoch_manager.get_block_info(last_block_hash)?;
        if last_block_info.slashed().contains_key(account_id) {
            return Ok(None);
        }
        let epoch_info = epoch_manager.get_epoch_info(epoch_id)?;
        Ok(epoch_info.get_validator_id(account_id).map(|id| epoch_info.validator_stake(*id)))
    }

    fn validator_total_stake(
        &self,
        epoch_id: &EpochId,
        last_block_hash: &CryptoHash,
    ) -> Result<Balance, EpochError> {
        let epoch_manager = self.read();
        let last_block_info = epoch_manager.get_block_info(last_block_hash)?;
        let epoch_info = epoch_manager.get_epoch_info(epoch_id)?;
        Ok(epoch_info
            .validators_iter()
            .filter(|info| !last_block_info.slashed().contains_key(info.account_id()))
            .map(|info| info.pledge())
            .sum())
    }

    fn minimum_pledge(&self, prev_block_hash: &CryptoHash) -> Result<Balance, EpochError> {
        let epoch_manager = self.read();
        epoch_manager.minimum_pledge(prev_block_hash)
    }
}

/// Tracks epoch information across different forks, such as validators.
/// Note: that even after garbage collection, the data about genesis epoch should be in the store.
pub struct EpochManager {
    store: Store,
    /// Current epoch config.
    config: AllEpochConfig,
    reward_calculator: RewardCalculator,
    /// Genesis protocol version. Useful when there are protocol upgrades.
    genesis_protocol_version: ProtocolVersion,
    genesis_num_block_producer_seats: NumSeats,
    /// Cache of epoch information.
    epochs_info: SyncLruCache<EpochId, Arc<EpochInfo>>,
    /// Cache of block information.
    blocks_info: SyncLruCache<CryptoHash, Arc<BlockInfo>>,
    /// Cache of epoch id to epoch start height
    epoch_id_to_start: SyncLruCache<EpochId, BlockHeight>,
    /// Epoch validators ordered by `block_producer_settlement`.
    epoch_validators_ordered: SyncLruCache<EpochId, Arc<[(ValidatorPowerAndPledge, bool)]>>,
    /// Unique validators ordered by `block_producer_settlement`.
    epoch_validators_ordered_unique: SyncLruCache<EpochId, Arc<[(ValidatorPowerAndPledge, bool)]>>,

    /// Unique chunk producers.
    epoch_chunk_producers_unique: SyncLruCache<EpochId, Arc<[ValidatorPowerAndPledge]>>,
    /// Aggregator that keeps statistics about the current epoch.  It’s data are
    /// synced up to the last final block.  The information are updated by
    /// [`Self::update_epoch_info_aggregator_upto_final`] method.  To get
    /// statistics up to a last block use
    /// [`Self::get_epoch_info_aggregator_upto_last`] method.
    epoch_info_aggregator: EpochInfoAggregator,
    /// Largest final height. Monotonically increasing.
    largest_final_height: BlockHeight,

    /// Counts loop iterations inside of aggregate_epoch_info_upto method.
    /// Used for tests as a bit of white-box testing.
    #[cfg(test)]
    epoch_info_aggregator_loop_counter: std::sync::atomic::AtomicUsize,
}

impl EpochManager {
    pub fn new_from_genesis_config(
        store: Store,
        genesis_config: &GenesisConfig,
    ) -> Result<Self, EpochError> {
        Self::new_from_genesis_config_with_test_overrides(store, genesis_config, None)
    }

    pub fn new_from_genesis_config_with_test_overrides(
        store: Store,
        genesis_config: &GenesisConfig,
        test_overrides: Option<AllEpochConfigTestOverrides>,
    ) -> Result<Self, EpochError> {
        let reward_calculator = RewardCalculator::new(genesis_config);
        let all_epoch_config =
            Self::new_all_epoch_config_with_test_overrides(genesis_config, test_overrides);
        let validators = genesis_config.validators();
        // Transforming ValidatorPowerAndPledge to ValidatorPower
        let power_validators: Vec<ValidatorPower> = validators
            .clone()
            .into_iter()
            .map(|validator| match validator {
                ValidatorPowerAndPledge::V1(v) => {
                    ValidatorPower::new_v1(v.account_id, v.public_key, v.power)
                }
            })
            .collect();
        let pledge_validators: Vec<ValidatorPledge> = validators
            .into_iter()
            .map(|validator| match validator {
                ValidatorPowerAndPledge::V1(v) => {
                    ValidatorPledge::new_v1(v.account_id, v.public_key, v.pledge)
                }
            })
            .collect();
        Self::new(
            store,
            all_epoch_config,
            genesis_config.protocol_version,
            reward_calculator,
            power_validators,
            pledge_validators,
        )
    }

    pub fn new_arc_handle(store: Store, genesis_config: &GenesisConfig) -> Arc<EpochManagerHandle> {
        Self::new_arc_handle_with_test_overrides(store, genesis_config, None)
    }

    pub fn new_arc_handle_with_test_overrides(
        store: Store,
        genesis_config: &GenesisConfig,
        test_overrides: Option<AllEpochConfigTestOverrides>,
    ) -> Arc<EpochManagerHandle> {
        Arc::new(
            Self::new_from_genesis_config_with_test_overrides(
                store,
                genesis_config,
                test_overrides,
            )
            .unwrap()
            .into_handle(),
        )
    }

    fn new_all_epoch_config_with_test_overrides(
        genesis_config: &GenesisConfig,
        test_overrides: Option<AllEpochConfigTestOverrides>,
    ) -> AllEpochConfig {
        let initial_epoch_config = EpochConfig::from(genesis_config);
        let epoch_config = AllEpochConfig::new_with_test_overrides(
            genesis_config.use_production_config(),
            initial_epoch_config,
            &genesis_config.chain_id,
            test_overrides,
        );
        epoch_config
    }

    pub fn new(
        store: Store,
        config: AllEpochConfig,
        genesis_protocol_version: ProtocolVersion,
        reward_calculator: RewardCalculator,
        power_validators: Vec<ValidatorPower>,
        pledge_validators: Vec<ValidatorPledge>,
    ) -> Result<Self, EpochError> {
        let validator_reward =
            HashMap::from([(reward_calculator.protocol_treasury_account.clone(), 0u128)]);
        let epoch_info_aggregator = store
            .get_ser(DBCol::EpochInfo, AGGREGATOR_KEY)
            .map_err(EpochError::from)?
            .unwrap_or_default();
        let genesis_num_block_producer_seats =
            config.for_protocol_version(genesis_protocol_version).num_block_producer_seats;
        let mut epoch_manager = EpochManager {
            store,
            config,
            reward_calculator,
            genesis_protocol_version,
            genesis_num_block_producer_seats,
            epochs_info: SyncLruCache::new(EPOCH_CACHE_SIZE),
            blocks_info: SyncLruCache::new(BLOCK_CACHE_SIZE),
            epoch_id_to_start: SyncLruCache::new(EPOCH_CACHE_SIZE),
            epoch_validators_ordered: SyncLruCache::new(EPOCH_CACHE_SIZE),
            epoch_validators_ordered_unique: SyncLruCache::new(EPOCH_CACHE_SIZE),
            epoch_chunk_producers_unique: SyncLruCache::new(EPOCH_CACHE_SIZE),
            epoch_info_aggregator,
            #[cfg(test)]
            epoch_info_aggregator_loop_counter: Default::default(),
            largest_final_height: 0,
        };
        let genesis_epoch_id = EpochId::default();
        if !epoch_manager.has_epoch_info(&genesis_epoch_id)? {
            // Missing genesis epoch, means that there is no validator initialize yet.
            let genesis_epoch_config =
                epoch_manager.config.for_protocol_version(genesis_protocol_version);
            let epoch_info = proposals_to_epoch_info(
                &genesis_epoch_config,
                [0; 32],
                &EpochInfo::default(),
                power_validators.clone(),
                pledge_validators.clone(),
                HashMap::default(),
                validator_reward.clone(),
                0,
                genesis_protocol_version,
                genesis_protocol_version,
            )?;
            // Dummy block info.
            // Artificial block we add to simplify implementation: dummy block is the
            // parent of genesis block that points to itself.
            // If we view it as block in epoch -1 and height -1, it naturally extends the
            // EpochId formula using T-2 for T=1, and height field is unused.
            let the_block_info = BlockInfo::new(
                Default::default(),
                0,
                0,
                Default::default(),
                Default::default(),
                power_validators.clone(),
                pledge_validators.clone(),
                vec![],
                vec![],
                0,
                0,
                0,
                //customized by james savechives
                Default::default(),
                vec![],
                Default::default(),
                vec![],
                vec![],
                vec![],
                Default::default(),
                Default::default(),
                Default::default(),
                validator_reward,
                0,
                0,
                power_validators,
                pledge_validators,
                Default::default(),
                Default::default(),
            );
            let block_info = Arc::new(the_block_info);
            let mut store_update = epoch_manager.store.store_update();
            epoch_manager.save_epoch_info(
                &mut store_update,
                &genesis_epoch_id,
                Arc::new(epoch_info),
            )?;
            epoch_manager.save_block_info(&mut store_update, block_info)?;
            store_update.commit()?;
        }
        Ok(epoch_manager)
    }

    pub fn into_handle(self) -> EpochManagerHandle {
        let inner = Arc::new(RwLock::new(self));
        EpochManagerHandle { inner }
    }

    /// Only used in mock node
    /// Copy the necessary epoch info related to `block_hash` from `source_epoch_manager` to
    /// the current epoch manager.
    /// Note that this function doesn't copy info stored in EpochInfoAggregator, so `block_hash` must be
    /// the last block in an epoch in order for the epoch manager to work properly after this function
    /// is called
    pub fn copy_epoch_info_as_of_block(
        &mut self,
        block_hash: &CryptoHash,
        source_epoch_manager: &EpochManager,
    ) -> Result<(), EpochError> {
        let block_info = source_epoch_manager.get_block_info(block_hash)?;
        let prev_hash = block_info.prev_hash();
        let epoch_id = &source_epoch_manager.get_epoch_id_from_prev_block(prev_hash)?;
        let next_epoch_id = &source_epoch_manager.get_next_epoch_id_from_prev_block(prev_hash)?;
        let mut store_update = self.store.store_update();
        self.save_epoch_info(
            &mut store_update,
            epoch_id,
            source_epoch_manager.get_epoch_info(epoch_id)?,
        )?;
        // save next epoch info too
        self.save_epoch_info(
            &mut store_update,
            next_epoch_id,
            source_epoch_manager.get_epoch_info(next_epoch_id)?,
        )?;
        // save next next epoch info if the block is the last block
        if source_epoch_manager.is_next_block_epoch_start(block_hash)? {
            let next_next_epoch_id =
                source_epoch_manager.get_next_epoch_id_from_prev_block(block_hash)?;
            self.save_epoch_info(
                &mut store_update,
                &next_next_epoch_id,
                source_epoch_manager.get_epoch_info(&next_next_epoch_id)?,
            )?;
        }

        // save block info for the first block in the epoch
        let epoch_first_block = block_info.epoch_first_block();
        self.save_block_info(
            &mut store_update,
            source_epoch_manager.get_block_info(epoch_first_block)?,
        )?;

        self.save_block_info(&mut store_update, block_info)?;

        self.save_epoch_start(
            &mut store_update,
            epoch_id,
            source_epoch_manager.get_epoch_start_from_epoch_id(epoch_id)?,
        )?;

        store_update.commit()?;
        Ok(())
    }

    pub fn init_after_epoch_sync(
        &mut self,
        prev_epoch_first_block_info: BlockInfo,
        prev_epoch_prev_last_block_info: BlockInfo,
        prev_epoch_last_block_info: BlockInfo,
        prev_epoch_id: &EpochId,
        prev_epoch_info: EpochInfo,
        epoch_id: &EpochId,
        epoch_info: EpochInfo,
        next_epoch_id: &EpochId,
        next_epoch_info: EpochInfo,
    ) -> Result<StoreUpdate, EpochError> {
        let mut store_update = self.store.store_update();
        self.save_block_info(&mut store_update, Arc::new(prev_epoch_first_block_info))?;
        self.save_block_info(&mut store_update, Arc::new(prev_epoch_prev_last_block_info))?;
        self.save_block_info(&mut store_update, Arc::new(prev_epoch_last_block_info))?;
        self.save_epoch_info(&mut store_update, prev_epoch_id, Arc::new(prev_epoch_info))?;
        self.save_epoch_info(&mut store_update, epoch_id, Arc::new(epoch_info))?;
        self.save_epoch_info(&mut store_update, next_epoch_id, Arc::new(next_epoch_info))?;
        // TODO #3488
        // put unreachable! here to avoid warnings
        unreachable!();
        // Ok(store_update)
    }

    /// When computing validators to kickout, we exempt some validators first so that
    /// the total pledge of exempted validators exceed a threshold. This is to make sure
    /// we don't kick out too many validators in case of network instability.
    /// We also make sure that these exempted validators were not kicked out in the last epoch,
    /// so it is guaranteed that they will stay as validators after this epoch.
    fn compute_exempted_kickout(
        epoch_info: &EpochInfo,
        validator_block_chunk_stats: &HashMap<AccountId, BlockChunkValidatorStats>,
        total_pledge: Balance,
        exempt_perc: u8,
        prev_validator_kickout: &HashMap<AccountId, ValidatorKickoutReason>,
    ) -> HashSet<AccountId> {
        // We want to make sure the total pledge of validators that will be kicked out in this epoch doesn't exceed
        // config.validator_max_kickout_pledge_ratio of total pledge.
        // To achieve that, we sort all validators by their average uptime (average of block and chunk
        // uptime) and add validators to `exempted_validators` one by one, from high uptime to low uptime,
        // until the total excepted pledge exceeds the ratio of total pledge that we need to keep.
        // Later when we perform the check to kick out validators, we don't kick out validators in
        // exempted_validators.
        let mut exempted_validators = HashSet::new();
        if checked_feature!("stable", MaxKickoutPledge, epoch_info.protocol_version()) {
            let min_keep_pledge = total_pledge * (exempt_perc as u128) / 100;
            let mut sorted_validators = validator_block_chunk_stats
                .iter()
                .map(|(account, stats)| {
                    let production_ratio =
                        if stats.block_stats.expected == 0 && stats.chunk_stats.expected == 0 {
                            Rational64::from_integer(1)
                        } else if stats.block_stats.expected == 0 {
                            Rational64::new(
                                stats.chunk_stats.produced as i64,
                                stats.chunk_stats.expected as i64,
                            )
                        } else if stats.chunk_stats.expected == 0 {
                            Rational64::new(
                                stats.block_stats.produced as i64,
                                stats.block_stats.expected as i64,
                            )
                        } else {
                            (Rational64::new(
                                stats.chunk_stats.produced as i64,
                                stats.chunk_stats.expected as i64,
                            ) + Rational64::new(
                                stats.block_stats.produced as i64,
                                stats.block_stats.expected as i64,
                            )) / 2
                        };
                    (account, production_ratio)
                })
                .collect::<Vec<_>>();
            sorted_validators.sort_by_key(|a| a.1);
            let mut exempted_pledge: Balance = 0;
            for (account_id, _) in sorted_validators.into_iter().rev() {
                if exempted_pledge >= min_keep_pledge {
                    break;
                }
                if !prev_validator_kickout.contains_key(account_id) {
                    exempted_pledge += epoch_info
                        .get_validator_by_account(account_id)
                        .map(|v| v.pledge())
                        .unwrap_or_default();
                    exempted_validators.insert(account_id.clone());
                }
            }
        }
        exempted_validators
    }

    /// # Parameters
    /// epoch_info
    /// block_validator_tracker
    /// chunk_validator_tracker
    ///
    /// slashed: set of slashed validators
    /// prev_validator_kickout: previously kicked out
    ///
    /// # Returns
    /// (set of validators to kickout, set of validators to reward with stats)
    ///
    /// - Slashed validators are ignored (they are handled separately)
    /// - The total pledge of validators that will be kicked out will not exceed
    ///   config.validator_max_kickout_pledge_perc of total pledge of all validators. This is
    ///   to ensure we don't kick out too many validators in case of network instability.
    /// - A validator is kicked out if he produced too few blocks or chunks
    /// - If all validators are either previously kicked out or to be kicked out, we choose one not to
    /// kick out
    fn compute_kickout_info(
        config: &EpochConfig,
        epoch_info: &EpochInfo,
        block_validator_tracker: &HashMap<ValidatorId, ValidatorStats>,
        chunk_validator_tracker: &HashMap<ShardId, HashMap<ValidatorId, ValidatorStats>>,
        slashed: &HashMap<AccountId, SlashState>,
        prev_validator_kickout: &HashMap<AccountId, ValidatorKickoutReason>,
    ) -> (HashMap<AccountId, ValidatorKickoutReason>, HashMap<AccountId, BlockChunkValidatorStats>)
    {
        let block_producer_kickout_threshold = config.block_producer_kickout_threshold;
        let chunk_producer_kickout_threshold = config.chunk_producer_kickout_threshold;
        let mut validator_block_chunk_stats = HashMap::new();
        let mut total_pledge: Balance = 0;
        let mut maximum_block_prod = 0;
        let mut max_validator = None;

        for (i, v) in epoch_info.validators_iter().enumerate() {
            let account_id = v.account_id();
            if slashed.contains_key(account_id) {
                continue;
            }
            let block_stats = block_validator_tracker
                .get(&(i as u64))
                .unwrap_or_else(|| &ValidatorStats { expected: 0, produced: 0 })
                .clone();
            let mut chunk_stats = ValidatorStats { produced: 0, expected: 0 };
            for (_, tracker) in chunk_validator_tracker.iter() {
                if let Some(stat) = tracker.get(&(i as u64)) {
                    chunk_stats.expected += stat.expected;
                    chunk_stats.produced += stat.produced;
                }
            }
            total_pledge += v.pledge();
            let is_already_kicked_out = prev_validator_kickout.contains_key(account_id);
            if (max_validator.is_none() || block_stats.produced > maximum_block_prod)
                && !is_already_kicked_out
            {
                maximum_block_prod = block_stats.produced;
                max_validator = Some(account_id.clone());
            }
            validator_block_chunk_stats
                .insert(account_id.clone(), BlockChunkValidatorStats { block_stats, chunk_stats });
        }

        let exempt_perc =
            100_u8.checked_sub(config.validator_max_kickout_pledge_perc).unwrap_or_default();
        let exempted_validators = Self::compute_exempted_kickout(
            epoch_info,
            &validator_block_chunk_stats,
            total_pledge,
            exempt_perc,
            prev_validator_kickout,
        );
        let mut all_kicked_out = true;
        let mut validator_kickout = HashMap::new();
        for (account_id, stats) in validator_block_chunk_stats.iter() {
            if exempted_validators.contains(account_id) {
                all_kicked_out = false;
                continue;
            }
            if stats.block_stats.produced * 100
                < u64::from(block_producer_kickout_threshold) * stats.block_stats.expected
            {
                validator_kickout.insert(
                    account_id.clone(),
                    ValidatorKickoutReason::NotEnoughBlocks {
                        produced: stats.block_stats.produced,
                        expected: stats.block_stats.expected,
                    },
                );
            }
            if stats.chunk_stats.produced * 100
                < u64::from(chunk_producer_kickout_threshold) * stats.chunk_stats.expected
            {
                validator_kickout.entry(account_id.clone()).or_insert_with(|| {
                    ValidatorKickoutReason::NotEnoughChunks {
                        produced: stats.chunk_stats.produced,
                        expected: stats.chunk_stats.expected,
                    }
                });
            }
            let is_already_kicked_out = prev_validator_kickout.contains_key(account_id);
            if !validator_kickout.contains_key(account_id) {
                if !is_already_kicked_out {
                    all_kicked_out = false;
                }
            }
        }
        if all_kicked_out {
            tracing::info!(target:"epoch_manager", "We are about to kick out all validators in the next two epochs, so we are going to save one {:?}", max_validator);
            if let Some(validator) = max_validator {
                validator_kickout.remove(&validator);
            }
        }
        for account_id in validator_kickout.keys() {
            validator_block_chunk_stats.remove(account_id);
        }
        (validator_kickout, validator_block_chunk_stats)
    }

    fn collect_blocks_info(
        &mut self,
        last_block_info: &BlockInfo,
        last_block_hash: &CryptoHash,
    ) -> Result<EpochSummary, EpochError> {
        let epoch_info = self.get_epoch_info(last_block_info.epoch_id())?;
        let next_epoch_id = self.get_next_epoch_id(last_block_hash)?;
        let next_epoch_info = self.get_epoch_info(&next_epoch_id)?;

        let EpochInfoAggregator {
            block_tracker: block_validator_tracker,
            shard_tracker: chunk_validator_tracker,
            all_power_proposals,
            all_pledge_proposals,
            version_tracker,
            ..
        } = self.get_epoch_info_aggregator_upto_last(last_block_hash)?;

        let mut power_proposals = vec![];
        let mut pledge_proposals = vec![];
        let mut validator_kickout = HashMap::new();

        // Next protocol version calculation.
        let mut versions = HashMap::new();
        for (validator_id, version) in version_tracker {
            let pledge = epoch_info.validator_stake(validator_id);
            *versions.entry(version).or_insert(0) += pledge;
        }
        let total_block_producer_pledge: u128 = epoch_info
            .block_producers_settlement()
            .iter()
            .copied()
            .collect::<HashSet<_>>()
            .iter()
            .map(|&id| epoch_info.validator_stake(id))
            .sum();

        let protocol_version =
            if epoch_info.protocol_version() >= UPGRADABILITY_FIX_PROTOCOL_VERSION {
                next_epoch_info.protocol_version()
            } else {
                epoch_info.protocol_version()
            };

        let config = self.config.for_protocol_version(protocol_version);
        // Note: non-deterministic iteration is fine here, there can be only one
        // version with large enough pledge.
        let next_version = if let Some((version, pledge)) =
            versions.into_iter().max_by_key(|&(_version, pledge)| pledge)
        {
            let numer = *config.protocol_upgrade_pledge_threshold.numer() as u128;
            let denom = *config.protocol_upgrade_pledge_threshold.denom() as u128;
            let threshold = total_block_producer_pledge * numer / denom;
            if pledge > threshold {
                version
            } else {
                protocol_version
            }
        } else {
            protocol_version
        };
        // Gather slashed validators and add them to kick out first.
        let slashed_validators = last_block_info.slashed();
        for (account_id, _) in slashed_validators.iter() {
            validator_kickout.insert(account_id.clone(), ValidatorKickoutReason::Slashed);
        }

        for (_account_id, power_proposal) in all_power_proposals.clone() {
            // if !slashed_validators.contains_key(&account_id) {
            //     if power_proposal.power() == 0
            //         && *next_epoch_info.power_change().get(&account_id).unwrap_or(&0) != 0
            //     {
            //         validator_kickout.insert(account_id.clone(), ValidatorKickoutReason::Unpowered);
            //     }
            //     power_proposals.push(power_proposal.clone());
            // }
            power_proposals.push(power_proposal.clone());
        }

        for (account_id, pledge_proposal) in all_pledge_proposals.clone() {
            if !slashed_validators.contains_key(&account_id) {
                if pledge_proposal.pledge() == 0
                    && *next_epoch_info.pledge_change().get(&account_id).unwrap_or(&0) != 0
                {
                    validator_kickout.insert(account_id.clone(), ValidatorKickoutReason::Unpledge);
                }
                pledge_proposals.push(pledge_proposal.clone());
            }
        }

        let prev_epoch_last_block_hash =
            *self.get_block_info(last_block_info.epoch_first_block())?.prev_hash();
        let prev_validator_kickout = next_epoch_info.validator_kickout();

        let config = self.config.for_protocol_version(epoch_info.protocol_version());
        // Compute kick outs for validators who are offline.
        let (kickout, validator_block_chunk_stats) = Self::compute_kickout_info(
            &config,
            &epoch_info,
            &block_validator_tracker,
            &chunk_validator_tracker,
            slashed_validators,
            prev_validator_kickout,
        );
        validator_kickout.extend(kickout);
        debug!(
            target: "epoch_manager",
            "All power proposals: {:?}, All pledge proposals: {:?}, Kickouts: {:?}, Block Tracker: {:?}, Shard Tracker: {:?}",
            all_power_proposals, all_pledge_proposals, validator_kickout.clone(), block_validator_tracker, chunk_validator_tracker
        );

        Ok(EpochSummary {
            prev_epoch_last_block_hash,
            all_power_proposals: power_proposals,
            all_pledge_proposals: pledge_proposals,
            validator_kickout,
            validator_block_chunk_stats,
            next_version,
        })
    }
    /// Finalize block
    fn finalize_block_summary_for_block(
        &mut self,
        block_info: &BlockInfo,
        last_block_hash: &CryptoHash,
        rng_seed: RngSeed,
    ) -> Result<BlockSummary, BlockError> {
        let validator_stake =
            block_info.validators_iter().map(|r| r.account_and_pledge()).collect::<HashMap<_, _>>();

        let (all_power_proposals, all_pledge_proposals, validator_kickout) = match block_info {
            // Assuming last_block_summary is wrapped in an Arc
            BlockInfo::V1(summary) => {
                // Now you can access the fields of BlockSummaryV1 through `summary`
                (
                    &summary.all_power_proposals,
                    &summary.all_pledge_proposals,
                    &summary.validator_kickout,
                )
                // Add more fields as needed
            }
            BlockInfo::V2(summary) => {
                // Now you can access the fields of BlockSummaryV1 through `summary`
                (
                    &summary.all_power_proposals,
                    &summary.all_pledge_proposals,
                    &summary.validator_kickout,
                )
                // Add more fields as needed
            }
        };

        //FIXME: This is a hack to get the block reward and minted amount
        let epoch_summary = self.collect_blocks_info(block_info, last_block_hash)?;
        let EpochSummary {
            //    all_power_proposals,
            //    all_pledge_proposals,
            //    validator_kickout,
            validator_block_chunk_stats,
            //next_version,
            ..
        } = epoch_summary;

        let next_version = 1u16 as ProtocolVersion;

        let (validator_reward, minted_amount) = {
            let last_epoch_last_block_hash =
                *self.get_block_info(block_info.epoch_first_block())?.prev_hash();
            let last_block_in_last_epoch = self.get_block_info(&last_epoch_last_block_hash)?;
            //    assert!(block_info.timestamp_nanosec() > last_block_in_last_epoch.timestamp_nanosec());
            let epoch_duration =
                block_info.timestamp_nanosec() - last_block_in_last_epoch.timestamp_nanosec();
            self.reward_calculator.calculate_reward(
                validator_block_chunk_stats,
                &validator_stake,
                *block_info.total_supply(),
                0u32,
                self.genesis_protocol_version,
                epoch_duration,
            )
        };
        let this_epoch_config = self.config.for_protocol_version(next_version);
        let this_block_summary = match proposals_to_block_summary(
            &this_epoch_config,
            block_info.hash(),
            &last_block_hash,
            rng_seed,
            &block_info,
            all_power_proposals.to_vec(),
            all_pledge_proposals.to_vec(),
            validator_kickout.clone(),
            validator_reward,
            minted_amount,
            next_version,
        ) {
            Ok(this_block_summary) => this_block_summary,
            // Err(BlockError::ThresholdError { pledge_sum, num_seats }) => {
            //     warn!(target: "epoch_manager", "Not enough pledge for required number of seats (all validators tried to unpledge?): amount = {} for {}", pledge_sum, num_seats);
            //     return Err(BlockError::ThresholdError { pledge_sum, num_seats });
            // }
            // Err(BlockError::NotEnoughValidators { num_validators, num_shards }) => {
            //     warn!(target: "epoch_manager", "Not enough validators for required number of shards (all validators tried to unpledge?): num_validators={} num_shards={}", num_validators, num_shards);
            //     return Err(BlockError::NotEnoughValidators { num_validators, num_shards });
            // }
            // Err(err) => return Err(err),
            _ => BlockSummary::default(),
        };
        // This epoch info is computed for the epoch after next (T+2),
        // where epoch_id of it is the hash of last block in this epoch (T).
        // self.save_block_summary(store_update, &block_info.hash(), Arc::new(this_block_summary))?;
        Ok(this_block_summary)
    }
    /// Finalizes epoch (T), where given last block hash is given, and returns next next epoch id (T + 2).
    fn finalize_epoch(
        &mut self,
        store_update: &mut StoreUpdate,
        block_info: &BlockInfo,
        last_block_hash: &CryptoHash,
        rng_seed: RngSeed,
    ) -> Result<(), EpochError> {
        let epoch_summary = self.collect_blocks_info(block_info, last_block_hash)?;
        let epoch_info = self.get_epoch_info(block_info.epoch_id())?;
        let epoch_protocol_version = epoch_info.protocol_version();
        let validator_stake =
            epoch_info.validators_iter().map(|r| r.account_and_pledge()).collect::<HashMap<_, _>>();
        let next_epoch_id = self.get_next_epoch_id_from_info(block_info)?;
        let next_epoch_info = self.get_epoch_info(&next_epoch_id)?;
        self.save_epoch_validator_info(store_update, block_info.epoch_id(), &epoch_summary)?;

        let EpochSummary {
            //    all_power_proposals,
            //    all_pledge_proposals,
            //    validator_kickout,
            validator_block_chunk_stats,
            next_version,
            ..
        } = epoch_summary;
        // start james savechives
        let (all_power_proposals, all_pledge_proposals, validator_kickout): (
            Vec<ValidatorPower>,
            Vec<ValidatorPledge>,
            HashMap<AccountId, ValidatorKickoutReason>,
        ) = match block_info {
            // Assuming last_block_summary is wrapped in an Arc
            BlockInfo::V1(summary) => {
                // Now you can access the fields of BlockSummaryV1 through `summary`
                (
                    summary.clone().all_power_proposals,
                    summary.clone().all_pledge_proposals,
                    summary.clone().validator_kickout,
                )
                // Add more fields as needed
            }
            BlockInfo::V2(summary) => {
                // Now you can access the fields of BlockSummaryV1 through `summary`
                (
                    summary.clone().all_power_proposals,
                    summary.clone().all_pledge_proposals,
                    summary.clone().validator_kickout,
                )
                // Add more fields as needed
            }
        };
        // end james savechives
        let (validator_reward, minted_amount) = {
            let last_epoch_last_block_hash =
                *self.get_block_info(block_info.epoch_first_block())?.prev_hash();
            let last_block_in_last_epoch = self.get_block_info(&last_epoch_last_block_hash)?;
            assert!(block_info.timestamp_nanosec() > last_block_in_last_epoch.timestamp_nanosec());
            let epoch_duration =
                block_info.timestamp_nanosec() - last_block_in_last_epoch.timestamp_nanosec();
            self.reward_calculator.calculate_reward(
                validator_block_chunk_stats,
                &validator_stake,
                *block_info.total_supply(),
                epoch_protocol_version,
                self.genesis_protocol_version,
                epoch_duration,
            )
        };
        let next_next_epoch_config = self.config.for_protocol_version(next_version);
        let next_next_epoch_info = match proposals_to_epoch_info(
            &next_next_epoch_config,
            rng_seed,
            &next_epoch_info,
            all_power_proposals,
            all_pledge_proposals,
            validator_kickout,
            validator_reward,
            minted_amount,
            next_version,
            epoch_protocol_version,
        ) {
            Ok(next_next_epoch_info) => next_next_epoch_info,
            Err(EpochError::ThresholdError { pledge_sum, num_seats }) => {
                warn!(target: "epoch_manager", "Not enough pledge for required number of seats (all validators tried to unpledge?): amount = {} for {}", pledge_sum, num_seats);
                let mut epoch_info = EpochInfo::clone(&next_epoch_info);
                *epoch_info.epoch_height_mut() += 1;
                epoch_info
            }
            Err(EpochError::NotEnoughValidators { num_validators, num_shards }) => {
                warn!(target: "epoch_manager", "Not enough validators for required number of shards (all validators tried to unpledge?): num_validators={} num_shards={}", num_validators, num_shards);
                let mut epoch_info = EpochInfo::clone(&next_epoch_info);
                *epoch_info.epoch_height_mut() += 1;
                epoch_info
            }
            Err(err) => return Err(err),
        };
        let next_next_epoch_id = EpochId(*last_block_hash);
        debug!(target: "epoch_manager", "next next epoch height: {}, id: {:?}, protocol version: {} shard layout: {:?} config: {:?}",
               next_next_epoch_info.epoch_height(),
               &next_next_epoch_id,
               next_next_epoch_info.protocol_version(),
               self.config.for_protocol_version(next_next_epoch_info.protocol_version()).shard_layout,
            self.config.for_protocol_version(next_next_epoch_info.protocol_version()));
        // This epoch info is computed for the epoch after next (T+2),
        // where epoch_id of it is the hash of last block in this epoch (T).
        self.save_epoch_info(store_update, &next_next_epoch_id, Arc::new(next_next_epoch_info))?;
        Ok(())
    }

    pub fn record_block_info(
        &mut self,
        mut block_info: BlockInfo,
        rng_seed: RngSeed,
    ) -> Result<StoreUpdate, EpochError> {
        let current_hash = *block_info.hash();
        let mut store_update = self.store.store_update();
        // Check that we didn't record this block yet.
        if !self.has_block_info(&current_hash)? {
            if block_info.prev_hash() == &CryptoHash::default() {
                // This is genesis block, we special case as new epoch.
                assert_eq!(block_info.power_proposals_iter().len(), 0);
                let pre_genesis_epoch_id = EpochId::default();
                let genesis_epoch_info = self.get_epoch_info(&pre_genesis_epoch_id)?;
                self.save_block_info(&mut store_update, Arc::new(block_info.clone()))?;
                self.save_epoch_info(
                    &mut store_update,
                    &EpochId(current_hash),
                    genesis_epoch_info,
                )?;
            } else {
                let prev_block_info = self.get_block_info(block_info.prev_hash())?;

                let mut is_epoch_start = false;
                if prev_block_info.prev_hash() == &CryptoHash::default() {
                    // This is first real block, starts the new epoch.
                    *block_info.epoch_id_mut() = EpochId::default();
                    *block_info.epoch_first_block_mut() = current_hash;
                    is_epoch_start = true;
                } else if self.is_next_block_in_next_epoch(&prev_block_info)? {
                    // Current block is in the new epoch, finalize the one in prev_block.
                    *block_info.epoch_id_mut() =
                        self.get_next_epoch_id_from_info(&prev_block_info)?;
                    *block_info.epoch_first_block_mut() = current_hash;
                    is_epoch_start = true;
                } else {
                    // Same epoch as parent, copy epoch_id and epoch_start_height.
                    *block_info.epoch_id_mut() = prev_block_info.epoch_id().clone();
                    *block_info.epoch_first_block_mut() = *prev_block_info.epoch_first_block();
                }
                let epoch_info = self.get_epoch_info(block_info.epoch_id())?;

                // Keep `slashed` from previous block if they are still in the epoch info pledge change
                // (e.g. we need to keep track that they are still slashed, because when we compute
                // returned pledge we are skipping account ids that are slashed in `pledge_change`).
                for (account_id, slash_state) in prev_block_info.slashed() {
                    if is_epoch_start {
                        if slash_state == &SlashState::DoubleSign
                            || slash_state == &SlashState::Other
                        {
                            block_info
                                .slashed_mut()
                                .entry(account_id.clone())
                                .or_insert(SlashState::AlreadySlashed);
                        } else if epoch_info.pledge_change().contains_key(account_id) {
                            block_info
                                .slashed_mut()
                                .entry(account_id.clone())
                                .or_insert_with(|| slash_state.clone());
                        }
                    } else {
                        block_info
                            .slashed_mut()
                            .entry(account_id.clone())
                            .and_modify(|e| {
                                if let SlashState::Other = slash_state {
                                    *e = SlashState::Other;
                                }
                            })
                            .or_insert_with(|| slash_state.clone());
                    }
                }

                if is_epoch_start {
                    self.save_epoch_start(
                        &mut store_update,
                        block_info.epoch_id(),
                        block_info.height(),
                    )?;
                }

                let block_info = Arc::new(block_info);
                // Save current block info.
                self.save_block_info(&mut store_update, Arc::clone(&block_info))?;

                // let block_summary = Arc::new(block_summary);
                // // Save current block summary
                // self.save_block_summary(&mut store_update, &block_info.hash().clone(), Arc::clone(&block_summary))?;

                if block_info.last_finalized_height() > self.largest_final_height {
                    self.largest_final_height = block_info.last_finalized_height();

                    // Update epoch info aggregator.  We only update the if
                    // there is a change in the last final block.  This way we
                    // never need to rollback any information in
                    // self.epoch_info_aggregator.
                    self.update_epoch_info_aggregator_upto_final(
                        block_info.last_final_block_hash(),
                        &mut store_update,
                    )?;
                }

                // If this is the last block in the epoch, finalize this epoch.
                if self.is_next_block_in_next_epoch(&block_info)? {
                    self.finalize_epoch(
                        &mut store_update,
                        &block_info.clone(),
                        &current_hash.clone(),
                        rng_seed,
                    )?;
                }
            }
        }
        Ok(store_update)
    }

    /// Given block hash, return all the miners
    pub fn get_all_miners(
        &self,
        block_hash: &CryptoHash,
        // height: BlockHeight,
    ) -> Result<AllMinersView, BlockError> {
        let block_info = self.get_block_info(block_hash)?;
        let validators = block_info.validators_iter();
        let all_miners_view: AllMinersView = validators.into();
        Ok(all_miners_view)
    }

    /// Given epoch id and height, returns validator information that suppose to produce
    /// the block at that height. We don't require caller to know about EpochIds.
    pub fn get_block_producer_info(
        &self,
        epoch_id: &EpochId,
        height: BlockHeight,
    ) -> Result<ValidatorPowerAndPledge, EpochError> {
        let epoch_info = self.get_epoch_info(epoch_id)?;
        let validator_id = Self::block_producer_from_info(&epoch_info, height);

        Ok(epoch_info.get_validator(validator_id))
    }

    pub fn get_block_producer_info_by_hash(
        &self,
        block_hash: &CryptoHash,
        // height: BlockHeight,
    ) -> Result<ValidatorPowerAndPledge, BlockError> {
        let block_info = self.get_block_info(block_hash)?;
        // let current_height = block_info.height();
        // if current_height +1 != height {
        //     return Err(BlockError::BlockOutOfBounds(*block_hash));
        // }
        let random_value = block_info.random_value();
        let validators = block_info.validators_iter();
        Self::choose_validator_vrf(validators, Self::hash_to_bigint(random_value))
    }

    fn hash_to_bigint(hash: &CryptoHash) -> BigInt {
        BigInt::from_bytes_be(num_bigint::Sign::Plus, hash.as_ref())
    }

    fn choose_validator_vrf(
        validators_iter: ValidatorPowerAndPledgeIter,
        random_value: BigInt,
    ) -> Result<ValidatorPowerAndPledge, BlockError> {
        let mut total_weight: BigInt = Zero::zero();
        for validator in validators_iter.clone() {
            let validator_power = match validator {
                ValidatorPowerAndPledge::V1(v) => v.power.to_bigint().unwrap_or_else(Zero::zero),
            };
            total_weight += validator_power;
        }

        if total_weight.is_zero() {
            return Err(BlockError::ValidatorTotalPowerError(String::from("Total Power is zero")));
        }

        let mut cumulative_weight = Zero::zero();
        let target = random_value % &total_weight;

        for validator in validators_iter {
            let validator_power = match validator {
                ValidatorPowerAndPledge::V1(ref v) => {
                    v.power.to_bigint().unwrap_or_else(Zero::zero)
                }
            };
            cumulative_weight += &validator_power;
            if target < cumulative_weight {
                return Ok(validator);
            }
        }

        return Err(BlockError::NoAvailableValidator(String::from(
            "Block Producer is not available",
        )));
    }

    /// Returns settlement of all block producers in current epoch, with indicator on whether they are slashed or not.
    pub fn get_all_block_producers_settlement(
        &self,
        epoch_id: &EpochId,
        last_known_block_hash: &CryptoHash,
    ) -> Result<Arc<[(ValidatorPowerAndPledge, bool)]>, EpochError> {
        // TODO(3674): Revisit this when we enable slashing
        self.epoch_validators_ordered.get_or_try_put(epoch_id.clone(), |epoch_id| {
            let block_info = self.get_block_info(last_known_block_hash)?;
            let epoch_info = self.get_epoch_info(epoch_id)?;
            let result = epoch_info
                .block_producers_settlement()
                .iter()
                .map(|&validator_id| {
                    let validator_stake = epoch_info.get_validator(validator_id);
                    let is_slashed =
                        block_info.slashed().contains_key(validator_stake.account_id());
                    (validator_stake, is_slashed)
                })
                .collect();
            Ok(result)
        })
    }

    /// Returns all unique block producers in current epoch sorted by account_id, with indicator on whether they are slashed or not.
    pub fn get_all_block_producers_ordered(
        &self,
        epoch_id: &EpochId,
        last_known_block_hash: &CryptoHash,
    ) -> Result<Arc<[(ValidatorPowerAndPledge, bool)]>, EpochError> {
        self.epoch_validators_ordered_unique.get_or_try_put(epoch_id.clone(), |epoch_id| {
            let settlement =
                self.get_all_block_producers_settlement(epoch_id, last_known_block_hash)?;
            let mut validators: HashSet<AccountId> = HashSet::default();
            let result = settlement
                .iter()
                .filter(|(validator_stake, _is_slashed)| {
                    let account_id = validator_stake.account_id();
                    validators.insert(account_id.clone())
                })
                .cloned()
                .collect();
            Ok(result)
        })
    }

    /// Returns settlement of all chunk producers in the current epoch.
    pub fn get_all_chunk_producers(
        &self,
        epoch_id: &EpochId,
    ) -> Result<Arc<[ValidatorPowerAndPledge]>, EpochError> {
        self.epoch_chunk_producers_unique.get_or_try_put(epoch_id.clone(), |epoch_id| {
            let mut producers: HashSet<u64> = HashSet::default();

            // Collect unique chunk producers.
            let epoch_info = self.get_epoch_info(epoch_id)?;
            for chunk_producers in epoch_info.chunk_producers_settlement() {
                producers.extend(chunk_producers);
            }

            Ok(producers.iter().map(|producer_id| epoch_info.get_validator(*producer_id)).collect())
        })
    }

    /// Returns the list of chunk validators for the given shard_id and height.
    pub fn get_chunk_validators(
        &self,
        epoch_id: &EpochId,
        shard_id: ShardId,
        height: BlockHeight,
    ) -> Result<HashMap<AccountId, AssignmentWeight>, EpochError> {
        let epoch_info = self.get_epoch_info(epoch_id)?;
        let chunk_validators_per_shard = epoch_info.sample_chunk_validators(height);
        let chunk_validators =
            chunk_validators_per_shard.get(shard_id as usize).ok_or_else(|| {
                EpochError::ChunkValidatorSelectionError(format!(
                    "Invalid shard ID {} for height {}, epoch {:?} for chunk validation",
                    shard_id, height, epoch_id,
                ))
            })?;
        Ok(chunk_validators
            .iter()
            .map(|(validator_id, seats)| {
                (epoch_info.get_validator(*validator_id).take_account_id(), seats.clone())
            })
            .collect())
    }

    /// get_heuristic_block_approvers_ordered: block producers for epoch
    /// get_all_block_producers_ordered: block producers for epoch, slashing info
    /// get_all_block_approvers_ordered: block producers for epoch, slashing info, sometimes block producers for next epoch
    pub fn get_heuristic_block_approvers_ordered(
        &self,
        epoch_id: &EpochId,
    ) -> Result<Vec<ApprovalPledge>, EpochError> {
        let epoch_info = self.get_epoch_info(epoch_id)?;
        let mut result = vec![];
        let mut validators: HashSet<AccountId> = HashSet::new();
        for validator_id in epoch_info.block_producers_settlement().into_iter() {
            let validator_stake = epoch_info.get_validator(*validator_id);
            let account_id = validator_stake.account_id();
            if validators.insert(account_id.clone()) {
                result.push(validator_stake.get_approval_pledge(false));
            }
        }

        Ok(result)
    }

    pub fn get_all_block_approvers_ordered(
        &self,
        parent_hash: &CryptoHash,
    ) -> Result<Vec<(ApprovalPledge, bool)>, EpochError> {
        let current_epoch_id = self.get_epoch_id_from_prev_block(parent_hash)?;
        let next_epoch_id = self.get_next_epoch_id_from_prev_block(parent_hash)?;

        let mut settlement =
            self.get_all_block_producers_settlement(&current_epoch_id, parent_hash)?.to_vec();

        let settlement_epoch_boundary = settlement.len();

        let block_info = self.get_block_info(parent_hash)?;
        if self.next_block_need_approvals_from_next_epoch(&block_info)? {
            settlement.extend(
                self.get_all_block_producers_settlement(&next_epoch_id, parent_hash)?
                    .iter()
                    .cloned(),
            );
        }

        let mut result = vec![];
        let mut validators: HashMap<AccountId, usize> = HashMap::default();
        for (ord, (validator_stake, is_slashed)) in settlement.into_iter().enumerate() {
            let account_id = validator_stake.account_id();
            match validators.get(account_id) {
                None => {
                    validators.insert(account_id.clone(), result.len());
                    result.push((
                        validator_stake.get_approval_pledge(ord >= settlement_epoch_boundary),
                        is_slashed,
                    ));
                }
                Some(old_ord) => {
                    if ord >= settlement_epoch_boundary {
                        result[*old_ord].0.pledge_next_epoch = validator_stake.pledge();
                    };
                }
            };
        }
        Ok(result)
    }

    /// For given epoch_id, height and shard_id returns validator that is chunk producer.
    pub fn get_chunk_producer_info(
        &self,
        epoch_id: &EpochId,
        height: BlockHeight,
        shard_id: ShardId,
    ) -> Result<ValidatorPowerAndPledge, EpochError> {
        let epoch_info = self.get_epoch_info(epoch_id)?;
        let validator_id = Self::chunk_producer_from_info(&epoch_info, height, shard_id);
        Ok(epoch_info.get_validator(validator_id))
    }

    /// Returns validator for given account id for given epoch.
    /// We don't require caller to know about EpochIds. Doesn't account for slashing.
    pub fn get_validator_by_account_id(
        &self,
        epoch_id: &EpochId,
        account_id: &AccountId,
    ) -> Result<ValidatorPowerAndPledge, EpochError> {
        let epoch_info = self.get_epoch_info(epoch_id)?;
        epoch_info
            .get_validator_by_account(account_id)
            .ok_or_else(|| EpochError::NotAValidator(account_id.clone(), epoch_id.clone()))
    }

    /// Returns fisherman for given account id for given epoch.
    pub fn get_fisherman_by_account_id(
        &self,
        epoch_id: &EpochId,
        account_id: &AccountId,
    ) -> Result<ValidatorPowerAndPledge, EpochError> {
        let epoch_info = self.get_epoch_info(epoch_id)?;
        epoch_info
            .get_fisherman_by_account(account_id)
            .ok_or_else(|| EpochError::NotAValidator(account_id.clone(), epoch_id.clone()))
    }

    pub fn get_epoch_id(&self, block_hash: &CryptoHash) -> Result<EpochId, EpochError> {
        Ok(self.get_block_info(block_hash)?.epoch_id().clone())
    }

    pub fn get_next_epoch_id(&self, block_hash: &CryptoHash) -> Result<EpochId, EpochError> {
        let block_info = self.get_block_info(block_hash)?;
        self.get_next_epoch_id_from_info(&block_info)
    }

    pub fn get_prev_epoch_id(&self, block_hash: &CryptoHash) -> Result<EpochId, EpochError> {
        let epoch_first_block = *self.get_block_info(block_hash)?.epoch_first_block();
        let prev_epoch_last_hash = *self.get_block_info(&epoch_first_block)?.prev_hash();
        self.get_epoch_id(&prev_epoch_last_hash)
    }

    pub fn get_epoch_info_from_hash(
        &self,
        block_hash: &CryptoHash,
    ) -> Result<Arc<EpochInfo>, EpochError> {
        let epoch_id = self.get_epoch_id(block_hash)?;
        self.get_epoch_info(&epoch_id)
    }

    pub fn cares_about_shard_from_prev_block(
        &self,
        parent_hash: &CryptoHash,
        account_id: &AccountId,
        shard_id: ShardId,
    ) -> Result<bool, EpochError> {
        let epoch_id = self.get_epoch_id_from_prev_block(parent_hash)?;
        self.cares_about_shard_in_epoch(epoch_id, account_id, shard_id)
    }

    // `shard_id` always refers to a shard in the current epoch that the next block from `parent_hash` belongs
    // If shard layout will change next epoch, returns true if it cares about any shard
    // that `shard_id` will split to
    pub fn cares_about_shard_next_epoch_from_prev_block(
        &self,
        parent_hash: &CryptoHash,
        account_id: &AccountId,
        shard_id: ShardId,
    ) -> Result<bool, EpochError> {
        let next_epoch_id = self.get_next_epoch_id_from_prev_block(parent_hash)?;
        if self.will_shard_layout_change(parent_hash)? {
            let shard_layout = self.get_shard_layout(&next_epoch_id)?;
            let split_shards = shard_layout
                .get_children_shards_ids(shard_id)
                .expect("all shard layouts expect the first one must have a split map");
            for next_shard_id in split_shards {
                if self.cares_about_shard_in_epoch(
                    next_epoch_id.clone(),
                    account_id,
                    next_shard_id,
                )? {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            self.cares_about_shard_in_epoch(next_epoch_id, account_id, shard_id)
        }
    }

    /// Returns true if next block after given block hash is in the new epoch.
    pub fn is_next_block_epoch_start(&self, parent_hash: &CryptoHash) -> Result<bool, EpochError> {
        let block_info = self.get_block_info(parent_hash)?;
        self.is_next_block_in_next_epoch(&block_info)
    }

    /// Relies on the fact that last block hash of an epoch is an EpochId of next next epoch.
    /// If this block is the last one in some epoch, and we fully processed it, there will be `EpochInfo` record with `hash` key.
    fn is_last_block_in_finished_epoch(&self, hash: &CryptoHash) -> Result<bool, EpochError> {
        match self.get_epoch_info(&EpochId(*hash)) {
            Ok(_) => Ok(true),
            Err(EpochError::IOErr(msg)) => Err(EpochError::IOErr(msg)),
            Err(EpochError::MissingBlock(_)) => Ok(false),
            Err(err) => {
                warn!(target: "epoch_manager", ?err, "Unexpected error in is_last_block_in_finished_epoch");
                Ok(false)
            }
        }
    }

    pub fn get_epoch_id_from_prev_block(
        &self,
        parent_hash: &CryptoHash,
    ) -> Result<EpochId, EpochError> {
        if self.is_next_block_epoch_start(parent_hash)? {
            self.get_next_epoch_id(parent_hash)
        } else {
            self.get_epoch_id(parent_hash)
        }
    }

    pub fn get_next_epoch_id_from_prev_block(
        &self,
        parent_hash: &CryptoHash,
    ) -> Result<EpochId, EpochError> {
        if self.is_next_block_epoch_start(parent_hash)? {
            // Because we ID epochs based on the last block of T - 2, this is ID for next next epoch.
            Ok(EpochId(*parent_hash))
        } else {
            self.get_next_epoch_id(parent_hash)
        }
    }

    pub fn get_epoch_start_height(
        &self,
        block_hash: &CryptoHash,
    ) -> Result<BlockHeight, EpochError> {
        let epoch_first_block = *self.get_block_info(block_hash)?.epoch_first_block();
        Ok(self.get_block_info(&epoch_first_block)?.height())
    }

    /// Compute pledge return info based on the last block hash of the epoch that is just finalized
    /// return the hashmap of account id to max_of_pledges, which is used in the calculation of account
    /// updates.
    ///
    /// # Returns
    /// If successful, a triple of (hashmap of account id to max of pledges in the past three epochs,
    /// validator rewards in the last epoch, double sign slashing for the past epoch).
    pub fn compute_power_return_info_for_block(
        &self,
        last_block_hash: &CryptoHash,
    ) -> Result<
        (
            HashMap<AccountId, Power>,
            HashMap<AccountId, Balance>,
            HashMap<AccountId, Balance>,
            HashMap<AccountId, Balance>,
        ),
        EpochError,
    > {
        let last_block_info = self.get_block_info(last_block_hash)?;
        let validator_reward = last_block_info.validator_reward().clone();
        // Fetch last block info to get the slashed accounts.

        // Since pledge changes for epoch T are stored in epoch info for T+2, the one stored by epoch_id
        // is the prev_prev_pledge_change.
        let pledge_change = last_block_info.pledge_change().clone();
        // Power changes are similar like pledge changes
        let power_change = last_block_info.power_change().clone();

        let all_power_changes = power_change.iter();
        let all_power_keys: HashSet<&AccountId> = all_power_changes.map(|(key, _)| key).collect();

        let mut power_info = HashMap::new();
        for account_id in all_power_keys {
            let new_power = *power_change.get(account_id).unwrap_or(&0);
            power_info.insert(account_id.clone(), new_power);
        }

        let all_pledge_changes = pledge_change.iter();
        let all_pledge_keys: HashSet<&AccountId> = all_pledge_changes.map(|(key, _)| key).collect();

        let mut pledge_info = HashMap::new();
        for account_id in all_pledge_keys {
            if last_block_info.slashed().contains_key(account_id) {
                if !pledge_change.contains_key(account_id) {
                    // slashed in prev_prev epoch so it is safe to return the remaining pledge in case of
                    // a double sign without violating the staking invariant.
                } else {
                    continue;
                }
            }
            let new_pledge = *pledge_change.get(account_id).unwrap_or(&0);
            pledge_info.insert(account_id.clone(), new_pledge);
        }

        // let slashing_info = self.compute_double_sign_slashing_info(last_block_hash)?;
        let slashing_info = HashMap::default();
        debug!(target: "epoch_manager", "power_info: {:?}, pledge_info: {:?}, validator_reward: {:?}", power_info, pledge_info, validator_reward);
        Ok((power_info, pledge_info, validator_reward, slashing_info))
    }
    pub fn compute_power_return_info(
        &self,
        last_block_hash: &CryptoHash,
    ) -> Result<
        (
            HashMap<AccountId, Power>,
            HashMap<AccountId, Balance>,
            HashMap<AccountId, Balance>,
            HashMap<AccountId, Balance>,
        ),
        EpochError,
    > {
        let next_next_epoch_id = EpochId(*last_block_hash);
        let validator_reward = self.get_epoch_info(&next_next_epoch_id)?.validator_reward().clone();

        let next_epoch_id = self.get_next_epoch_id(last_block_hash)?;
        let epoch_id = self.get_epoch_id(last_block_hash)?;
        debug!(target: "epoch_manager",
            "epoch id: {:?}, prev_epoch_id: {:?}, prev_prev_epoch_id: {:?}",
            next_next_epoch_id, next_epoch_id, epoch_id
        );
        // Fetch last block info to get the slashed accounts.
        let last_block_info = self.get_block_info(last_block_hash)?;
        // Since pledge changes for epoch T are stored in epoch info for T+2, the one stored by epoch_id
        // is the prev_prev_pledge_change.
        let prev_prev_pledge_change = self.get_epoch_info(&epoch_id)?.pledge_change().clone();
        let prev_pledge_change = self.get_epoch_info(&next_epoch_id)?.pledge_change().clone();
        let pledge_change = self.get_epoch_info(&next_next_epoch_id)?.pledge_change().clone();
        // Power changes are similar like pledge changes
        let prev_prev_power_change = self.get_epoch_info(&epoch_id)?.power_change().clone();
        let prev_power_change = self.get_epoch_info(&next_epoch_id)?.power_change().clone();
        let power_change = self.get_epoch_info(&next_next_epoch_id)?.power_change().clone();

        debug!(target: "epoch_manager",
            "prev_prev_power_change: {:?}, prev_power_change: {:?}, power_change: {:?}, slashed: {:?},
             prev_prev_pledge_change: {:?}, prev_pledge_change: {:?}, pledge_change: {:?}",
            prev_prev_power_change, prev_power_change, power_change, last_block_info.slashed(),
            prev_prev_pledge_change, prev_pledge_change, pledge_change
        );
        let all_power_changes =
            prev_prev_power_change.iter().chain(&prev_power_change).chain(&power_change);
        let all_power_keys: HashSet<&AccountId> = all_power_changes.map(|(key, _)| key).collect();

        let mut power_info = HashMap::new();
        for account_id in all_power_keys {
            let new_power = *power_change.get(account_id).unwrap_or(&0);
            let prev_power = *prev_power_change.get(account_id).unwrap_or(&0);
            let prev_prev_power = *prev_prev_power_change.get(account_id).unwrap_or(&0);
            let max_of_power =
                vec![prev_prev_power, prev_power, new_power].into_iter().max().unwrap();
            power_info.insert(account_id.clone(), max_of_power);
        }

        let all_pledge_changes =
            prev_prev_pledge_change.iter().chain(&prev_pledge_change).chain(&pledge_change);
        let all_pledge_keys: HashSet<&AccountId> = all_pledge_changes.map(|(key, _)| key).collect();

        let mut pledge_info = HashMap::new();
        for account_id in all_pledge_keys {
            if last_block_info.slashed().contains_key(account_id) {
                if prev_prev_pledge_change.contains_key(account_id)
                    && !prev_pledge_change.contains_key(account_id)
                    && !pledge_change.contains_key(account_id)
                {
                    // slashed in prev_prev epoch so it is safe to return the remaining pledge in case of
                    // a double sign without violating the staking invariant.
                } else {
                    continue;
                }
            }
            let new_pledge = *pledge_change.get(account_id).unwrap_or(&0);
            let prev_pledge = *prev_pledge_change.get(account_id).unwrap_or(&0);
            let prev_prev_pledge = *prev_prev_pledge_change.get(account_id).unwrap_or(&0);
            let max_of_pledge =
                vec![prev_prev_pledge, prev_pledge, new_pledge].into_iter().max().unwrap();
            pledge_info.insert(account_id.clone(), max_of_pledge);
        }

        let slashing_info = self.compute_double_sign_slashing_info(last_block_hash)?;
        debug!(target: "epoch_manager", "power_info: {:?}, pledge_info: {:?}, validator_reward: {:?}", power_info, pledge_info, validator_reward);
        Ok((power_info, pledge_info, validator_reward, slashing_info))
    }

    /// Compute slashing information. Returns a hashmap of account id to slashed amount for double sign
    /// slashing.
    fn compute_double_sign_slashing_info(
        &self,
        last_block_hash: &CryptoHash,
    ) -> Result<HashMap<AccountId, Balance>, EpochError> {
        let last_block_info = self.get_block_info(last_block_hash)?;
        let epoch_id = self.get_epoch_id(last_block_hash)?;
        let epoch_info = self.get_epoch_info(&epoch_id)?;
        let total_pledge: Balance = epoch_info.validators_iter().map(|v| v.pledge()).sum();
        let total_slashed_pledge: Balance = last_block_info
            .slashed()
            .iter()
            .filter_map(|(account_id, slashed)| match slashed {
                SlashState::DoubleSign => Some(
                    epoch_info
                        .get_validator_id(account_id)
                        .map_or(0, |id| epoch_info.validator_stake(*id)),
                ),
                _ => None,
            })
            .sum();
        let is_totally_slashed = total_slashed_pledge * 3 >= total_pledge;
        let mut res = HashMap::default();
        for (account_id, slash_state) in last_block_info.slashed() {
            if let SlashState::DoubleSign = slash_state {
                if let Some(&idx) = epoch_info.get_validator_id(account_id) {
                    let pledge = epoch_info.validator_stake(idx);
                    let slashed_pledge = if is_totally_slashed {
                        pledge
                    } else {
                        let pledge = U256::from(pledge);
                        // 3 * (total_slashed_pledge / total_pledge) * pledge
                        (U256::from(3) * U256::from(total_slashed_pledge) * pledge
                            / U256::from(total_pledge))
                        .as_u128()
                    };
                    res.insert(account_id.clone(), slashed_pledge);
                }
            }
        }
        Ok(res)
    }

    /// Get validators for current epoch and next epoch.
    /// WARNING: this function calls EpochManager::get_epoch_info_aggregator_upto_last
    /// underneath which can be very expensive.
    pub fn get_validator_info(
        &self,
        epoch_identifier: ValidatorInfoIdentifier,
    ) -> Result<EpochValidatorInfo, EpochError> {
        let epoch_id = match epoch_identifier {
            ValidatorInfoIdentifier::EpochId(ref id) => id.clone(),
            ValidatorInfoIdentifier::BlockHash(ref b) => self.get_epoch_id(b)?,
        };
        let cur_epoch_info = self.get_epoch_info(&epoch_id)?;
        let epoch_height = cur_epoch_info.epoch_height();
        let epoch_start_height = self.get_epoch_start_from_epoch_id(&epoch_id)?;
        let mut validator_to_shard = (0..cur_epoch_info.validators_len())
            .map(|_| HashSet::default())
            .collect::<Vec<HashSet<ShardId>>>();
        for (shard_id, validators) in
            cur_epoch_info.chunk_producers_settlement().into_iter().enumerate()
        {
            for validator_id in validators {
                validator_to_shard[*validator_id as usize].insert(shard_id as ShardId);
            }
        }

        // This ugly code arises because of the incompatible types between `block_tracker` in `EpochInfoAggregator`
        // and `validator_block_chunk_stats` in `EpochSummary`. Rust currently has no support for Either type
        // in std.
        let (current_validators, next_epoch_id, all_power_proposals, all_pledge_proposals) =
            match &epoch_identifier {
                ValidatorInfoIdentifier::EpochId(id) => {
                    let epoch_summary = self.get_epoch_validator_info(id)?;
                    let cur_validators = cur_epoch_info
                        .validators_iter()
                        .enumerate()
                        .map(|(validator_id, info)| {
                            let validator_stats = epoch_summary
                                .validator_block_chunk_stats
                                .get(info.account_id())
                                .unwrap_or(&BlockChunkValidatorStats {
                                    block_stats: ValidatorStats { produced: 0, expected: 0 },
                                    chunk_stats: ValidatorStats { produced: 0, expected: 0 },
                                });
                            let mut shards = validator_to_shard[validator_id]
                                .iter()
                                .cloned()
                                .collect::<Vec<ShardId>>();
                            shards.sort();
                            let (account_id, public_key, power, pledge) = info.destructure();
                            Ok(CurrentEpochValidatorInfo {
                                is_slashed: false, // currently there is no slashing
                                account_id,
                                public_key,
                                power,
                                pledge,
                                // TODO: Maybe fill in the per shard info about the chunk produced for requests coming from RPC.
                                num_produced_chunks_per_shard: vec![0; shards.len()],
                                num_expected_chunks_per_shard: vec![0; shards.len()],
                                shards,
                                num_produced_blocks: validator_stats.block_stats.produced,
                                num_expected_blocks: validator_stats.block_stats.expected,
                                num_produced_chunks: validator_stats.chunk_stats.produced,
                                num_expected_chunks: validator_stats.chunk_stats.expected,
                            })
                        })
                        .collect::<Result<Vec<CurrentEpochValidatorInfo>, EpochError>>()?;
                    (
                        cur_validators,
                        EpochId(epoch_summary.prev_epoch_last_block_hash),
                        epoch_summary.all_power_proposals.into_iter().map(Into::into).collect(),
                        epoch_summary.all_pledge_proposals.into_iter().map(Into::into).collect(),
                    )
                }
                ValidatorInfoIdentifier::BlockHash(ref h) => {
                    // If we are here, `h` is hash of the latest block of the
                    // current epoch.
                    let aggregator = self.get_epoch_info_aggregator_upto_last(h)?;
                    let cur_validators = cur_epoch_info
                        .validators_iter()
                        .enumerate()
                        .map(|(validator_id, info)| {
                            let block_stats = aggregator
                                .block_tracker
                                .get(&(validator_id as u64))
                                .unwrap_or_else(|| &ValidatorStats { produced: 0, expected: 0 })
                                .clone();

                            let mut chunks_produced_by_shard: HashMap<ShardId, NumBlocks> =
                                HashMap::new();
                            let mut chunks_expected_by_shard: HashMap<ShardId, NumBlocks> =
                                HashMap::new();
                            let mut chunk_stats = ValidatorStats { produced: 0, expected: 0 };
                            for (shard, tracker) in aggregator.shard_tracker.iter() {
                                if let Some(stats) = tracker.get(&(validator_id as u64)) {
                                    chunk_stats.produced += stats.produced;
                                    chunk_stats.expected += stats.expected;
                                    *chunks_produced_by_shard.entry(*shard).or_insert(0) +=
                                        stats.produced;
                                    *chunks_expected_by_shard.entry(*shard).or_insert(0) +=
                                        stats.expected;
                                }
                            }
                            let mut shards = validator_to_shard[validator_id]
                                .clone()
                                .into_iter()
                                .collect::<Vec<ShardId>>();
                            shards.sort();
                            let (account_id, public_key, power, pledge) = info.destructure();
                            Ok(CurrentEpochValidatorInfo {
                                is_slashed: false, // currently there is no slashing
                                account_id,
                                public_key,
                                power,
                                pledge,
                                shards: shards.clone(),
                                num_produced_blocks: block_stats.produced,
                                num_expected_blocks: block_stats.expected,
                                num_produced_chunks: chunk_stats.produced,
                                num_expected_chunks: chunk_stats.expected,
                                num_produced_chunks_per_shard: shards
                                    .iter()
                                    .map(|shard| {
                                        *chunks_produced_by_shard.entry(*shard).or_default()
                                    })
                                    .collect(),
                                num_expected_chunks_per_shard: shards
                                    .iter()
                                    .map(|shard| {
                                        *chunks_expected_by_shard.entry(*shard).or_default()
                                    })
                                    .collect(),
                            })
                        })
                        .collect::<Result<Vec<CurrentEpochValidatorInfo>, EpochError>>()?;
                    let all_power_proposals = aggregator
                        .all_power_proposals
                        .iter()
                        .map(|(_, p)| p.clone().into())
                        .collect();
                    let all_pledge_proposals = aggregator
                        .all_pledge_proposals
                        .iter()
                        .map(|(_, p)| p.clone().into())
                        .collect();
                    let next_epoch_id = self.get_next_epoch_id(h)?;
                    (cur_validators, next_epoch_id, all_power_proposals, all_pledge_proposals)
                }
            };

        let next_epoch_info = self.get_epoch_info(&next_epoch_id)?;
        let mut next_validator_to_shard = (0..next_epoch_info.validators_len())
            .map(|_| HashSet::default())
            .collect::<Vec<HashSet<ShardId>>>();
        for (shard_id, validators) in
            next_epoch_info.chunk_producers_settlement().iter().enumerate()
        {
            for validator_id in validators {
                next_validator_to_shard[*validator_id as usize].insert(shard_id as u64);
            }
        }
        let next_validators = next_epoch_info
            .validators_iter()
            .enumerate()
            .map(|(validator_id, info)| {
                let mut shards = next_validator_to_shard[validator_id]
                    .clone()
                    .into_iter()
                    .collect::<Vec<ShardId>>();
                shards.sort();
                let (account_id, public_key, power, pledge) = info.destructure();
                NextEpochValidatorInfo { account_id, public_key, power, pledge, shards }
            })
            .collect();
        let prev_epoch_kickout = next_epoch_info
            .validator_kickout()
            .clone()
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(account_id, reason)| ValidatorKickoutView { account_id, reason })
            .collect();

        Ok(EpochValidatorInfo {
            current_validators,
            next_validators,
            current_fishermen: cur_epoch_info.fishermen_iter().map(Into::into).collect(),
            next_fishermen: next_epoch_info.fishermen_iter().map(Into::into).collect(),
            current_power_proposals: all_power_proposals,
            current_pledge_proposals: all_pledge_proposals,
            prev_epoch_kickout,
            epoch_start_height,
            epoch_height,
        })
    }

    #[allow(dead_code)]
    pub fn add_validator_proposals(
        &mut self,
        block_header_info: BlockHeaderInfo,
    ) -> Result<StoreUpdate, EpochError> {
        // Check that genesis block doesn't have any proposals.
        assert!(
            block_header_info.height > 0
                || (block_header_info.power_proposals.is_empty()
                    && block_header_info.pledge_proposals.is_empty()
                    && block_header_info.slashed_validators.is_empty())
        );
        debug!(target: "epoch_manager",
            height = block_header_info.height,
            power_proposals = ?block_header_info.power_proposals,
            pledge_proposals = ?block_header_info.pledge_proposals,
            "add_validator_proposals");
        // Deal with validator proposals and epoch finishing.
        let block_info = BlockInfo::new(
            block_header_info.hash,
            block_header_info.height,
            block_header_info.last_finalized_height,
            block_header_info.last_finalized_block_hash,
            block_header_info.prev_hash,
            block_header_info.power_proposals,
            block_header_info.pledge_proposals,
            block_header_info.chunk_mask,
            block_header_info.slashed_validators,
            block_header_info.total_supply,
            block_header_info.latest_protocol_version,
            block_header_info.timestamp_nanosec,
            Default::default(),
            vec![],
            Default::default(),
            vec![],
            vec![],
            vec![],
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            0,
            0,
            vec![],
            vec![],
            Default::default(),
            Default::default(),
        );
        let rng_seed = block_header_info.random_value.0;
        self.record_block_info(block_info, rng_seed)
    }

    #[allow(dead_code)]
    pub fn add_validator_proposals_for_blocks(
        &mut self,
        block_header_info: BlockHeaderInfo,
    ) -> Result<StoreUpdate, EpochError> {
        // Check that genesis block doesn't have any proposals.
        assert!(
            block_header_info.height > 0
                || (block_header_info.power_proposals.is_empty()
                    && block_header_info.pledge_proposals.is_empty()
                    && block_header_info.slashed_validators.is_empty())
        );
        debug!(target: "epoch_manager",
            height = block_header_info.height,
            power_proposals = ?block_header_info.power_proposals,
            pledge_proposals = ?block_header_info.pledge_proposals,
            "add_validator_proposals");
        let rng_seed = block_header_info.random_value.0;
        // start customized by James Savechives
        let BlockSummary::V1(BlockSummaryV1 {
            random_value: _random_value,
            validators,
            validator_to_index,
            block_producers_settlement,
            chunk_producers_settlement,
            fishermen,
            fishermen_to_index,
            power_change,
            pledge_change,
            validator_reward,
            seat_price,
            minted_amount,
            all_power_proposals,
            all_pledge_proposals,
            validator_kickout,
            validator_mandates,
            ..
        }) = if block_header_info.hash == CryptoHash::default() {
            BlockSummary::default()
        } else {
            let BlockInfo::V2(BlockInfoV2 {
                validators,
                validator_to_index,
                block_producers_settlement,
                chunk_producers_settlement,
                fishermen,
                fishermen_to_index,
                power_change,
                pledge_change,
                validator_reward,
                seat_price,
                minted_amount,
                all_power_proposals,
                all_pledge_proposals,
                validator_kickout,
                validator_mandates,
                ..
            }) = &*self.get_block_info(&block_header_info.prev_hash)?
            else {
                todo!()
            };
            let all_power_proposals: Vec<_> = remove_duplicate_power_proposals(
                all_power_proposals
                    .clone()
                    .into_iter()
                    .chain(block_header_info.power_proposals.clone().into_iter())
                    .collect(),
            );
            let all_pledge_proposals: Vec<_> = remove_duplicate_pledge_proposals(
                all_pledge_proposals
                    .clone()
                    .into_iter()
                    .chain(block_header_info.pledge_proposals.clone().into_iter())
                    .collect(),
            );

            let block_info = BlockInfo::new(
                block_header_info.hash,
                block_header_info.height,
                block_header_info.last_finalized_height,
                block_header_info.last_finalized_block_hash,
                block_header_info.prev_hash,
                block_header_info.power_proposals.clone(),
                block_header_info.pledge_proposals.clone(),
                block_header_info.chunk_mask.clone(),
                block_header_info.slashed_validators.clone(),
                block_header_info.total_supply,
                block_header_info.latest_protocol_version,
                block_header_info.timestamp_nanosec,
                // start customized by James Savechives
                block_header_info.random_value,
                validators.clone(),
                validator_to_index.clone(),
                block_producers_settlement.clone(),
                chunk_producers_settlement.clone(),
                fishermen.clone(),
                fishermen_to_index.clone(),
                power_change.clone(),
                pledge_change.clone(),
                validator_reward.clone(),
                *seat_price,
                *minted_amount,
                all_power_proposals,
                all_pledge_proposals,
                validator_kickout.clone(),
                validator_mandates.clone(),
                // end customized by James Savechives
            );
            self.finalize_block_summary_for_block(
                &block_info,
                &block_header_info.prev_hash,
                rng_seed,
            )?
        };

        // end customized by James Savechives
        // Deal with validator proposals and epoch finishing.
        let block_info = BlockInfo::new(
            block_header_info.hash,
            block_header_info.height,
            block_header_info.last_finalized_height,
            block_header_info.last_finalized_block_hash,
            block_header_info.prev_hash,
            block_header_info.power_proposals,
            block_header_info.pledge_proposals.clone(),
            block_header_info.chunk_mask,
            block_header_info.slashed_validators,
            block_header_info.total_supply,
            block_header_info.latest_protocol_version,
            block_header_info.timestamp_nanosec,
            // start customized by James Savechives
            block_header_info.random_value,
            validators.clone(),
            validator_to_index,
            block_producers_settlement.clone(),
            chunk_producers_settlement,
            fishermen,
            fishermen_to_index,
            power_change,
            pledge_change,
            validator_reward,
            seat_price,
            minted_amount,
            all_power_proposals,
            all_pledge_proposals,
            validator_kickout,
            validator_mandates, // end customized by James Savechives
        );

        debug!(target: "epoch_manager", "the random value is: {:?}, the validators value is: {:?}, the block producers settlement from block_info is: {:?}",
        block_header_info.random_value,
        validators,
        block_producers_settlement);

        self.record_block_info(block_info, rng_seed)
    }

    /// Compare two epoch ids based on their start height. This works because finality gadget
    /// guarantees that we cannot have two different epochs on two forks
    pub fn compare_epoch_id(
        &self,
        epoch_id: &EpochId,
        other_epoch_id: &EpochId,
    ) -> Result<Ordering, EpochError> {
        if epoch_id.0 == other_epoch_id.0 {
            return Ok(Ordering::Equal);
        }
        match (
            self.get_epoch_start_from_epoch_id(epoch_id),
            self.get_epoch_start_from_epoch_id(other_epoch_id),
        ) {
            (Ok(index1), Ok(index2)) => Ok(index1.cmp(&index2)),
            (Ok(_), Err(_)) => self.get_epoch_info(other_epoch_id).map(|_| Ordering::Less),
            (Err(_), Ok(_)) => self.get_epoch_info(epoch_id).map(|_| Ordering::Greater),
            (Err(_), Err(_)) => Err(EpochError::EpochOutOfBounds(epoch_id.clone())), // other_epoch_id may be out of bounds as well
        }
    }

    /// Get minimum pledge allowed at current block. Attempts to pledge with a lower pledge will be
    /// rejected.
    pub fn minimum_pledge(&self, prev_block_hash: &CryptoHash) -> Result<Balance, EpochError> {
        let next_epoch_id = self.get_next_epoch_id_from_prev_block(prev_block_hash)?;
        let (protocol_version, seat_price) = {
            let epoch_info = self.get_epoch_info(&next_epoch_id)?;
            (epoch_info.protocol_version(), epoch_info.seat_price())
        };
        let config = self.config.for_protocol_version(protocol_version);
        let pledge_divisor = { config.minimum_pledge_divisor as Balance };
        Ok(seat_price / pledge_divisor)
    }

    /// Get minimum power allowed at current block. Attempts to pledge with a lower power will be
    /// rejected.
    pub fn minimum_power(&self, _prev_block_hash: &CryptoHash) -> Result<Power, EpochError> {
        // To Do
        Ok(0)
    }
}

fn remove_duplicate_power_proposals(power_proposals: Vec<ValidatorPower>) -> Vec<ValidatorPower> {
    let mut unique_proposals_map: HashMap<_, _> = HashMap::new();

    for proposal in power_proposals {
        // Use account_id as the key for uniqueness.
        // This overwrites any existing entry with the same account_id, effectively keeping only the last seen instance.
        unique_proposals_map.insert(proposal.account_id().clone(), proposal);
    }

    // Extract the values (ValidatorPower instances) into a new Vec
    let unique_proposals = unique_proposals_map.into_values().collect::<Vec<_>>();
    unique_proposals
}

fn remove_duplicate_pledge_proposals(
    pledge_proposals: Vec<ValidatorPledge>,
) -> Vec<ValidatorPledge> {
    let mut unique_proposals_map: HashMap<_, _> = HashMap::new();

    for proposal in pledge_proposals {
        // Use account_id as the key for uniqueness.
        // This overwrites any existing entry with the same account_id, effectively keeping only the last seen instance.
        unique_proposals_map.insert(proposal.account_id().clone(), proposal);
    }

    // Extract the values (ValidatorPower instances) into a new Vec
    let unique_proposals = unique_proposals_map.into_values().collect::<Vec<_>>();
    unique_proposals
}

/// Private utilities for EpochManager.
impl EpochManager {
    fn cares_about_shard_in_epoch(
        &self,
        epoch_id: EpochId,
        account_id: &AccountId,
        shard_id: ShardId,
    ) -> Result<bool, EpochError> {
        let epoch_info = self.get_epoch_info(&epoch_id)?;
        let chunk_producers = epoch_info.chunk_producers_settlement();
        for validator_id in chunk_producers[shard_id as usize].iter() {
            if epoch_info.validator_account_id(*validator_id) == account_id {
                return Ok(true);
            }
        }
        Ok(false)
    }
    // #[inline]
    // pub(crate) fn block_producer_from_info_vrf(
    //     epoch_info: &EpochInfo,
    //     random_value: &CryptoHash,
    // ) -> ValidatorId { epoch_info.vrf_block_producer(random_value) }
    #[inline]
    pub(crate) fn block_producer_from_info(
        epoch_info: &EpochInfo,
        height: BlockHeight,
    ) -> ValidatorId {
        epoch_info.sample_block_producer(height)
    }

    #[inline]
    pub(crate) fn chunk_producer_from_info(
        epoch_info: &EpochInfo,
        height: BlockHeight,
        shard_id: ShardId,
    ) -> ValidatorId {
        epoch_info.sample_chunk_producer(height, shard_id)
    }

    /// Returns true, if given current block info, next block supposed to be in the next epoch.
    fn is_next_block_in_next_epoch(&self, block_info: &BlockInfo) -> Result<bool, EpochError> {
        if block_info.prev_hash() == &CryptoHash::default() {
            return Ok(true);
        }
        let protocol_version = self.get_epoch_info_from_hash(block_info.hash())?.protocol_version();
        let epoch_length = self.config.for_protocol_version(protocol_version).epoch_length;
        let estimated_next_epoch_start =
            self.get_block_info(block_info.epoch_first_block())?.height() + epoch_length;

        if epoch_length <= 3 {
            // This is here to make epoch_manager tests pass. Needs to be removed, tracked in
            // https://github.com/utility/unc/issues/2522
            return Ok(block_info.height() + 1 >= estimated_next_epoch_start);
        }

        Ok(block_info.last_finalized_height() + 3 >= estimated_next_epoch_start)
    }

    /// Returns true, if given current block info, next block must include the approvals from the next
    /// epoch (in addition to the approvals from the current epoch)
    fn next_block_need_approvals_from_next_epoch(
        &self,
        block_info: &BlockInfo,
    ) -> Result<bool, EpochError> {
        if self.is_next_block_in_next_epoch(block_info)? {
            return Ok(false);
        }
        let epoch_length = {
            let protocol_version =
                self.get_epoch_info_from_hash(block_info.hash())?.protocol_version();
            let config = self.config.for_protocol_version(protocol_version);
            config.epoch_length
        };
        let estimated_next_epoch_start =
            self.get_block_info(block_info.epoch_first_block())?.height() + epoch_length;
        Ok(block_info.last_finalized_height() + 3 < estimated_next_epoch_start
            && block_info.height() + 3 >= estimated_next_epoch_start)
    }

    /// Returns epoch id for the next epoch (T+1), given an block info in current epoch (T).
    fn get_next_epoch_id_from_info(&self, block_info: &BlockInfo) -> Result<EpochId, EpochError> {
        let first_block_info = self.get_block_info(block_info.epoch_first_block())?;
        Ok(EpochId(*first_block_info.prev_hash()))
    }

    pub fn get_shard_config(&self, epoch_id: &EpochId) -> Result<ShardConfig, EpochError> {
        let protocol_version = self.get_epoch_info(epoch_id)?.protocol_version();
        let epoch_config = self.config.for_protocol_version(protocol_version);
        Ok(ShardConfig::new(epoch_config))
    }

    pub fn get_epoch_config(&self, epoch_id: &EpochId) -> Result<EpochConfig, EpochError> {
        let protocol_version = self.get_epoch_info(epoch_id)?.protocol_version();
        Ok(self.config.for_protocol_version(protocol_version))
    }

    pub fn get_shard_layout(&self, epoch_id: &EpochId) -> Result<ShardLayout, EpochError> {
        let protocol_version = self.get_epoch_info(epoch_id)?.protocol_version();
        let shard_layout = self.config.for_protocol_version(protocol_version).shard_layout;
        Ok(shard_layout)
    }

    pub fn will_shard_layout_change(&self, parent_hash: &CryptoHash) -> Result<bool, EpochError> {
        let epoch_id = self.get_epoch_id_from_prev_block(parent_hash)?;
        let next_epoch_id = self.get_next_epoch_id_from_prev_block(parent_hash)?;
        let shard_layout = self.get_shard_layout(&epoch_id)?;
        let next_shard_layout = self.get_shard_layout(&next_epoch_id)?;
        Ok(shard_layout != next_shard_layout)
    }

    pub fn get_epoch_info(&self, epoch_id: &EpochId) -> Result<Arc<EpochInfo>, EpochError> {
        self.epochs_info.get_or_try_put(epoch_id.clone(), |epoch_id| {
            self.store
                .get_ser(DBCol::EpochInfo, epoch_id.as_ref())?
                .ok_or_else(|| EpochError::EpochOutOfBounds(epoch_id.clone()))
        })
    }

    fn has_epoch_info(&self, epoch_id: &EpochId) -> Result<bool, EpochError> {
        match self.get_epoch_info(epoch_id) {
            Ok(_) => Ok(true),
            Err(EpochError::EpochOutOfBounds(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn save_epoch_info(
        &mut self,
        store_update: &mut StoreUpdate,
        epoch_id: &EpochId,
        epoch_info: Arc<EpochInfo>,
    ) -> Result<(), EpochError> {
        store_update.set_ser(DBCol::EpochInfo, epoch_id.as_ref(), &epoch_info)?;
        self.epochs_info.put(epoch_id.clone(), epoch_info);
        Ok(())
    }

    pub fn get_epoch_validator_info(&self, epoch_id: &EpochId) -> Result<EpochSummary, EpochError> {
        // We don't use cache here since this query happens rarely and only for rpc.
        self.store
            .get_ser(DBCol::EpochValidatorInfo, epoch_id.as_ref())?
            .ok_or_else(|| EpochError::EpochOutOfBounds(epoch_id.clone()))
    }

    // Note(#6572): beware, after calling `save_epoch_validator_info`,
    // `get_epoch_validator_info` will return stale results.
    fn save_epoch_validator_info(
        &self,
        store_update: &mut StoreUpdate,
        epoch_id: &EpochId,
        epoch_summary: &EpochSummary,
    ) -> Result<(), EpochError> {
        store_update
            .set_ser(DBCol::EpochValidatorInfo, epoch_id.as_ref(), epoch_summary)
            .map_err(EpochError::from)
    }

    fn has_block_info(&self, hash: &CryptoHash) -> Result<bool, EpochError> {
        match self.get_block_info(hash) {
            Ok(_) => Ok(true),
            Err(EpochError::MissingBlock(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Get BlockInfo for a block
    /// # Errors
    /// EpochError::IOErr if storage returned an error
    /// EpochError::MissingBlock if block is not in storage
    pub fn get_block_info(&self, hash: &CryptoHash) -> Result<Arc<BlockInfo>, EpochError> {
        self.blocks_info.get_or_try_put(*hash, |hash| {
            self.store
                .get_ser(DBCol::BlockInfo, hash.as_ref())?
                .ok_or_else(|| EpochError::MissingBlock(*hash))
                .map(Arc::new)
        })
    }

    fn save_block_info(
        &mut self,
        store_update: &mut StoreUpdate,
        block_info: Arc<BlockInfo>,
    ) -> Result<(), EpochError> {
        let block_hash = *block_info.hash();
        store_update
            .insert_ser(DBCol::BlockInfo, block_hash.as_ref(), &block_info)
            .map_err(EpochError::from)?;
        self.blocks_info.put(block_hash, block_info);
        Ok(())
    }

    fn save_epoch_start(
        &mut self,
        store_update: &mut StoreUpdate,
        epoch_id: &EpochId,
        epoch_start: BlockHeight,
    ) -> Result<(), EpochError> {
        store_update
            .set_ser(DBCol::EpochStart, epoch_id.as_ref(), &epoch_start)
            .map_err(EpochError::from)?;
        self.epoch_id_to_start.put(epoch_id.clone(), epoch_start);
        Ok(())
    }

    fn get_epoch_start_from_epoch_id(&self, epoch_id: &EpochId) -> Result<BlockHeight, EpochError> {
        self.epoch_id_to_start.get_or_try_put(epoch_id.clone(), |epoch_id| {
            self.store
                .get_ser(DBCol::EpochStart, epoch_id.as_ref())?
                .ok_or_else(|| EpochError::EpochOutOfBounds(epoch_id.clone()))
        })
    }

    /// Updates epoch info aggregator to state as of `last_final_block_hash`
    /// block.
    ///
    /// The block hash passed as argument should be a final block so that the
    /// method can perform efficient incremental updates.  Calling this method
    /// on a block which has not been finalised yet is likely to result in
    /// performance issues since handling forks will force it to traverse the
    /// entire epoch from scratch.
    ///
    /// The result of the aggregation is stored in `self.epoch_info_aggregator`.
    ///
    /// Saves the aggregator to `store_update` if epoch id changes or every
    /// [`AGGREGATOR_SAVE_PERIOD`] heights.
    pub fn update_epoch_info_aggregator_upto_final(
        &mut self,
        last_final_block_hash: &CryptoHash,
        store_update: &mut StoreUpdate,
    ) -> Result<(), EpochError> {
        if let Some((aggregator, replace)) =
            self.aggregate_epoch_info_upto(last_final_block_hash)?
        {
            let save = if replace {
                self.epoch_info_aggregator = aggregator;
                true
            } else {
                self.epoch_info_aggregator.merge(aggregator);
                let block_info = self.get_block_info(last_final_block_hash)?;
                block_info.height() % AGGREGATOR_SAVE_PERIOD == 0
            };
            if save {
                store_update.set_ser(
                    DBCol::EpochInfo,
                    AGGREGATOR_KEY,
                    &self.epoch_info_aggregator,
                )?;
            }
        }
        Ok(())
    }

    /// Returns epoch info aggregate with state up to `last_block_hash`.
    ///
    /// The block hash passed as argument should be the latest block belonging
    /// to current epoch.  Calling this method on any other block is likely to
    /// result in performance issues since handling something which is not past
    /// the final block will force it to traverse the entire epoch from scratch.
    ///
    /// This method does not change `self.epoch_info_aggregator`.
    pub fn get_epoch_info_aggregator_upto_last(
        &self,
        last_block_hash: &CryptoHash,
    ) -> Result<EpochInfoAggregator, EpochError> {
        if let Some((mut aggregator, replace)) = self.aggregate_epoch_info_upto(last_block_hash)? {
            if !replace {
                aggregator.merge_prefix(&self.epoch_info_aggregator);
            }
            Ok(aggregator)
        } else {
            Ok(self.epoch_info_aggregator.clone())
        }
    }

    /// Aggregates epoch info between last final block and given block.
    ///
    /// More specifically, aggregates epoch information from block denoted by
    /// `self.epoch_info_aggregator.last_block_hash` (excluding that block) up
    /// to one denoted by `block_hash` (including that block).  If the two
    /// blocks belong to different epochs, stops aggregating once it reaches
    /// start of epoch `block_hash` belongs to.
    ///
    /// The block hash passed as argument should be a latest final block or
    /// a descendant of a latest final block. Calling this method on any other
    /// block is likely to result in performance issues since handling forks
    /// will force it to traverse the entire epoch from scratch.
    ///
    /// If `block_hash` equals `self.epoch_info_aggregator.last_block_hash`
    /// returns None.  Otherwise returns `Some((aggregator, full_info))` tuple.
    /// The first element of the pair is aggregator with collected information;
    /// the second specifies whether the returned aggregator includes full
    /// information about an epoch (such that it does not need to be merged with
    /// `self.epoch_info_aggregator`).  That happens if the method reaches epoch
    /// boundary.
    fn aggregate_epoch_info_upto(
        &self,
        block_hash: &CryptoHash,
    ) -> Result<Option<(EpochInfoAggregator, bool)>, EpochError> {
        if block_hash == &self.epoch_info_aggregator.last_block_hash {
            return Ok(None);
        }

        if cfg!(debug) {
            let agg_hash = self.epoch_info_aggregator.last_block_hash;
            let agg_height = self.get_block_info(&agg_hash)?.height();
            let block_height = self.get_block_info(block_hash)?.height();
            assert!(
                agg_height < block_height,
                "#{agg_hash} {agg_height} >= #{block_hash} {block_height}",
            );
        }

        let epoch_id = self.get_block_info(block_hash)?.epoch_id().clone();
        let epoch_info = self.get_epoch_info(&epoch_id)?;

        let mut aggregator = EpochInfoAggregator::new(epoch_id.clone(), *block_hash);
        let mut cur_hash = *block_hash;
        Ok(Some(loop {
            #[cfg(test)]
            {
                self.epoch_info_aggregator_loop_counter
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }

            // To avoid cloning BlockInfo we need to first get reference to the
            // current block, but then drop it so that we can call
            // get_block_info for previous block.
            let block_info = self.get_block_info(&cur_hash)?;
            let prev_hash = *block_info.prev_hash();
            let different_epoch = &epoch_id != block_info.epoch_id();

            if different_epoch || prev_hash == CryptoHash::default() {
                // We’ve reached the beginning of an epoch or a genesis block
                // without seeing self.epoch_info_aggregator.last_block_hash.
                // This implies self.epoch_info_aggregator.last_block_hash
                // belongs to different epoch or we’re on different fork (though
                // the latter should never happen).  In either case, the
                // aggregator contains full epoch information.
                break (aggregator, true);
            }

            let prev_info = self.get_block_info(&prev_hash)?;
            let prev_height = prev_info.height();
            let prev_epoch = prev_info.epoch_id().clone();

            let block_info = self.get_block_info(&cur_hash)?;
            aggregator.update_tail(&block_info, &epoch_info, prev_height);

            if prev_hash == self.epoch_info_aggregator.last_block_hash {
                // We’ve reached sync point of the old aggregator.  If old
                // aggregator was for a different epoch, we have full info in
                // our aggregator; otherwise we don’t.
                break (aggregator, epoch_id != prev_epoch);
            }

            cur_hash = prev_hash;
        }))
    }

    pub fn get_protocol_upgrade_block_height(
        &self,
        block_hash: CryptoHash,
    ) -> Result<Option<BlockHeight>, EpochError> {
        let cur_epoch_info = self.get_epoch_info_from_hash(&block_hash)?;
        let next_epoch_id = self.get_next_epoch_id(&block_hash)?;
        let next_epoch_info = self.get_epoch_info(&next_epoch_id)?;
        if cur_epoch_info.protocol_version() != next_epoch_info.protocol_version() {
            let block_info = self.get_block_info(&block_hash)?;
            let epoch_length =
                self.config.for_protocol_version(cur_epoch_info.protocol_version()).epoch_length;
            let estimated_next_epoch_start =
                self.get_block_info(block_info.epoch_first_block())?.height() + epoch_length;

            Ok(Some(estimated_next_epoch_start))
        } else {
            Ok(None)
        }
    }

    #[cfg(feature = "new_epoch_sync")]
    pub fn get_all_epoch_hashes_from_db(
        &self,
        last_block_info: &BlockInfo,
    ) -> Result<Vec<CryptoHash>, EpochError> {
        let _span =
            tracing::debug_span!(target: "epoch_manager", "get_all_epoch_hashes_from_db", ?last_block_info)
                .entered();

        let mut result = vec![];
        let first_epoch_block_height =
            self.get_block_info(last_block_info.epoch_first_block())?.height();
        let mut current_block_info = last_block_info.clone();
        while current_block_info.hash() != last_block_info.epoch_first_block() {
            // Check that we didn't reach previous epoch.
            // This only should happen if BlockInfo data is incorrect.
            // Without this assert same BlockInfo will cause infinite loop instead of crash with a message.
            assert!(
                current_block_info.height() > first_epoch_block_height,
                "Reached {:?} from {:?} when first epoch height is {:?}",
                current_block_info,
                last_block_info,
                first_epoch_block_height
            );

            result.push(*current_block_info.hash());
            current_block_info = (*self.get_block_info(current_block_info.prev_hash())?).clone();
        }
        // First block of an epoch is not covered by the while loop.
        result.push(*current_block_info.hash());

        Ok(result)
    }

    #[cfg(feature = "new_epoch_sync")]
    fn get_all_epoch_hashes_from_cache(
        &self,
        last_block_info: &BlockInfo,
        hash_to_prev_hash: &HashMap<CryptoHash, CryptoHash>,
    ) -> Result<Vec<CryptoHash>, EpochError> {
        let _span =
            tracing::debug_span!(target: "epoch_manager", "get_all_epoch_hashes_from_cache", ?last_block_info)
                .entered();

        let mut result = vec![];
        let mut current_hash = *last_block_info.hash();
        while current_hash != *last_block_info.epoch_first_block() {
            result.push(current_hash);
            current_hash = *hash_to_prev_hash
                .get(&current_hash)
                .ok_or(EpochError::MissingBlock(current_hash))?;
        }
        // First block of an epoch is not covered by the while loop.
        result.push(current_hash);

        Ok(result)
    }
}
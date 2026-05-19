mod memo;
mod manifest_place_cancel;
mod meteora_dlmm_wsol_one_side;
mod raydium_cp_swap;

use {
    crate::{
        cli::{
            ManifestPlaceCancelArgs, ManifestPlaceCancelOrder,
            MeteoraDlmmAddRemoveWsolLiquidityArgs, RaydiumCpSwapArgs, TransactionMode,
        },
        error::BoxError,
    },
    solana_address::Address,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{
        bundles::RpcSimulateBundleResult, config::RpcSimulateTransactionAccountsConfig,
    },
    solana_transaction::versioned::VersionedTransaction,
    std::io,
};

pub(crate) enum PreparedTransactions {
    Memo,
    ManifestPlaceCancel(manifest_place_cancel::PreparedManifestPlaceCancel),
    MeteoraDlmmAddRemoveWsolLiquidity(
        meteora_dlmm_wsol_one_side::PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    ),
    RaydiumCpSwap(raydium_cp_swap::PreparedRaydiumCpSwap),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RoundTripSimulationSummary {
    pub input_spent: u64,
    pub returned_input_amount: u64,
    pub bundle_input_delta: i128,
}

#[derive(Clone, Debug)]
pub(crate) enum ResolvedTransactionMode {
    Memo,
    ManifestPlaceCancel(ResolvedManifestPlaceCancelArgs),
    MeteoraDlmmAddRemoveWsolLiquidity(ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs),
    RaydiumCpSwap(ResolvedRaydiumCpSwapArgs),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedRaydiumCpSwapArgs {
    pub pool: Address,
    pub input_mint: Address,
    pub input_amount: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs {
    pub pair: Address,
    pub wsol_amount: u64,
    pub min_wsol_discount_bps: i64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedManifestPlaceCancelArgs {
    pub market: Address,
    pub base_mint: Address,
    pub quote_mint: Address,
    pub order: ResolvedManifestPlaceCancelOrder,
    pub ui_price_quote_per_base: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ResolvedManifestPlaceCancelOrder {
    Ask { base_amount: u64 },
    Bid { quote_amount: u64 },
}

pub(crate) fn resolve_transaction_modes(
    transaction_mode: &TransactionMode,
    slot_count: usize,
) -> Result<Vec<ResolvedTransactionMode>, BoxError> {
    match transaction_mode {
        TransactionMode::Memo => Ok((0..slot_count)
            .map(|_| ResolvedTransactionMode::Memo)
            .collect()),
        TransactionMode::RaydiumCpSwap(args) => resolve_raydium_transaction_modes(args, slot_count),
        TransactionMode::MeteoraDlmmAddRemoveWsolLiquidity(args) => {
            resolve_meteora_dlmm_transaction_modes(args, slot_count)
        }
        TransactionMode::ManifestPlaceCancel(args) => {
            resolve_manifest_transaction_modes(args, slot_count)
        }
        TransactionMode::ScanRaydiumCpSwap(_)
        | TransactionMode::ScanMeteoraDlmm(_)
        | TransactionMode::ProvisionRaydiumCpSwapPool(_)
        | TransactionMode::CreateManifestMarket(_) => Err(io::Error::other(
            "selected subcommand is not a slot-monitor transaction mode",
        )
        .into()),
    }
}

fn resolve_meteora_dlmm_transaction_modes(
    args: &MeteoraDlmmAddRemoveWsolLiquidityArgs,
    slot_count: usize,
) -> Result<Vec<ResolvedTransactionMode>, BoxError> {
    if args.pairs.len() < slot_count {
        return Err(io::Error::other(format!(
            "need at least {} configured dlmm pairs for slot_count={}, got {}",
            slot_count,
            slot_count,
            args.pairs.len(),
        ))
        .into());
    }

    Ok(args
        .pairs
        .iter()
        .take(slot_count)
        .copied()
        .map(|pair| {
            ResolvedTransactionMode::MeteoraDlmmAddRemoveWsolLiquidity(
                ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs {
                    pair,
                    wsol_amount: args.wsol_amount,
                    min_wsol_discount_bps: args.min_wsol_discount_bps,
                },
            )
        })
        .collect())
}

fn resolve_raydium_transaction_modes(
    args: &RaydiumCpSwapArgs,
    slot_count: usize,
) -> Result<Vec<ResolvedTransactionMode>, BoxError> {
    if args.pools.len() < slot_count {
        return Err(io::Error::other(format!(
            "need at least {} configured pools for slot_count={}, got {}",
            slot_count,
            slot_count,
            args.pools.len(),
        ))
        .into());
    }

    Ok(args
        .pools
        .iter()
        .take(slot_count)
        .copied()
        .map(|pool| {
            ResolvedTransactionMode::RaydiumCpSwap(ResolvedRaydiumCpSwapArgs {
                pool,
                input_mint: args.input_mint,
                input_amount: args.input_amount,
            })
        })
        .collect())
}

fn resolve_manifest_transaction_modes(
    args: &ManifestPlaceCancelArgs,
    slot_count: usize,
) -> Result<Vec<ResolvedTransactionMode>, BoxError> {
    let common = args.common();
    if common.markets.len() < slot_count {
        return Err(io::Error::other(format!(
            "need at least {} configured markets for slot_count={}, got {}",
            slot_count,
            slot_count,
            common.markets.len(),
        ))
        .into());
    }

    let order = match &args.order {
        ManifestPlaceCancelOrder::Ask(ask_args) => ResolvedManifestPlaceCancelOrder::Ask {
            base_amount: ask_args.base_amount,
        },
        ManifestPlaceCancelOrder::Bid(bid_args) => ResolvedManifestPlaceCancelOrder::Bid {
            quote_amount: bid_args.quote_amount,
        },
    };

    Ok(common
        .markets
        .iter()
        .take(slot_count)
        .copied()
        .map(|market| {
            ResolvedTransactionMode::ManifestPlaceCancel(ResolvedManifestPlaceCancelArgs {
                market,
                base_mint: common.base_mint,
                quote_mint: common.quote_mint,
                order,
                ui_price_quote_per_base: common.ui_price_quote_per_base,
            })
        })
        .collect())
}

pub(crate) async fn prepare_transactions(
    rpc_client: &RpcClient,
    signer: &Keypair,
    blockhash: Hash,
    transaction_mode: &ResolvedTransactionMode,
    target_slot: Slot,
) -> Result<PreparedTransactions, BoxError> {
    match transaction_mode {
        ResolvedTransactionMode::Memo => Ok(PreparedTransactions::Memo),
        ResolvedTransactionMode::ManifestPlaceCancel(args) => Ok(
            PreparedTransactions::ManifestPlaceCancel(
                manifest_place_cancel::prepare_transactions(rpc_client, signer, args).await?,
            ),
        ),
        ResolvedTransactionMode::MeteoraDlmmAddRemoveWsolLiquidity(args) => Ok(
            PreparedTransactions::MeteoraDlmmAddRemoveWsolLiquidity(
                meteora_dlmm_wsol_one_side::prepare_transactions(
                    rpc_client,
                    signer,
                    args,
                    target_slot,
                )
                .await?,
            ),
        ),
        ResolvedTransactionMode::RaydiumCpSwap(args) => Ok(PreparedTransactions::RaydiumCpSwap(
            raydium_cp_swap::prepare_transactions(rpc_client, signer, blockhash, args, target_slot)
                .await?,
        )),
    }
}

pub(crate) fn build_transactions(
    prepared_transactions: &PreparedTransactions,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<Vec<VersionedTransaction>, BoxError> {
    match prepared_transactions {
        PreparedTransactions::Memo => Ok(memo::build_transactions(
            signer,
            blockhash,
            target_slot,
            include_slot_assert,
        )),
        PreparedTransactions::ManifestPlaceCancel(prepared) => {
            manifest_place_cancel::build_transactions(
                prepared,
                signer,
                blockhash,
                target_slot,
                include_slot_assert,
            )
        }
        PreparedTransactions::MeteoraDlmmAddRemoveWsolLiquidity(prepared) => {
            meteora_dlmm_wsol_one_side::build_transactions(
                prepared,
                signer,
                blockhash,
                target_slot,
                include_slot_assert,
            )
        }
        PreparedTransactions::RaydiumCpSwap(prepared) => {
            raydium_cp_swap::build_transactions(
                prepared,
                signer,
                blockhash,
                target_slot,
                include_slot_assert,
            )
        }
    }
}

pub async fn run_startup_setup(
    rpc_client: &RpcClient,
    signer: &Keypair,
    transaction_modes: &[ResolvedTransactionMode],
) -> Result<(), BoxError> {
    for transaction_mode in transaction_modes {
        match transaction_mode {
            ResolvedTransactionMode::Memo => {}
            ResolvedTransactionMode::ManifestPlaceCancel(args) => {
                manifest_place_cancel::run_startup_setup(rpc_client, signer, args).await?;
            }
            ResolvedTransactionMode::MeteoraDlmmAddRemoveWsolLiquidity(_) => {}
            ResolvedTransactionMode::RaydiumCpSwap(args) => {
                raydium_cp_swap::run_startup_setup(rpc_client, signer, args).await?;
            }
        }
    }

    Ok(())
}

pub async fn run_target_setup(
    rpc_client: &RpcClient,
    signer: &Keypair,
    transaction_modes: &[ResolvedTransactionMode],
) -> Result<(), BoxError> {
    for transaction_mode in transaction_modes {
        match transaction_mode {
            ResolvedTransactionMode::MeteoraDlmmAddRemoveWsolLiquidity(args) => {
                meteora_dlmm_wsol_one_side::run_target_setup(rpc_client, signer, args).await?;
            }
            ResolvedTransactionMode::Memo
            | ResolvedTransactionMode::ManifestPlaceCancel(_)
            | ResolvedTransactionMode::RaydiumCpSwap(_) => {}
        }
    }

    Ok(())
}

pub fn simulation_account_configs(
    prepared_transactions: &PreparedTransactions,
    transaction_count: usize,
) -> Result<
    (
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    ),
    BoxError,
> {
    match prepared_transactions {
        PreparedTransactions::Memo => Ok((vec![None; transaction_count], vec![None; transaction_count])),
        PreparedTransactions::ManifestPlaceCancel(prepared) => {
            manifest_place_cancel::simulation_account_configs(prepared, transaction_count)
        }
        PreparedTransactions::MeteoraDlmmAddRemoveWsolLiquidity(_) => {
            Ok((vec![None; transaction_count], vec![None; transaction_count]))
        }
        PreparedTransactions::RaydiumCpSwap(prepared) => {
            raydium_cp_swap::simulation_account_configs(prepared, transaction_count)
        }
    }
}

pub fn log_post_simulation(
    prepared_transactions: &PreparedTransactions,
    simulation_result: &RpcSimulateBundleResult,
) -> Result<(), BoxError> {
    match prepared_transactions {
        PreparedTransactions::Memo => Ok(()),
        PreparedTransactions::ManifestPlaceCancel(prepared) => {
            manifest_place_cancel::log_post_simulation(prepared, simulation_result)
        }
        PreparedTransactions::MeteoraDlmmAddRemoveWsolLiquidity(prepared) => {
            meteora_dlmm_wsol_one_side::log_post_simulation(prepared, simulation_result)
        }
        PreparedTransactions::RaydiumCpSwap(prepared) => {
            raydium_cp_swap::log_post_simulation(prepared, simulation_result)
        }
    }
}

pub fn round_trip_simulation_summary(
    prepared_transactions: &PreparedTransactions,
    simulation_result: &RpcSimulateBundleResult,
) -> Result<Option<RoundTripSimulationSummary>, BoxError> {
    match prepared_transactions {
        PreparedTransactions::Memo => Ok(None),
        PreparedTransactions::ManifestPlaceCancel(_) => Ok(None),
        PreparedTransactions::MeteoraDlmmAddRemoveWsolLiquidity(_) => Ok(None),
        PreparedTransactions::RaydiumCpSwap(prepared) => Ok(Some(
            raydium_cp_swap::round_trip_simulation_summary(prepared, simulation_result)?,
        )),
    }
}

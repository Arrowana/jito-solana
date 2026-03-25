mod memo;
mod raydium_cp_swap;

use {
    crate::{
        cli::{RaydiumCpSwapArgs, TransactionMode},
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
    RaydiumCpSwap(ResolvedRaydiumCpSwapArgs),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedRaydiumCpSwapArgs {
    pub pool: Address,
    pub input_mint: Address,
    pub input_amount: u64,
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
        TransactionMode::ScanRaydiumCpSwap(_)
        | TransactionMode::ProvisionRaydiumCpSwapPool(_) => Err(
            io::Error::other("scan-raydium-cp-swap is not a writable transaction mode").into(),
        ),
    }
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

pub(crate) async fn prepare_transactions(
    rpc_client: &RpcClient,
    signer: &Keypair,
    blockhash: Hash,
    transaction_mode: &ResolvedTransactionMode,
    target_slot: Slot,
) -> Result<PreparedTransactions, BoxError> {
    match transaction_mode {
        ResolvedTransactionMode::Memo => Ok(PreparedTransactions::Memo),
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
            ResolvedTransactionMode::RaydiumCpSwap(args) => {
                raydium_cp_swap::run_startup_setup(rpc_client, signer, args).await?;
            }
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
        PreparedTransactions::RaydiumCpSwap(prepared) => Ok(Some(
            raydium_cp_swap::round_trip_simulation_summary(prepared, simulation_result)?,
        )),
    }
}

mod memo;
mod raydium_cp_swap;

use {
    crate::cli::TransactionMode,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{
        bundles::RpcSimulateBundleResult, config::RpcSimulateTransactionAccountsConfig,
    },
    solana_transaction::versioned::VersionedTransaction,
    std::error::Error,
};

pub(crate) enum PreparedTransactions {
    Memo,
    RaydiumCpSwap(raydium_cp_swap::PreparedRaydiumCpSwap),
}

pub(crate) fn prepare_transactions(
    rpc_client: &RpcClient,
    signer: &Keypair,
    blockhash: Hash,
    transaction_mode: &TransactionMode,
    target_slot: Slot,
) -> Result<PreparedTransactions, Box<dyn Error>> {
    match transaction_mode {
        TransactionMode::Memo => Ok(PreparedTransactions::Memo),
        TransactionMode::RaydiumCpSwap(args) => Ok(PreparedTransactions::RaydiumCpSwap(
            raydium_cp_swap::prepare_transactions(
                rpc_client,
                signer,
                blockhash,
                args,
                target_slot,
            )?,
        )),
    }
}

pub(crate) fn build_transactions(
    prepared_transactions: &PreparedTransactions,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
) -> Result<Vec<VersionedTransaction>, Box<dyn Error>> {
    match prepared_transactions {
        PreparedTransactions::Memo => Ok(memo::build_transactions(signer, blockhash, target_slot)),
        PreparedTransactions::RaydiumCpSwap(prepared) => {
            raydium_cp_swap::build_transactions(prepared, signer, blockhash, target_slot)
        }
    }
}

pub fn run_startup_setup(
    rpc_client: &RpcClient,
    signer: &Keypair,
    transaction_mode: &TransactionMode,
) -> Result<(), Box<dyn Error>> {
    match transaction_mode {
        TransactionMode::Memo => Ok(()),
        TransactionMode::RaydiumCpSwap(args) => {
            raydium_cp_swap::run_startup_setup(rpc_client, signer, args)
        }
    }
}

pub fn simulation_account_configs(
    rpc_client: &RpcClient,
    signer: &Keypair,
    transaction_mode: &TransactionMode,
    transaction_count: usize,
) -> Result<
    (
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    ),
    Box<dyn Error>,
> {
    match transaction_mode {
        TransactionMode::Memo => Ok((vec![None; transaction_count], vec![None; transaction_count])),
        TransactionMode::RaydiumCpSwap(args) => {
            raydium_cp_swap::simulation_account_configs(rpc_client, signer, args, transaction_count)
        }
    }
}

pub fn log_post_simulation(
    rpc_client: &RpcClient,
    signer: &Keypair,
    transaction_mode: &TransactionMode,
    simulation_result: &RpcSimulateBundleResult,
) -> Result<(), Box<dyn Error>> {
    match transaction_mode {
        TransactionMode::Memo => Ok(()),
        TransactionMode::RaydiumCpSwap(args) => {
            raydium_cp_swap::log_post_simulation(rpc_client, signer, args, simulation_result)
        }
    }
}

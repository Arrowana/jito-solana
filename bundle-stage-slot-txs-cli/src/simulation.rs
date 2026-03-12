use {
    crate::error::BoxError,
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::bundles::{
        RpcBundleSimulationSummary, RpcSimulateBundleConfig, RpcSimulateBundleResult,
        SimulationSlotConfig,
    },
    solana_rpc_client_api::config::RpcSimulateTransactionAccountsConfig,
    solana_transaction::versioned::VersionedTransaction,
    std::io,
};

pub async fn simulate_bundle_with_accounts(
    rpc_client: &RpcClient,
    target_slot: u64,
    transactions: &[VersionedTransaction],
    pre_execution_accounts_configs: Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    post_execution_accounts_configs: Vec<Option<RpcSimulateTransactionAccountsConfig>>,
) -> Result<RpcSimulateBundleResult, BoxError> {
    let simulation_response = rpc_client.simulate_bundle_with_config(
        transactions,
        RpcSimulateBundleConfig {
            pre_execution_accounts_configs,
            post_execution_accounts_configs,
            simulation_bank: Some(SimulationSlotConfig::Commitment(
                CommitmentConfig::processed(),
            )),
            replace_recent_blockhash: false,
            skip_sig_verify: false,
            ..RpcSimulateBundleConfig::default()
        },
    )
    .await?;
    let simulation_result = simulation_response.value;

    match simulation_result.summary {
        RpcBundleSimulationSummary::Succeeded => Ok(simulation_result),
        _ => Err(Box::new(io::Error::other(format!(
            "bundle simulation failed for target_slot={} summary={:?} transaction_results={:?}",
            target_slot, simulation_result.summary, simulation_result.transaction_results
        )))),
    }
}

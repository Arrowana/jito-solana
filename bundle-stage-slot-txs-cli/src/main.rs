mod cli;
mod error;
mod logging;
mod manifest;
mod manifest_market;
mod provision;
mod raydium_cp_swap_constants;
mod scan;
mod schedule;
mod simulation;
mod snapshot;
mod slot_assert;
mod transactions;

use {
    clap::Parser,
    cli::Config,
    dotenvy::from_path_override,
    error::BoxError,
    logging::init_logging,
    manifest_market::create_manifest_market,
    provision::provision_raydium_cp_swap_pool,
    scan::{scan_meteora_dlmm_pools, scan_raydium_cp_swap_pools},
    schedule::{
        countdown_log_bucket, current_slot, estimated_time_to_target, format_eta,
        published_state_for_target, published_state_is_within_grace_window,
        remaining_slots_to_target, LeaderScheduleCache, PublishedState,
    },
    simulation::simulate_bundle_with_accounts,
    snapshot::{
        clear_snapshot_file, write_target_slots_snapshot, BaitAndDisappearSlotTransactions,
        BAIT_AND_DISAPPEAR_TXS_PATH,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::{read_keypair_file, Keypair},
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{io, path::PathBuf, sync::Arc, time::Duration},
    tokio::{task::JoinSet, time::sleep},
    tracing::{error, info},
    transactions::{
        build_transactions, log_post_simulation, prepare_transactions, resolve_transaction_modes,
        run_startup_setup, run_target_setup, simulation_account_configs, ResolvedTransactionMode,
    },
};

const POLL_INTERVAL: Duration = Duration::from_millis(1_000);

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BoxError> {
    load_dotenv();
    init_logging();

    let config = Config::parse();
    config.validate()?;

    let transaction_mode = config.selected_transaction_mode();
    let rpc_client = Arc::new(RpcClient::new(config.rpc_url.clone()));
    if let cli::TransactionMode::ScanRaydiumCpSwap(args) = &transaction_mode {
        let signer = if args.simulate_top > 0 {
            let keypair_path = config.require_keypair()?;
            Some(read_keypair_file(keypair_path).map_err(|err| {
                io::Error::other(format!(
                    "failed to read keypair from {}: {err}",
                    keypair_path,
                ))
            })?)
        } else {
            None
        };
        info!(
            rpc_url = %config.rpc_url,
            input_mint = %args.input_mint,
            input_amount = args.input_amount,
            token_allowlist_source = args.token_allowlist_source.label(),
            min_estimated_tvl_usdc = args.min_estimated_tvl_usdc,
            min_token_volume_24h_usd = args.min_token_volume_24h_usd,
            allowed_trade_fee_rates = ?args.trade_fee_rates,
            top = args.top,
            simulate_top = args.simulate_top,
            "starting raydium cp-swap pool scan",
        );
        scan_raydium_cp_swap_pools(rpc_client.as_ref(), args, signer.as_ref()).await?;
        return Ok(());
    }
    if let cli::TransactionMode::ScanMeteoraDlmm(args) = &transaction_mode {
        info!(
            rpc_url = %config.rpc_url,
            token_allowlist_source = args.token_allowlist_source.label(),
            min_token_volume_24h_usd = args.min_token_volume_24h_usd,
            min_wsol_discount_bps = args.min_wsol_discount_bps,
            min_estimated_tvl_usdc = args.min_estimated_tvl_usdc,
            top = args.top,
            output_csv = %args.output_csv.display(),
            "starting meteora dlmm pool scan",
        );
        scan_meteora_dlmm_pools(rpc_client.as_ref(), args).await?;
        return Ok(());
    }
    if let cli::TransactionMode::ProvisionRaydiumCpSwapPool(args) = &transaction_mode {
        let keypair_path = config.require_keypair()?;
        let jup_api_key = config.jup_api_key();
        let signer = read_keypair_file(keypair_path).map_err(|err| {
            io::Error::other(format!(
                "failed to read keypair from {}: {err}",
                keypair_path,
            ))
        })?;
        info!(
            rpc_url = %config.rpc_url,
            token_a_mint = %args.token_a_mint,
            token_b_mint = %args.token_b_mint,
            total_tvl_usdc = args.total_tvl_usdc,
            chunk_tvl_usdc = args.chunk_tvl_usdc,
            max_existing_tvl_usdc = args.max_existing_tvl_usdc,
            max_price_deviation_bps = args.max_price_deviation_bps,
            max_jupiter_price_impact_bps = args.max_jupiter_price_impact_bps,
            jupiter_slippage_bps = args.jupiter_slippage_bps,
            using_jup_api_key = jup_api_key.is_some(),
            "starting raydium cp-swap pool provisioning",
        );
        provision_raydium_cp_swap_pool(rpc_client.as_ref(), &signer, args, jup_api_key.as_deref())
            .await?;
        return Ok(());
    }
    if let cli::TransactionMode::CreateManifestMarket(args) = &transaction_mode {
        let keypair_path = config.require_keypair()?;
        let signer = read_keypair_file(keypair_path).map_err(|err| {
            io::Error::other(format!(
                "failed to read keypair from {}: {err}",
                keypair_path,
            ))
        })?;
        info!(
            rpc_url = %config.rpc_url,
            base_mint = %args.base_mint,
            quote_mint = %args.quote_mint,
            "starting manifest market creation",
        );
        create_manifest_market(rpc_client.as_ref(), &signer, args.base_mint, args.quote_mint).await?;
        return Ok(());
    }

    let configured_transaction_modes =
        resolve_transaction_modes(&transaction_mode, usize::from(config.consecutive_slots))?;
    let identity = config.require_identity()?;
    let keypair_path = config.require_keypair()?;
    let signer = Arc::new(read_keypair_file(keypair_path).map_err(|err| {
        io::Error::other(format!(
            "failed to read keypair from {}: {err}",
            keypair_path,
        ))
    })?);
    let mut published_state = PublishedState::Cleared;
    let mut previous_target_slots = None;
    let mut previous_setup_target_slots = None;
    let mut previous_countdown_bucket = None;
    let mut leader_schedule_cache = LeaderScheduleCache::default();

    clear_snapshot_file(config.gap_duration_millis)?;
    info!(
        identity = %identity,
        rpc_url = %config.rpc_url,
        snapshot_path = BAIT_AND_DISAPPEAR_TXS_PATH,
        gap_duration_millis = config.gap_duration_millis,
        consecutive_slots = config.consecutive_slots,
        transaction_mode = transaction_mode.label(),
        "started bundle-stage slot tx monitor",
    );
    run_startup_setup(
        rpc_client.as_ref(),
        signer.as_ref(),
        &configured_transaction_modes,
    )
    .await?;

    loop {
        let current_slot = match current_slot(rpc_client.as_ref()).await {
            Ok(current_slot) => current_slot,
            Err(err) => {
                error!(err = %err, "failed to fetch current slot");
                sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        let target_slots = match leader_schedule_cache
            .target_slots(
                rpc_client.as_ref(),
                &identity,
                current_slot,
                usize::from(config.consecutive_slots),
            )
            .await
        {
            Ok(Some(target_slots)) => target_slots,
            Ok(None) => {
                panic!(
                    "no upcoming leader slot found for configured identity {} at current_slot={}",
                    identity, current_slot
                );
            }
            Err(err) => {
                error!(current_slot, err = %err, "failed to determine next target slot");
                sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        if previous_target_slots.as_ref() != Some(&target_slots) {
            let remaining_slots = remaining_slots_to_target(target_slots.start_slot(), current_slot);
            let eta = estimated_time_to_target(target_slots.start_slot(), current_slot);
            info!(
                current_slot,
                target_start_slot = target_slots.start_slot(),
                target_last_slot = target_slots.last_slot(),
                target_slot_count = target_slots.slots.len(),
                remaining_slots,
                eta = %format_eta(eta),
                "observed new target slot range",
            );
            previous_target_slots = Some(target_slots.clone());
            previous_countdown_bucket =
                countdown_log_bucket(target_slots.start_slot(), current_slot);
        }

        if previous_setup_target_slots.as_ref() != Some(&target_slots) {
            let slot_transaction_modes = configured_transaction_modes
                .iter()
                .take(target_slots.slots.len())
                .cloned()
                .collect::<Vec<_>>();
            match run_target_setup(rpc_client.as_ref(), signer.as_ref(), &slot_transaction_modes).await
            {
                Ok(()) => {
                    info!(
                        current_slot,
                        target_start_slot = target_slots.start_slot(),
                        target_last_slot = target_slots.last_slot(),
                        target_slot_count = target_slots.slots.len(),
                        "completed target slot range setup",
                    );
                    previous_setup_target_slots = Some(target_slots.clone());
                }
                Err(err) => {
                    error!(
                        current_slot,
                        target_start_slot = target_slots.start_slot(),
                        target_last_slot = target_slots.last_slot(),
                        err = %err,
                        "failed target slot range setup",
                    );
                    sleep(POLL_INTERVAL).await;
                    continue;
                }
            }
        }

        if previous_target_slots.as_ref() == Some(&target_slots) {
            let countdown_bucket = countdown_log_bucket(target_slots.start_slot(), current_slot);
            if previous_countdown_bucket != countdown_bucket {
                let remaining_slots =
                    remaining_slots_to_target(target_slots.start_slot(), current_slot);
                let eta = estimated_time_to_target(target_slots.start_slot(), current_slot);
                info!(
                    current_slot,
                    target_start_slot = target_slots.start_slot(),
                    target_last_slot = target_slots.last_slot(),
                    target_slot_count = target_slots.slots.len(),
                    remaining_slots,
                    eta = %format_eta(eta),
                    "target slot range countdown update",
                );
                previous_countdown_bucket = countdown_bucket;
            }
        }

        match published_state_for_target(&target_slots, current_slot) {
            next_published_state @ PublishedState::Armed {
                start_slot,
                last_slot,
            } if published_state != next_published_state => {
                let blockhash = match rpc_client.get_latest_blockhash().await {
                    Ok(blockhash) => blockhash,
                    Err(err) => {
                        error!(start_slot, last_slot, err = %err, "failed to fetch latest blockhash");
                        sleep(POLL_INTERVAL).await;
                        continue;
                    }
                };

                let slot_transaction_modes = configured_transaction_modes
                    .iter()
                    .take(target_slots.slots.len())
                    .cloned()
                    .collect::<Vec<_>>();

                let slot_transactions = match prepare_target_slots(
                    rpc_client.clone(),
                    signer.clone(),
                    blockhash,
                    &target_slots.slots,
                    slot_transaction_modes,
                )
                .await
                {
                    Ok(slot_transactions) => slot_transactions,
                    Err(err) => {
                        clear_stale_snapshot_if_needed(
                            config.gap_duration_millis,
                            &mut published_state,
                            current_slot,
                        )?;
                        error!(
                            target_start_slot = start_slot,
                            target_last_slot = last_slot,
                            err = %err,
                            "failed to prepare target slot transactions",
                        );
                        sleep(POLL_INTERVAL).await;
                        continue;
                    }
                };

                write_target_slots_snapshot(config.gap_duration_millis, slot_transactions)?;
                info!(
                    target_start_slot = start_slot,
                    target_last_slot = last_slot,
                    target_slot_count = target_slots.slots.len(),
                    gap_duration_millis = config.gap_duration_millis,
                    "armed next leader rotation slot range after successful simulation",
                );
                published_state = next_published_state;
            }
            PublishedState::Cleared
                if published_state != PublishedState::Cleared
                    && !published_state_is_within_grace_window(&published_state, current_slot) =>
            {
                clear_snapshot_file(config.gap_duration_millis)?;
                info!(
                    target_start_slot = target_slots.start_slot(),
                    target_last_slot = target_slots.last_slot(),
                    target_slot_count = target_slots.slots.len(),
                    "cleared snapshot after target slot range moved out of arming window",
                );
                published_state = PublishedState::Cleared;
            }
            _ => {}
        }

        sleep(POLL_INTERVAL).await;
    }
}

fn load_dotenv() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for path in [manifest_dir.join(".env"), PathBuf::from(".env")] {
        if path.is_file() {
            let _ = from_path_override(path);
            break;
        }
    }
}

async fn prepare_target_slots(
    rpc_client: Arc<RpcClient>,
    signer: Arc<Keypair>,
    blockhash: Hash,
    target_slots: &[Slot],
    transaction_modes: Vec<ResolvedTransactionMode>,
) -> Result<Vec<BaitAndDisappearSlotTransactions>, BoxError> {
    let mut join_set = JoinSet::new();

    for (target_slot, transaction_mode) in target_slots
        .iter()
        .copied()
        .zip(transaction_modes.into_iter())
    {
        let rpc_client = rpc_client.clone();
        let signer = signer.clone();
        join_set.spawn(async move {
            prepare_target_slot(rpc_client, signer, blockhash, target_slot, transaction_mode).await
        });
    }

    let mut slot_transactions = Vec::with_capacity(target_slots.len());
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(Ok(slot_entry)) => slot_transactions.push(slot_entry),
            Ok(Err(err)) => {
                join_set.abort_all();
                return Err(err);
            }
            Err(err) => {
                join_set.abort_all();
                return Err(io::Error::other(format!(
                    "target slot task join failure: {err}",
                ))
                .into());
            }
        }
    }

    slot_transactions.sort_unstable_by_key(|slot_entry| slot_entry.slot);
    Ok(slot_transactions)
}

async fn prepare_target_slot(
    rpc_client: Arc<RpcClient>,
    signer: Arc<Keypair>,
    blockhash: Hash,
    target_slot: Slot,
    transaction_mode: ResolvedTransactionMode,
) -> Result<BaitAndDisappearSlotTransactions, BoxError> {
    let prepared_transactions = prepare_transactions(
        rpc_client.as_ref(),
        signer.as_ref(),
        blockhash,
        &transaction_mode,
        target_slot,
    )
    .await
    .map_err(|err| {
        io::Error::other(format!(
            "failed to prepare transactions for slot {}: {err}",
            target_slot,
        ))
    })?;

    let transactions = build_transactions(
        &prepared_transactions,
        signer.as_ref(),
        blockhash,
        target_slot,
        false,
    )
    .map_err(|err| {
        io::Error::other(format!(
            "failed to build transactions for slot {}: {err}",
            target_slot,
        ))
    })?;

    let (pre_execution_accounts_configs, post_execution_accounts_configs) =
        simulation_account_configs(&prepared_transactions, transactions.len()).map_err(|err| {
            io::Error::other(format!(
                "failed to build simulation account configs for slot {}: {err}",
                target_slot,
            ))
        })?;

    let simulation_result = simulate_bundle_with_accounts(
        rpc_client.as_ref(),
        target_slot,
        &transactions,
        pre_execution_accounts_configs,
        post_execution_accounts_configs,
    )
    .await
    .map_err(|err| {
        io::Error::other(format!(
            "bundle simulation failed for slot {}: {err}",
            target_slot,
        ))
    })?;

    log_post_simulation(&prepared_transactions, &simulation_result).map_err(|err| {
        io::Error::other(format!(
            "failed to log simulation details for slot {}: {err}",
            target_slot,
        ))
    })?;

    let published_transactions = build_transactions(
        &prepared_transactions,
        signer.as_ref(),
        blockhash,
        target_slot,
        true,
    )
    .map_err(|err| {
        io::Error::other(format!(
            "failed to build published transactions for slot {}: {err}",
            target_slot,
        ))
    })?;

    Ok(BaitAndDisappearSlotTransactions {
        slot: target_slot,
        transactions: published_transactions,
    })
}

fn clear_stale_snapshot_if_needed(
    gap_duration_millis: u64,
    published_state: &mut PublishedState,
    current_slot: Slot,
) -> Result<(), BoxError> {
    if *published_state != PublishedState::Cleared
        && !published_state_is_within_grace_window(published_state, current_slot)
    {
        clear_snapshot_file(gap_duration_millis)?;
        *published_state = PublishedState::Cleared;
    }

    Ok(())
}

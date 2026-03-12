mod cli;
mod logging;
mod schedule;
mod simulation;
mod snapshot;
mod transactions;

use {
    clap::Parser,
    cli::Config,
    logging::init_logging,
    schedule::{
        countdown_log_bucket, current_slot, estimated_time_to_target, format_eta,
        published_state_for_target, published_state_is_within_grace_window,
        remaining_slots_to_target, LeaderScheduleCache, PublishedState,
    },
    simulation::simulate_bundle_with_accounts,
    snapshot::{clear_snapshot_file, write_target_slot_snapshot, BAIT_AND_DISAPPEAR_TXS_PATH},
    solana_keypair::read_keypair_file,
    solana_rpc_client::rpc_client::RpcClient,
    std::{error::Error, thread, time::Duration},
    tracing::{error, info},
    transactions::{
        build_transactions, log_post_simulation, run_startup_setup, simulation_account_configs,
    },
};

const POLL_INTERVAL: Duration = Duration::from_millis(1_000);

fn main() -> Result<(), Box<dyn Error>> {
    init_logging();

    let config = Config::parse();
    let transaction_mode = config.selected_transaction_mode();
    let rpc_client = RpcClient::new(config.rpc_url.clone());
    let signer = read_keypair_file(&config.keypair)?;
    let mut published_state = PublishedState::Cleared;
    let mut previous_target_slot = None;
    let mut previous_countdown_bucket = None;
    let mut leader_schedule_cache = LeaderScheduleCache::default();

    clear_snapshot_file(config.gap_duration_millis)?;
    info!(
        identity = %config.identity,
        rpc_url = %config.rpc_url,
        snapshot_path = BAIT_AND_DISAPPEAR_TXS_PATH,
        gap_duration_millis = config.gap_duration_millis,
        transaction_mode = transaction_mode.label(),
        "started bundle-stage slot tx monitor",
    );
    run_startup_setup(&rpc_client, &signer, &transaction_mode)?;

    loop {
        let current_slot = match current_slot(&rpc_client) {
            Ok(current_slot) => current_slot,
            Err(err) => {
                error!(err = %err, "failed to fetch current slot");
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };

        let target_slot = match leader_schedule_cache
            .target_slot(&rpc_client, &config.identity, current_slot)
        {
            Ok(Some(target_slot)) => target_slot,
            Ok(None) => {
                panic!(
                    "no upcoming leader slot found for configured identity {} at current_slot={}",
                    config.identity, current_slot
                );
            }
            Err(err) => {
                error!(current_slot, err = %err, "failed to determine next target slot");
                thread::sleep(POLL_INTERVAL);
                continue;
            }
        };

        if previous_target_slot != Some(target_slot) {
            let remaining_slots = remaining_slots_to_target(target_slot, current_slot);
            let eta = estimated_time_to_target(target_slot, current_slot);
            info!(
                current_slot,
                target_slot,
                remaining_slots,
                eta = %format_eta(eta),
                "observed new target slot",
            );
            previous_target_slot = Some(target_slot);
            previous_countdown_bucket = countdown_log_bucket(target_slot, current_slot);
        } else {
            let countdown_bucket = countdown_log_bucket(target_slot, current_slot);
            if previous_countdown_bucket != countdown_bucket {
                let remaining_slots = remaining_slots_to_target(target_slot, current_slot);
                let eta = estimated_time_to_target(target_slot, current_slot);
                info!(
                    current_slot,
                    target_slot,
                    remaining_slots,
                    eta = %format_eta(eta),
                    "target slot countdown update",
                );
                previous_countdown_bucket = countdown_bucket;
            }
        }

        match published_state_for_target(target_slot, current_slot) {
            PublishedState::Armed(slot) if published_state != PublishedState::Armed(slot) => {
                let blockhash = match rpc_client.get_latest_blockhash() {
                    Ok(blockhash) => blockhash,
                    Err(err) => {
                        error!(slot, err = %err, "failed to fetch latest blockhash");
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                let transactions =
                    match build_transactions(&rpc_client, &signer, blockhash, &transaction_mode) {
                        Ok(transactions) => transactions,
                        Err(err) => {
                            if published_state != PublishedState::Cleared
                                && !published_state_is_within_grace_window(
                                    &published_state,
                                    current_slot,
                                )
                            {
                                clear_snapshot_file(config.gap_duration_millis)?;
                                published_state = PublishedState::Cleared;
                            }
                            error!(slot, err = %err, "failed to build target transactions");
                            thread::sleep(POLL_INTERVAL);
                            continue;
                        }
                    };

                let (pre_execution_accounts_configs, post_execution_accounts_configs) =
                    match simulation_account_configs(
                        &rpc_client,
                        &signer,
                        &transaction_mode,
                        transactions.len(),
                    ) {
                        Ok(configs) => configs,
                        Err(err) => {
                            error!(
                                slot,
                                err = %err,
                                "failed to prepare bundle simulation account configs",
                            );
                            thread::sleep(POLL_INTERVAL);
                            continue;
                        }
                    };

                let simulation_result = match simulate_bundle_with_accounts(
                    &rpc_client,
                    slot,
                    &transactions,
                    pre_execution_accounts_configs,
                    post_execution_accounts_configs,
                ) {
                    Ok(simulation_result) => simulation_result,
                    Err(err) => {
                        if published_state != PublishedState::Cleared
                            && !published_state_is_within_grace_window(
                                &published_state,
                                current_slot,
                            )
                        {
                            clear_snapshot_file(config.gap_duration_millis)?;
                            published_state = PublishedState::Cleared;
                        }
                        error!(slot, err = %err, "bundle simulation failed, not publishing snapshot");
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                if let Err(err) =
                    log_post_simulation(&rpc_client, &signer, &transaction_mode, &simulation_result)
                {
                    if published_state != PublishedState::Cleared
                        && !published_state_is_within_grace_window(&published_state, current_slot)
                    {
                        clear_snapshot_file(config.gap_duration_millis)?;
                        published_state = PublishedState::Cleared;
                    }
                    error!(slot, err = %err, "failed to log bundle simulation details");
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }

                write_target_slot_snapshot(config.gap_duration_millis, slot, transactions)?;
                info!(
                    slot,
                    gap_duration_millis = config.gap_duration_millis,
                    "armed next leader rotation slot after successful simulation",
                );
                published_state = PublishedState::Armed(slot);
            }
            PublishedState::Cleared
                if published_state != PublishedState::Cleared
                    && !published_state_is_within_grace_window(&published_state, current_slot) =>
            {
                clear_snapshot_file(config.gap_duration_millis)?;
                info!(
                    target_slot,
                    "cleared snapshot after target slot moved out of arming window",
                );
                published_state = PublishedState::Cleared;
            }
            _ => {}
        }

        thread::sleep(POLL_INTERVAL);
    }
}

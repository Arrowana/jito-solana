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
    snapshot::{
        clear_snapshot_file, write_target_slots_snapshot, BaitAndDisappearSlotTransactions,
        BAIT_AND_DISAPPEAR_TXS_PATH,
    },
    solana_clock::Slot,
    solana_keypair::read_keypair_file,
    solana_rpc_client::rpc_client::RpcClient,
    std::{error::Error, thread, time::Duration},
    tracing::{error, info},
    transactions::{
        build_transactions, log_post_simulation, prepare_transactions, run_startup_setup,
        simulation_account_configs,
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
    let mut previous_target_slots = None;
    let mut previous_countdown_bucket = None;
    let mut leader_schedule_cache = LeaderScheduleCache::default();

    clear_snapshot_file(config.gap_duration_millis)?;
    info!(
        identity = %config.identity,
        rpc_url = %config.rpc_url,
        snapshot_path = BAIT_AND_DISAPPEAR_TXS_PATH,
        gap_duration_millis = config.gap_duration_millis,
        consecutive_slots = config.consecutive_slots,
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

        let target_slots = match leader_schedule_cache.target_slots(
            &rpc_client,
            &config.identity,
            current_slot,
            usize::from(config.consecutive_slots),
        )
        {
            Ok(Some(target_slots)) => target_slots,
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

        if previous_target_slots.as_ref() != Some(&target_slots) {
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
                "observed new target slot range",
            );
            previous_target_slots = Some(target_slots.clone());
            previous_countdown_bucket =
                countdown_log_bucket(target_slots.start_slot(), current_slot);
        } else {
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
                let blockhash = match rpc_client.get_latest_blockhash() {
                    Ok(blockhash) => blockhash,
                    Err(err) => {
                        error!(start_slot, last_slot, err = %err, "failed to fetch latest blockhash");
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                let prepared_transactions = match prepare_transactions(
                    &rpc_client,
                    &signer,
                    blockhash,
                    &transaction_mode,
                    start_slot,
                ) {
                    Ok(prepared_transactions) => prepared_transactions,
                    Err(err) => {
                        clear_stale_snapshot_if_needed(
                            config.gap_duration_millis,
                            &mut published_state,
                            current_slot,
                        )?;
                        error!(start_slot, err = %err, "failed to prepare target transactions");
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                let simulated_transactions = match build_transactions(
                    &prepared_transactions,
                    &signer,
                    blockhash,
                    start_slot,
                ) {
                    Ok(transactions) => transactions,
                    Err(err) => {
                        clear_stale_snapshot_if_needed(
                            config.gap_duration_millis,
                            &mut published_state,
                            current_slot,
                        )?;
                        error!(start_slot, err = %err, "failed to build simulated target transactions");
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                let (pre_execution_accounts_configs, post_execution_accounts_configs) =
                    match simulation_account_configs(
                        &rpc_client,
                        &signer,
                        &transaction_mode,
                        simulated_transactions.len(),
                    ) {
                        Ok(configs) => configs,
                        Err(err) => {
                            clear_stale_snapshot_if_needed(
                                config.gap_duration_millis,
                                &mut published_state,
                                current_slot,
                            )?;
                            error!(
                                start_slot,
                                err = %err,
                                "failed to prepare bundle simulation account configs",
                            );
                            thread::sleep(POLL_INTERVAL);
                            continue;
                        }
                    };

                let simulation_result = match simulate_bundle_with_accounts(
                    &rpc_client,
                    start_slot,
                    &simulated_transactions,
                    pre_execution_accounts_configs,
                    post_execution_accounts_configs,
                ) {
                    Ok(simulation_result) => simulation_result,
                    Err(err) => {
                        clear_stale_snapshot_if_needed(
                            config.gap_duration_millis,
                            &mut published_state,
                            current_slot,
                        )?;
                        error!(
                            start_slot,
                            err = %err,
                            "bundle simulation failed, not publishing snapshot",
                        );
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                if let Err(err) = log_post_simulation(
                    &rpc_client,
                    &signer,
                    &transaction_mode,
                    &simulation_result,
                ) {
                    clear_stale_snapshot_if_needed(
                        config.gap_duration_millis,
                        &mut published_state,
                        current_slot,
                    )?;
                    error!(
                        start_slot,
                        err = %err,
                        "failed to log bundle simulation details",
                    );
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }

                let mut slot_transactions = Vec::with_capacity(target_slots.slots.len());
                slot_transactions.push(BaitAndDisappearSlotTransactions {
                    slot: start_slot,
                    transactions: simulated_transactions,
                });

                let mut build_failed = false;
                for slot in target_slots.slots.iter().skip(1) {
                    let transactions = match build_transactions(
                        &prepared_transactions,
                        &signer,
                        blockhash,
                        *slot,
                    ) {
                        Ok(transactions) => transactions,
                        Err(err) => {
                            clear_stale_snapshot_if_needed(
                                config.gap_duration_millis,
                                &mut published_state,
                                current_slot,
                            )?;
                            error!(slot, err = %err, "failed to build target transactions");
                            build_failed = true;
                            thread::sleep(POLL_INTERVAL);
                            break;
                        }
                    };

                    slot_transactions.push(BaitAndDisappearSlotTransactions {
                        slot: *slot,
                        transactions,
                    });
                }

                if build_failed {
                    continue;
                }

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

        thread::sleep(POLL_INTERVAL);
    }
}

fn clear_stale_snapshot_if_needed(
    gap_duration_millis: u64,
    published_state: &mut PublishedState,
    current_slot: Slot,
) -> Result<(), Box<dyn Error>> {
    if *published_state != PublishedState::Cleared
        && !published_state_is_within_grace_window(published_state, current_slot)
    {
        clear_snapshot_file(gap_duration_millis)?;
        *published_state = PublishedState::Cleared;
    }

    Ok(())
}

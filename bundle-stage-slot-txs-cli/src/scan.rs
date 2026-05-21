use {
    crate::{
        cli::{
            DlmmTargetBinMode, ScanMeteoraDlmmArgs, ScanRaydiumCpSwapArgs, TokenAllowlistSource,
        },
        error::BoxError,
        raydium_cp_swap_constants::{
            AMM_CONFIG_DISCRIMINATOR, POOL_STATE_DISCRIMINATOR, RAYDIUM_CP_SWAP_PROGRAM_ID,
        },
        simulation::simulate_bundle_with_accounts,
        transactions::{
            build_transactions, log_post_simulation, prepare_transactions,
            round_trip_simulation_summary, run_startup_setup, simulation_account_configs,
            PreparedTransactions, ResolvedRaydiumCpSwapArgs, ResolvedTransactionMode,
        },
    },
    anchor_lang::{declare_program, solana_program::pubkey::Pubkey as AnchorPubkey, Discriminator},
    bytemuck::{Pod, Zeroable},
    csv::{ReaderBuilder, WriterBuilder},
    reqwest::Client,
    serde::Deserialize,
    solana_account::Account,
    solana_address::{address, Address},
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{
        config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        filter::{Memcmp, RpcFilterType},
        response::UiAccountEncoding,
    },
    std::{
        cmp::Reverse,
        collections::{HashMap, HashSet},
        io,
        mem::size_of,
        path::PathBuf,
        time::Duration,
    },
    tracing::{error, info},
};

declare_program!(dlmm);

const WSOL_MINT: Address = address!("So11111111111111111111111111111111111111112");
const METEORA_DLMM_PROGRAM_ID: Address = address!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const DLMM_BINS_PER_ARRAY: i32 = 70;
const LB_PAIR_TOKEN_X_MINT_OFFSET: usize = 88;
const LB_PAIR_TOKEN_Y_MINT_OFFSET: usize = 120;
const POOL_STATE_TOKEN_0_MINT_OFFSET: usize = 8 + (size_of::<Address>() * 5);
const POOL_STATE_TOKEN_1_MINT_OFFSET: usize =
    POOL_STATE_TOKEN_0_MINT_OFFSET + size_of::<Address>();
const TOKEN_ACCOUNT_AMOUNT_OFFSET: usize = 64;
const RPC_BATCH_SIZE: usize = 100;
const ROUND_TRIP_TOLERANCE_BPS: u64 = 500;
const DEFI_LLAMA_SOLANA_TOP200_PROTOCOL_TOKEN_MAPPING_CSV: &str =
    "defillama_solana_top200_protocol_token_mapping.csv";
const JUPITER_VERIFIED_TOKENS_CSV: &str = "jupiter_verified_tokens.csv";
const JUPITER_VERIFIED_TOKENS_API_URL: &str = "https://api.jup.ag/tokens/v2/tag?query=verified";
const JUPITER_VERIFIED_TOKENS_LITE_URL: &str =
    "https://lite-api.jup.ag/tokens/v2/tag?query=verified";
const JUPITER_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const WSOL_DECIMALS_FACTOR: f64 = 1_000_000_000.0;

#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PoolStateRaw {
    discriminator: [u8; 8],
    amm_config: Address,
    pool_creator: Address,
    token_0_vault: Address,
    token_1_vault: Address,
    lp_mint: Address,
    token_0_mint: Address,
    token_1_mint: Address,
    token_0_program: Address,
    token_1_program: Address,
    observation_key: Address,
    auth_bump: u8,
    status: u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct AmmConfigRaw {
    discriminator: [u8; 8],
    bump: u8,
    disable_create_pool: u8,
    index: u16,
    trade_fee_rate: u64,
    protocol_fee_rate: u64,
    fund_fee_rate: u64,
    create_pool_fee: u64,
    protocol_owner: Address,
    fund_owner: Address,
    creator_fee_rate: u64,
    padding: [u64; 15],
}

#[derive(Clone, Copy, Debug)]
struct AmmConfigInfo {
    trade_fee_rate: u64,
    protocol_fee_rate: u64,
    fund_fee_rate: u64,
    creator_fee_rate: u64,
}

#[derive(Clone, Copy, Debug)]
struct PoolCandidate {
    pool: Address,
    amm_config: Address,
    output_mint: Address,
    non_wsol_mint: Address,
    input_vault: Address,
    output_vault: Address,
    wsol_vault: Address,
}

#[derive(Clone, Copy, Debug)]
struct RankedPoolCandidate {
    pool: Address,
    amm_config: Address,
    output_mint: Address,
    non_wsol_mint: Address,
    input_reserve: u64,
    output_reserve: u64,
    wsol_reserve: u64,
    estimated_tvl_usdc: f64,
    trade_fee_rate: u64,
    protocol_fee_rate: u64,
    fund_fee_rate: u64,
    creator_fee_rate: u64,
    estimated_output_amount: u64,
    estimated_price_impact_bps: u64,
    input_share_bps: u64,
}

#[derive(Clone, Debug, Default)]
struct TokenAllowlist {
    mints: HashSet<Address>,
    symbols_by_mint: HashMap<Address, String>,
}

#[derive(Clone, Debug)]
struct TokenMetadata {
    symbol: String,
    decimals: u8,
    usd_price: f64,
    volume_24h_usd: f64,
}

#[derive(Clone, Debug)]
struct DlmmPairCandidate {
    pair: Address,
    token_x_mint: Address,
    token_y_mint: Address,
    reserve_x: Address,
    reserve_y: Address,
    non_wsol_mint: Address,
    active_id: i32,
    bin_step: u16,
    status: u8,
}

#[derive(Clone, Debug)]
struct RankedDlmmPairCandidate {
    pair: Address,
    token_x_mint: Address,
    token_y_mint: Address,
    non_wsol_mint: Address,
    non_wsol_symbol: String,
    active_id: i32,
    target_bin_id: i32,
    bin_step: u16,
    status: u8,
    target_bin_array: Address,
    target_bin_array_index: i64,
    target_bin_index: usize,
    wsol_side: &'static str,
    target_bin_amount_x: u64,
    target_bin_amount_y: u64,
    target_bin_liquidity_supply: u128,
    target_bin_open_order_amount: u64,
    target_bin_is_empty: bool,
    reserve_x_ui_amount: f64,
    reserve_y_ui_amount: f64,
    wsol_reserve_ui_amount: f64,
    estimated_tvl_usdc: f64,
    active_price_y_per_x: f64,
    fair_price_y_per_x: f64,
    implied_wsol_usdc_price: f64,
    reference_wsol_usdc_price: f64,
    wsol_discount_bps: i64,
    wsol_is_cheap_for_arbers: bool,
    token_volume_24h_usd: f64,
}

#[derive(Clone, Copy, Debug)]
struct TargetBinInfo {
    bin_array: Address,
    bin_array_index: i64,
    bin_index: usize,
    amount_x: u64,
    amount_y: u64,
    liquidity_supply: u128,
    open_order_amount: u64,
    total_processing_order_amount: u64,
    processed_order_remaining_amount: u64,
}

pub async fn scan_raydium_cp_swap_pools(
    rpc_client: &RpcClient,
    args: &ScanRaydiumCpSwapArgs,
    signer: Option<&Keypair>,
) -> Result<(), BoxError> {
    let allowed_fee_rates = args.trade_fee_rates.iter().copied().collect::<HashSet<_>>();
    let token_allowlist = load_token_allowlist(args).await?;
    let wsol_usdc_price = load_wsol_usdc_price_from_jupiter_verified_csv()?;
    info!(
        token_allowlist_source = args.token_allowlist_source.label(),
        token_allowlist_count = token_allowlist.mints.len(),
        "loaded scan token allowlist",
    );
    info!(
        wsol_usdc_price,
        "loaded wsol price for tvl estimation",
    );
    info!(
        input_mint = %args.input_mint,
        input_amount = args.input_amount,
        allowed_trade_fee_rates = ?args.trade_fee_rates,
        wsol_mint = %WSOL_MINT,
        "fetching raydium cp-swap pool accounts with wsol-side filter",
    );
    let pool_accounts = fetch_pool_accounts_with_wsol_side(rpc_client).await?;
    info!(
        total_pool_accounts = pool_accounts.len(),
        "fetched raydium cp-swap pool accounts with wsol-side filter",
    );

    info!(
        input_mint = %args.input_mint,
        "filtering pool accounts by input mint",
    );
    let matching_input_mint_candidates = pool_accounts
        .iter()
        .filter_map(|(pool, account)| {
            decode_pool_candidate(*pool, account, args.input_mint).transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let input_mint_candidate_count = matching_input_mint_candidates.len();
    info!(
        matching_input_mint_pools = matching_input_mint_candidates.len(),
        "filtered pool accounts by input mint",
    );

    info!(
        token_allowlist_count = token_allowlist.mints.len(),
        "filtering pool accounts by scan token allowlist",
    );
    let allowlist_filtered_candidates = matching_input_mint_candidates
        .into_iter()
        .filter(|candidate| token_allowlist.mints.contains(&candidate.non_wsol_mint))
        .collect::<Vec<_>>();
    info!(
        input_mint_filtered_pools = input_mint_candidate_count,
        token_allowlist_count = token_allowlist.mints.len(),
        allowlist_filtered_pools = allowlist_filtered_candidates.len(),
        "filtered pool accounts by scan token allowlist",
    );

    info!(
        amm_config_count = allowlist_filtered_candidates
            .iter()
            .map(|candidate| candidate.amm_config)
            .collect::<HashSet<_>>()
            .len(),
        "fetching raydium amm config accounts",
    );
    let amm_config_accounts = fetch_accounts_map_chunked(
        rpc_client,
        &allowlist_filtered_candidates
            .iter()
            .map(|candidate| candidate.amm_config)
            .collect::<Vec<_>>(),
    )
    .await?;
    info!(
        fetched_amm_config_accounts = amm_config_accounts.len(),
        "fetched raydium amm config accounts",
    );

    info!(
        allowed_trade_fee_rates = ?args.trade_fee_rates,
        "filtering pools by allowed trade fee rates",
    );
    let filtered_candidates = allowlist_filtered_candidates
        .into_iter()
        .filter_map(|candidate| {
            decode_amm_config(
                candidate.amm_config,
                amm_config_accounts.get(&candidate.amm_config)?,
            )
            .ok()
            .and_then(|config| {
                allowed_fee_rates
                    .contains(&config.trade_fee_rate)
                    .then_some((candidate, config))
            })
        })
        .collect::<Vec<_>>();
    info!(
        fee_filtered_pools = filtered_candidates.len(),
        "filtered pools by allowed trade fee rates",
    );

    info!(
        vault_account_count = filtered_candidates
            .iter()
            .flat_map(|(candidate, _)| [candidate.input_vault, candidate.output_vault])
            .collect::<HashSet<_>>()
            .len(),
        "fetching pool vault accounts",
    );
    let vault_accounts = fetch_accounts_map_chunked(
        rpc_client,
        &filtered_candidates
            .iter()
            .flat_map(|(candidate, _)| [candidate.input_vault, candidate.output_vault])
            .collect::<Vec<_>>(),
    )
    .await?;
    info!(
        fetched_vault_accounts = vault_accounts.len(),
        "fetched pool vault accounts",
    );

    info!(
        candidate_count = filtered_candidates.len(),
        input_amount = args.input_amount,
        "ranking filtered pools",
    );
    let mut ranked_candidates = filtered_candidates
        .into_iter()
        .filter_map(|(candidate, config)| {
            rank_pool_candidate(
                candidate,
                config,
                args.input_amount,
                &vault_accounts,
                wsol_usdc_price,
            )
            .ok()
        })
        .collect::<Vec<_>>();

    if args.min_estimated_tvl_usdc > 0.0 {
        let pre_tvl_filter_count = ranked_candidates.len();
        ranked_candidates.retain(|candidate| candidate.estimated_tvl_usdc >= args.min_estimated_tvl_usdc);
        info!(
            min_estimated_tvl_usdc = args.min_estimated_tvl_usdc,
            pre_tvl_filter_count,
            post_tvl_filter_count = ranked_candidates.len(),
            "filtered ranked pools by minimum estimated tvl",
        );
    }

    ranked_candidates.sort_unstable_by_key(|candidate| {
        (
            candidate.estimated_tvl_usdc.to_bits(),
            Reverse(candidate.estimated_price_impact_bps),
            Reverse(candidate.estimated_output_amount),
            Reverse(candidate.input_share_bps),
            candidate.pool,
        )
    });

    info!(
        input_mint = %args.input_mint,
        input_amount = args.input_amount,
        allowed_trade_fee_rates = ?args.trade_fee_rates,
        total_pool_accounts = pool_accounts.len(),
        matching_input_mint_pools = ranked_candidates.len(),
        "completed raydium cp-swap pool scan",
    );

    for (rank, candidate) in ranked_candidates.iter().take(args.top).enumerate() {
        info!(
            rank = rank + 1,
            pool = %candidate.pool,
            amm_config = %candidate.amm_config,
            output_mint = %candidate.output_mint,
            input_reserve = candidate.input_reserve,
            output_reserve = candidate.output_reserve,
            wsol_reserve = candidate.wsol_reserve,
            estimated_tvl_usdc = candidate.estimated_tvl_usdc,
            trade_fee_rate = candidate.trade_fee_rate,
            protocol_fee_rate = candidate.protocol_fee_rate,
            fund_fee_rate = candidate.fund_fee_rate,
            creator_fee_rate = candidate.creator_fee_rate,
            estimated_output_amount = candidate.estimated_output_amount,
            estimated_price_impact_bps = candidate.estimated_price_impact_bps,
            input_share_bps = candidate.input_share_bps,
            "raydium cp-swap candidate",
        );
    }

    let simulation_outcomes = if args.simulate_top > 0 {
        info!(
            simulate_top = args.simulate_top,
            available_ranked_candidates = ranked_candidates.len(),
            "starting simulation of top ranked pools",
        );
        simulate_top_candidates(
            rpc_client,
            signer.ok_or_else(|| {
                io::Error::other("--keypair is required when --simulate-top is greater than 0")
            })?,
            args,
            &ranked_candidates,
        )
        .await?
    } else {
        HashMap::new()
    };

    write_ranked_candidates_csv(
        args,
        &ranked_candidates,
        &token_allowlist.symbols_by_mint,
        &simulation_outcomes,
    )?;

    Ok(())
}

pub async fn scan_meteora_dlmm_pools(
    rpc_client: &RpcClient,
    args: &ScanMeteoraDlmmArgs,
) -> Result<(), BoxError> {
    let token_allowlist = load_dlmm_token_allowlist(args).await?;
    let token_metadata = load_jupiter_verified_token_metadata_csv()?;
    let wsol_metadata = token_metadata
        .get(&WSOL_MINT)
        .ok_or_else(|| io::Error::other("jupiter verified token csv is missing WSOL metadata"))?;
    info!(
        token_allowlist_source = args.token_allowlist_source.label(),
        token_allowlist_count = token_allowlist.mints.len(),
        token_metadata_count = token_metadata.len(),
        wsol_usdc_price = wsol_metadata.usd_price,
        "loaded meteora dlmm scan token metadata",
    );

    let pair_accounts = fetch_dlmm_pair_accounts_with_wsol_side(rpc_client).await?;
    info!(
        fetched_pairs = pair_accounts.len(),
        "fetched meteora dlmm lb pair accounts with wsol side",
    );

    let candidates = pair_accounts
        .iter()
        .filter_map(|(pair, account)| {
            match decode_dlmm_pair_candidate(*pair, account) {
                Ok(Some(candidate))
                    if token_allowlist.mints.contains(&candidate.non_wsol_mint)
                        && token_metadata.contains_key(&candidate.non_wsol_mint) =>
                {
                    Some(Ok(candidate))
                }
                Ok(_) => None,
                Err(err) => Some(Err(err)),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    info!(
        allowlist_filtered_pairs = candidates.len(),
        "filtered meteora dlmm pairs by wsol side and bluechip/verified token metadata",
    );

    let reserve_accounts = fetch_accounts_map_chunked(
        rpc_client,
        &candidates
            .iter()
            .flat_map(|candidate| [candidate.reserve_x, candidate.reserve_y])
            .collect::<Vec<_>>(),
    )
    .await?;
    info!(
        fetched_reserve_accounts = reserve_accounts.len(),
        "fetched meteora dlmm reserve token accounts",
    );
    let active_bin_array_addresses = candidates
        .iter()
        .map(|candidate| {
            target_bin_array_address(candidate.pair, target_bin_id_for_wsol_deposit(candidate))
        })
        .collect::<Vec<_>>();
    info!(
        active_bin_array_count = active_bin_array_addresses.len(),
        "fetching meteora dlmm active bin array accounts",
    );
    let active_bin_array_accounts =
        fetch_existing_accounts_map_chunked(rpc_client, &active_bin_array_addresses).await?;
    info!(
        fetched_active_bin_array_accounts = active_bin_array_accounts.len(),
        "fetched meteora dlmm active bin array accounts",
    );

    let mut ranked_candidates = candidates
        .into_iter()
        .filter_map(|candidate| {
            rank_dlmm_pair_candidate(
                candidate,
                &reserve_accounts,
                &active_bin_array_accounts,
                &token_metadata,
                wsol_metadata.usd_price,
                &args.target_bin_mode,
            )
            .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;

    let pre_filter_count = ranked_candidates.len();
    ranked_candidates.retain(|candidate| {
        candidate.wsol_discount_bps <= -args.min_wsol_discount_bps
            && candidate.estimated_tvl_usdc >= args.min_estimated_tvl_usdc
            && target_bin_matches_mode(candidate, &args.target_bin_mode)
            && candidate.wsol_is_cheap_for_arbers
    });
    info!(
        pre_filter_count,
        post_filter_count = ranked_candidates.len(),
        min_wsol_discount_bps = args.min_wsol_discount_bps,
        min_estimated_tvl_usdc = args.min_estimated_tvl_usdc,
        "filtered meteora dlmm candidates by cheap wsol price, tvl, and empty active bin",
    );

    ranked_candidates.sort_by(|left, right| {
        left.wsol_discount_bps
            .cmp(&right.wsol_discount_bps)
            .then_with(|| {
                right
                    .estimated_tvl_usdc
                    .partial_cmp(&left.estimated_tvl_usdc)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.pair.cmp(&right.pair))
    });

    for (rank, candidate) in ranked_candidates.iter().take(args.top).enumerate() {
        info!(
            rank = rank + 1,
            pair = %candidate.pair,
            token_x_mint = %candidate.token_x_mint,
            token_y_mint = %candidate.token_y_mint,
            non_wsol_mint = %candidate.non_wsol_mint,
            non_wsol_symbol = %candidate.non_wsol_symbol,
            active_id = candidate.active_id,
            bin_step = candidate.bin_step,
            target_bin_id = candidate.target_bin_id,
            target_bin_array = %candidate.target_bin_array,
            target_bin_array_index = candidate.target_bin_array_index,
            target_bin_index = candidate.target_bin_index,
            wsol_side = candidate.wsol_side,
            target_bin_amount_x = candidate.target_bin_amount_x,
            target_bin_amount_y = candidate.target_bin_amount_y,
            target_bin_liquidity_supply = candidate.target_bin_liquidity_supply,
            target_bin_open_order_amount = candidate.target_bin_open_order_amount,
            active_price_y_per_x = candidate.active_price_y_per_x,
            fair_price_y_per_x = candidate.fair_price_y_per_x,
            implied_wsol_usdc_price = candidate.implied_wsol_usdc_price,
            reference_wsol_usdc_price = candidate.reference_wsol_usdc_price,
            wsol_discount_bps = candidate.wsol_discount_bps,
            wsol_is_cheap_for_arbers = candidate.wsol_is_cheap_for_arbers,
            estimated_tvl_usdc = candidate.estimated_tvl_usdc,
            token_volume_24h_usd = candidate.token_volume_24h_usd,
            "meteora dlmm wsol-cheap candidate",
        );
    }

    write_dlmm_candidates_csv(args, &ranked_candidates)?;
    Ok(())
}

async fn fetch_dlmm_pair_accounts_with_wsol_side(
    rpc_client: &RpcClient,
) -> Result<Vec<(Address, Account)>, BoxError> {
    let mut pairs_by_address = HashMap::new();
    for (side, mint_offset) in [
        ("token_x_mint", LB_PAIR_TOKEN_X_MINT_OFFSET),
        ("token_y_mint", LB_PAIR_TOKEN_Y_MINT_OFFSET),
    ] {
        info!(
            side,
            mint_offset,
            wsol_mint = %WSOL_MINT,
            discriminator = ?dlmm::accounts::LbPair::DISCRIMINATOR,
            "fetching meteora dlmm pairs for wsol side",
        );
        let side_pair_accounts = rpc_client
            .get_program_ui_accounts_with_config(
                &METEORA_DLMM_PROGRAM_ID,
                RpcProgramAccountsConfig {
                    filters: Some(vec![
                        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                            0,
                            dlmm::accounts::LbPair::DISCRIMINATOR.to_vec(),
                        )),
                        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                            mint_offset,
                            WSOL_MINT.as_ref().to_vec(),
                        )),
                    ]),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64Zstd),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                },
            )
            .await?;
        info!(
            side,
            matching_pair_accounts = side_pair_accounts.len(),
            "fetched meteora dlmm pairs for wsol side",
        );
        for (pair, account) in side_pair_accounts {
            let account = account.to_account().ok_or_else(|| {
                io::Error::other(format!("failed to decode meteora dlmm pair account {pair}"))
            })?;
            pairs_by_address.entry(pair).or_insert(account);
        }
    }

    Ok(pairs_by_address.into_iter().collect())
}

async fn fetch_pool_accounts_with_wsol_side(
    rpc_client: &RpcClient,
) -> Result<Vec<(Address, Account)>, BoxError> {
    let mut pools_by_address = HashMap::new();

    for (side, mint_offset) in [
        ("token_0_mint", POOL_STATE_TOKEN_0_MINT_OFFSET),
        ("token_1_mint", POOL_STATE_TOKEN_1_MINT_OFFSET),
    ] {
        info!(
            side,
            mint_offset,
            wsol_mint = %WSOL_MINT,
            "fetching raydium cp-swap pools for wsol side",
        );
        let side_pool_accounts = rpc_client
            .get_program_ui_accounts_with_config(
                &RAYDIUM_CP_SWAP_PROGRAM_ID,
                RpcProgramAccountsConfig {
                    filters: Some(vec![
                        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                            0,
                            POOL_STATE_DISCRIMINATOR.to_vec(),
                        )),
                        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                            mint_offset,
                            WSOL_MINT.as_ref().to_vec(),
                        )),
                    ]),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64Zstd),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                },
            )
            .await?;
        info!(
            side,
            matching_pool_accounts = side_pool_accounts.len(),
            "fetched raydium cp-swap pools for wsol side",
        );
        for (pool, account) in side_pool_accounts {
            let account = account.to_account().ok_or_else(|| {
                io::Error::other(format!("failed to decode raydium cp-swap pool account {pool}"))
            })?;
            pools_by_address.entry(pool).or_insert(account);
        }
    }

    Ok(pools_by_address.into_iter().collect())
}

async fn simulate_top_candidates(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ScanRaydiumCpSwapArgs,
    ranked_candidates: &[RankedPoolCandidate],
) -> Result<HashMap<Address, SimulationCsvOutcome>, BoxError> {
    let simulate_count = args.simulate_top.min(ranked_candidates.len());
    if simulate_count == 0 {
        info!("no ranked pools available for simulation");
        return Ok(HashMap::new());
    }

    let target_slot = rpc_client
        .get_slot_with_commitment(CommitmentConfig::processed())
        .await?;
    info!(
        simulate_count,
        target_slot, "resolved simulation target slot for top ranked pools",
    );
    let transaction_modes = ranked_candidates
        .iter()
        .take(simulate_count)
        .map(|candidate| {
            ResolvedTransactionMode::RaydiumCpSwap(ResolvedRaydiumCpSwapArgs {
                pool: candidate.pool,
                input_mint: args.input_mint,
                input_amount: args.input_amount,
            })
        })
        .collect::<Vec<_>>();

    run_startup_setup(rpc_client, signer, &transaction_modes).await?;
    info!(
        simulate_count,
        "completed startup setup for simulated top ranked pools",
    );

    let mut simulation_outcomes = HashMap::with_capacity(simulate_count);
    for (rank, candidate) in ranked_candidates.iter().take(simulate_count).enumerate() {
        info!(
            rank = rank + 1,
            pool = %candidate.pool,
            output_mint = %candidate.output_mint,
            "simulating ranked raydium cp-swap candidate",
        );
        match simulate_candidate(rpc_client, signer, args, candidate, target_slot).await {
            Ok(simulation_check) => {
                info!(
                    rank = rank + 1,
                    pool = %candidate.pool,
                    output_mint = %candidate.output_mint,
                    expected_input_amount = args.input_amount,
                    input_spent = simulation_check.input_spent,
                    returned_input_amount = simulation_check.returned_input_amount,
                    bundle_input_delta = simulation_check.bundle_input_delta,
                    input_spent_within_tolerance = simulation_check.input_spent_within_tolerance,
                    returned_input_within_tolerance = simulation_check.returned_input_within_tolerance,
                    round_trip_verified = simulation_check.round_trip_verified,
                    tolerance_bps = ROUND_TRIP_TOLERANCE_BPS,
                    "simulated raydium cp-swap candidate",
                );
                simulation_outcomes.insert(
                    candidate.pool,
                    SimulationCsvOutcome {
                        input_spent: Some(simulation_check.input_spent),
                        returned_input_amount: Some(simulation_check.returned_input_amount),
                        bundle_input_delta: Some(simulation_check.bundle_input_delta),
                        input_spent_within_tolerance: Some(
                            simulation_check.input_spent_within_tolerance,
                        ),
                        returned_input_within_tolerance: Some(
                            simulation_check.returned_input_within_tolerance,
                        ),
                        round_trip_verified: Some(simulation_check.round_trip_verified),
                        simulation_error: None,
                    },
                );
            }
            Err(err) => {
                error!(
                    rank = rank + 1,
                    pool = %candidate.pool,
                    output_mint = %candidate.output_mint,
                    err = %err,
                    "failed to simulate raydium cp-swap candidate",
                );
                simulation_outcomes.insert(
                    candidate.pool,
                    SimulationCsvOutcome {
                        input_spent: None,
                        returned_input_amount: None,
                        bundle_input_delta: None,
                        input_spent_within_tolerance: None,
                        returned_input_within_tolerance: None,
                        round_trip_verified: None,
                        simulation_error: Some(err.to_string()),
                    },
                );
            }
        }
    }

    Ok(simulation_outcomes)
}

async fn simulate_candidate(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ScanRaydiumCpSwapArgs,
    candidate: &RankedPoolCandidate,
    target_slot: Slot,
) -> Result<SimulationCheck, BoxError> {
    let blockhash = rpc_client.get_latest_blockhash().await?;
    let transaction_mode = ResolvedTransactionMode::RaydiumCpSwap(ResolvedRaydiumCpSwapArgs {
        pool: candidate.pool,
        input_mint: args.input_mint,
        input_amount: args.input_amount,
    });
    let prepared_transactions = prepare_transactions(
        rpc_client,
        signer,
        blockhash,
        &transaction_mode,
        target_slot,
    )
    .await?;
    let transactions = build_transactions(
        &prepared_transactions,
        signer,
        blockhash,
        target_slot,
        false,
    )?;
    let (pre_execution_accounts_configs, post_execution_accounts_configs) =
        simulation_account_configs(&prepared_transactions, transactions.len())?;
    let simulation_result = simulate_bundle_with_accounts(
        rpc_client,
        target_slot,
        &transactions,
        pre_execution_accounts_configs,
        post_execution_accounts_configs,
    )
    .await?;
    log_post_simulation(&prepared_transactions, &simulation_result)?;
    let summary = round_trip_summary(&prepared_transactions, &simulation_result)?;
    let input_spent_within_tolerance = within_tolerance_bps(
        summary.input_spent,
        args.input_amount,
        ROUND_TRIP_TOLERANCE_BPS,
    );
    let returned_input_within_tolerance = within_tolerance_bps(
        summary.returned_input_amount,
        args.input_amount,
        ROUND_TRIP_TOLERANCE_BPS,
    );

    Ok(SimulationCheck {
        input_spent: summary.input_spent,
        returned_input_amount: summary.returned_input_amount,
        bundle_input_delta: summary.bundle_input_delta,
        input_spent_within_tolerance,
        returned_input_within_tolerance,
        round_trip_verified: input_spent_within_tolerance && returned_input_within_tolerance,
    })
}

fn round_trip_summary(
    prepared_transactions: &PreparedTransactions,
    simulation_result: &solana_rpc_client_api::bundles::RpcSimulateBundleResult,
) -> Result<crate::transactions::RoundTripSimulationSummary, BoxError> {
    round_trip_simulation_summary(prepared_transactions, simulation_result)?.ok_or_else(|| {
        io::Error::other("missing round-trip simulation summary for non-raydium transaction mode")
            .into()
    })
}

#[derive(Clone, Copy, Debug)]
struct SimulationCheck {
    input_spent: u64,
    returned_input_amount: u64,
    bundle_input_delta: i128,
    input_spent_within_tolerance: bool,
    returned_input_within_tolerance: bool,
    round_trip_verified: bool,
}

#[derive(Clone, Debug)]
struct SimulationCsvOutcome {
    input_spent: Option<u64>,
    returned_input_amount: Option<u64>,
    bundle_input_delta: Option<i128>,
    input_spent_within_tolerance: Option<bool>,
    returned_input_within_tolerance: Option<bool>,
    round_trip_verified: Option<bool>,
    simulation_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct JupiterVerifiedToken {
    id: String,
    symbol: Option<String>,
    #[serde(rename = "stats24h")]
    stats_24h: Option<JupiterTokenStats24h>,
}

#[derive(Clone, Debug, Deserialize)]
struct JupiterTokenStats24h {
    #[serde(rename = "buyVolume")]
    buy_volume: Option<f64>,
    #[serde(rename = "sellVolume")]
    sell_volume: Option<f64>,
}

fn decode_pool_candidate(
    pool: Address,
    account: &Account,
    input_mint: Address,
) -> Result<Option<PoolCandidate>, BoxError> {
    if account.owner != RAYDIUM_CP_SWAP_PROGRAM_ID {
        return Ok(None);
    }

    let raw = decode_pool_state(pool, &account.data)?;
    let non_wsol_mint = pool_non_wsol_mint(&raw).ok_or_else(|| {
        io::Error::other(format!("pool {} does not have exactly one wsol side", pool))
    })?;
    let maybe_candidate = if raw.token_0_mint == input_mint {
        Some(PoolCandidate {
            pool,
            amm_config: raw.amm_config,
            output_mint: raw.token_1_mint,
            non_wsol_mint,
            input_vault: raw.token_0_vault,
            output_vault: raw.token_1_vault,
            wsol_vault: if raw.token_0_mint == WSOL_MINT {
                raw.token_0_vault
            } else {
                raw.token_1_vault
            },
        })
    } else if raw.token_1_mint == input_mint {
        Some(PoolCandidate {
            pool,
            amm_config: raw.amm_config,
            output_mint: raw.token_0_mint,
            non_wsol_mint,
            input_vault: raw.token_1_vault,
            output_vault: raw.token_0_vault,
            wsol_vault: if raw.token_0_mint == WSOL_MINT {
                raw.token_0_vault
            } else {
                raw.token_1_vault
            },
        })
    } else {
        None
    };

    Ok(maybe_candidate)
}

fn decode_dlmm_pair_candidate(
    pair: Address,
    account: &Account,
) -> Result<Option<DlmmPairCandidate>, BoxError> {
    if account.owner != METEORA_DLMM_PROGRAM_ID {
        return Ok(None);
    }
    let lb_pair = bytemuck::try_pod_read_unaligned::<dlmm::accounts::LbPair>(
        account
            .data
            .get(8..8 + size_of::<dlmm::accounts::LbPair>())
            .ok_or_else(|| io::Error::other(format!("dlmm lb pair {pair} data is too short")))?,
    )
    .map_err(|err| io::Error::other(format!("failed to decode dlmm lb pair {pair}: {err}")))?;
    let token_x_mint = pubkey_to_address(lb_pair.token_x_mint);
    let token_y_mint = pubkey_to_address(lb_pair.token_y_mint);
    let non_wsol_mint = match (token_x_mint == WSOL_MINT, token_y_mint == WSOL_MINT) {
        (true, false) => token_y_mint,
        (false, true) => token_x_mint,
        _ => return Ok(None),
    };

    Ok(Some(DlmmPairCandidate {
        pair,
        token_x_mint,
        token_y_mint,
        reserve_x: pubkey_to_address(lb_pair.reserve_x),
        reserve_y: pubkey_to_address(lb_pair.reserve_y),
        non_wsol_mint,
        active_id: lb_pair.active_id,
        bin_step: lb_pair.bin_step,
        status: lb_pair.status,
    }))
}

fn rank_dlmm_pair_candidate(
    candidate: DlmmPairCandidate,
    reserve_accounts: &HashMap<Address, Account>,
    active_bin_array_accounts: &HashMap<Address, Account>,
    token_metadata: &HashMap<Address, TokenMetadata>,
    wsol_usdc_price: f64,
    target_bin_mode: &DlmmTargetBinMode,
) -> Result<Option<RankedDlmmPairCandidate>, BoxError> {
    let non_wsol_metadata = token_metadata
        .get(&candidate.non_wsol_mint)
        .ok_or_else(|| io::Error::other("missing non-wsol token metadata"))?;
    let x_metadata = token_metadata
        .get(&candidate.token_x_mint)
        .ok_or_else(|| io::Error::other("missing token x metadata"))?;
    let y_metadata = token_metadata
        .get(&candidate.token_y_mint)
        .ok_or_else(|| io::Error::other("missing token y metadata"))?;
    if !(non_wsol_metadata.usd_price.is_finite() && non_wsol_metadata.usd_price > 0.0) {
        return Ok(None);
    }

    let reserve_x_amount = parse_token_account_amount(
        reserve_accounts
            .get(&candidate.reserve_x)
            .ok_or_else(|| io::Error::other(format!("missing reserve_x {}", candidate.reserve_x)))?,
    )?;
    let reserve_y_amount = parse_token_account_amount(
        reserve_accounts
            .get(&candidate.reserve_y)
            .ok_or_else(|| io::Error::other(format!("missing reserve_y {}", candidate.reserve_y)))?,
    )?;
    let wsol_reserve_amount = if candidate.token_x_mint == WSOL_MINT {
        reserve_x_amount
    } else {
        reserve_y_amount
    };
    if wsol_reserve_amount == 0 {
        return Ok(None);
    }
    let target_bin_id = match target_bin_mode {
        DlmmTargetBinMode::Empty => target_bin_id_for_wsol_deposit(&candidate),
        DlmmTargetBinMode::OppositeTokenOnly => candidate.active_id,
    };
    let Some(target_bin_info) =
        decode_target_bin_info(candidate.pair, target_bin_id, active_bin_array_accounts)?
    else {
        return Ok(None);
    };
    let target_bin_is_empty = target_bin_info.amount_x == 0
        && target_bin_info.amount_y == 0
        && target_bin_info.liquidity_supply == 0
        && target_bin_info.open_order_amount == 0
        && target_bin_info.total_processing_order_amount == 0
        && target_bin_info.processed_order_remaining_amount == 0;
    let reserve_x_ui_amount =
        reserve_x_amount as f64 / 10_f64.powi(i32::from(x_metadata.decimals));
    let reserve_y_ui_amount =
        reserve_y_amount as f64 / 10_f64.powi(i32::from(y_metadata.decimals));
    let wsol_reserve_ui_amount = wsol_reserve_amount as f64 / WSOL_DECIMALS_FACTOR;

    let raw_price_y_per_x =
        (1.0 + (f64::from(candidate.bin_step) / 10_000.0)).powi(target_bin_id);
    let active_price_y_per_x = raw_price_y_per_x
        * 10_f64.powi(i32::from(x_metadata.decimals) - i32::from(y_metadata.decimals));
    if !(active_price_y_per_x.is_finite() && active_price_y_per_x > 0.0) {
        return Ok(None);
    }

    let fair_price_y_per_x = y_metadata.usd_price.recip() * x_metadata.usd_price;
    let implied_wsol_usdc_price = if candidate.token_x_mint == WSOL_MINT {
        active_price_y_per_x * y_metadata.usd_price
    } else {
        x_metadata.usd_price / active_price_y_per_x
    };
    if !(implied_wsol_usdc_price.is_finite() && implied_wsol_usdc_price > 0.0) {
        return Ok(None);
    }

    let wsol_discount_bps =
        (((implied_wsol_usdc_price / wsol_usdc_price) - 1.0) * 10_000.0).round() as i64;
    let wsol_is_cheap_for_arbers = wsol_discount_bps < 0;
    let estimated_tvl_usdc = 2.0 * wsol_reserve_ui_amount * wsol_usdc_price;

    Ok(Some(RankedDlmmPairCandidate {
        pair: candidate.pair,
        token_x_mint: candidate.token_x_mint,
        token_y_mint: candidate.token_y_mint,
        non_wsol_mint: candidate.non_wsol_mint,
        non_wsol_symbol: non_wsol_metadata.symbol.clone(),
        active_id: candidate.active_id,
        target_bin_id,
        bin_step: candidate.bin_step,
        status: candidate.status,
        target_bin_array: target_bin_info.bin_array,
        target_bin_array_index: target_bin_info.bin_array_index,
        target_bin_index: target_bin_info.bin_index,
        wsol_side: if candidate.token_x_mint == WSOL_MINT {
            "x"
        } else {
            "y"
        },
        target_bin_amount_x: target_bin_info.amount_x,
        target_bin_amount_y: target_bin_info.amount_y,
        target_bin_liquidity_supply: target_bin_info.liquidity_supply,
        target_bin_open_order_amount: target_bin_info.open_order_amount,
        target_bin_is_empty,
        reserve_x_ui_amount,
        reserve_y_ui_amount,
        wsol_reserve_ui_amount,
        estimated_tvl_usdc,
        active_price_y_per_x,
        fair_price_y_per_x,
        implied_wsol_usdc_price,
        reference_wsol_usdc_price: wsol_usdc_price,
        wsol_discount_bps,
        wsol_is_cheap_for_arbers,
        token_volume_24h_usd: non_wsol_metadata.volume_24h_usd,
    }))
}

fn decode_target_bin_info(
    pair: Address,
    target_bin_id: i32,
    active_bin_array_accounts: &HashMap<Address, Account>,
) -> Result<Option<TargetBinInfo>, BoxError> {
    let bin_array = target_bin_array_address(pair, target_bin_id);
    let Some(account) = active_bin_array_accounts.get(&bin_array) else {
        return Ok(None);
    };
    if account.owner != METEORA_DLMM_PROGRAM_ID {
        return Err(io::Error::other(format!(
            "active bin array {bin_array} has unexpected owner {}",
            account.owner
        ))
        .into());
    }
    if account.data.get(..8) != Some(dlmm::accounts::BinArray::DISCRIMINATOR) {
        return Err(io::Error::other(format!(
            "active bin array {bin_array} has unexpected discriminator"
        ))
        .into());
    }
    let decoded = bytemuck::try_pod_read_unaligned::<dlmm::accounts::BinArray>(
        account
            .data
            .get(8..8 + size_of::<dlmm::accounts::BinArray>())
            .ok_or_else(|| io::Error::other(format!("bin array {bin_array} data is too short")))?,
    )
    .map_err(|err| io::Error::other(format!("failed to decode bin array {bin_array}: {err}")))?;
    let bin_array_index = bin_array_index_for_bin_id(target_bin_id);
    if decoded.index != bin_array_index {
        return Err(io::Error::other(format!(
            "bin array {bin_array} index mismatch: expected {bin_array_index}, got {}",
            decoded.index
        ))
        .into());
    }
    let bin_index = bin_index_in_array(target_bin_id);
    let bin = decoded.bins[bin_index];
    Ok(Some(TargetBinInfo {
        bin_array,
        bin_array_index,
        bin_index,
        amount_x: bin.amount_x,
        amount_y: bin.amount_y,
        liquidity_supply: bin.liquidity_supply,
        open_order_amount: bin.open_order_amount,
        total_processing_order_amount: bin.total_processing_order_amount,
        processed_order_remaining_amount: bin.processed_order_remaining_amount,
    }))
}

fn target_bin_matches_mode(
    candidate: &RankedDlmmPairCandidate,
    mode: &DlmmTargetBinMode,
) -> bool {
    match mode {
        DlmmTargetBinMode::Empty => candidate.target_bin_is_empty,
        DlmmTargetBinMode::OppositeTokenOnly => {
            candidate.target_bin_liquidity_supply > 0
                && candidate.target_bin_open_order_amount == 0
                && match candidate.wsol_side {
                    "x" => candidate.target_bin_amount_x == 0 && candidate.target_bin_amount_y > 0,
                    "y" => candidate.target_bin_amount_y == 0 && candidate.target_bin_amount_x > 0,
                    _ => false,
                }
        }
    }
}

fn target_bin_array_address(pair: Address, target_bin_id: i32) -> Address {
    let bin_array_index = bin_array_index_for_bin_id(target_bin_id);
    derive_bin_array_address(pair, bin_array_index)
}

fn target_bin_id_for_wsol_deposit(candidate: &DlmmPairCandidate) -> i32 {
    if candidate.token_x_mint == WSOL_MINT {
        candidate.active_id + 1
    } else {
        candidate.active_id - 1
    }
}

fn derive_bin_array_address(pair: Address, bin_array_index: i64) -> Address {
    let bin_array_index_bytes = bin_array_index.to_le_bytes();
    let pair_pubkey = AnchorPubkey::new_from_array(address_to_array(pair));
    let program_pubkey = AnchorPubkey::new_from_array(address_to_array(METEORA_DLMM_PROGRAM_ID));
    let (bin_array, _) = AnchorPubkey::find_program_address(
        &[b"bin_array", pair_pubkey.as_ref(), &bin_array_index_bytes],
        &program_pubkey,
    );
    pubkey_to_address(bin_array)
}

fn bin_array_index_for_bin_id(bin_id: i32) -> i64 {
    i64::from(bin_id.div_euclid(DLMM_BINS_PER_ARRAY))
}

fn bin_index_in_array(bin_id: i32) -> usize {
    bin_id.rem_euclid(DLMM_BINS_PER_ARRAY) as usize
}

fn address_to_array(address: Address) -> [u8; 32] {
    address
        .as_ref()
        .try_into()
        .expect("solana address must be 32 bytes")
}

fn pubkey_to_address(pubkey: impl AsRef<[u8]>) -> Address {
    Address::new_from_array(
        pubkey
            .as_ref()
            .try_into()
            .expect("solana pubkey must be 32 bytes"),
    )
}

fn decode_pool_state(pool: Address, data: &[u8]) -> Result<PoolStateRaw, BoxError> {
    let raw = bytemuck::try_pod_read_unaligned::<PoolStateRaw>(
        data.get(..size_of::<PoolStateRaw>()).ok_or_else(|| {
            io::Error::other(format!(
                "pool {} account data is too short: {} bytes",
                pool,
                data.len()
            ))
        })?,
    )
    .map_err(|err| io::Error::other(format!("failed to decode pool {}: {err}", pool)))?;

    if raw.discriminator != POOL_STATE_DISCRIMINATOR {
        return Err(
            io::Error::other(format!("pool {} has unexpected pool discriminator", pool)).into(),
        );
    }

    Ok(raw)
}

fn decode_amm_config(amm_config: Address, account: &Account) -> Result<AmmConfigInfo, BoxError> {
    if account.owner != RAYDIUM_CP_SWAP_PROGRAM_ID {
        return Err(io::Error::other(format!(
            "amm config {} is not owned by raydium cp-swap",
            amm_config
        ))
        .into());
    }

    let raw = bytemuck::try_pod_read_unaligned::<AmmConfigRaw>(
        account
            .data
            .get(..size_of::<AmmConfigRaw>())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "amm config {} data is too short: {} bytes",
                    amm_config,
                    account.data.len()
                ))
            })?,
    )
    .map_err(|err| {
        io::Error::other(format!("failed to decode amm config {}: {err}", amm_config))
    })?;

    if raw.discriminator != AMM_CONFIG_DISCRIMINATOR {
        return Err(io::Error::other(format!(
            "amm config {} has unexpected discriminator",
            amm_config
        ))
        .into());
    }

    Ok(AmmConfigInfo {
        trade_fee_rate: raw.trade_fee_rate,
        protocol_fee_rate: raw.protocol_fee_rate,
        fund_fee_rate: raw.fund_fee_rate,
        creator_fee_rate: raw.creator_fee_rate,
    })
}

fn rank_pool_candidate(
    candidate: PoolCandidate,
    config: AmmConfigInfo,
    input_amount: u64,
    vault_accounts: &HashMap<Address, Account>,
    wsol_usdc_price: f64,
) -> Result<RankedPoolCandidate, BoxError> {
    let input_reserve =
        parse_token_account_amount(vault_accounts.get(&candidate.input_vault).ok_or_else(
            || io::Error::other(format!("missing input vault {}", candidate.input_vault)),
        )?)?;
    let output_reserve =
        parse_token_account_amount(vault_accounts.get(&candidate.output_vault).ok_or_else(
            || io::Error::other(format!("missing output vault {}", candidate.output_vault)),
        )?)?;
    let wsol_reserve =
        parse_token_account_amount(vault_accounts.get(&candidate.wsol_vault).ok_or_else(
            || io::Error::other(format!("missing wsol vault {}", candidate.wsol_vault)),
        )?)?;

    if input_reserve == 0 || output_reserve == 0 {
        return Err(io::Error::other(format!("pool {} has zero reserves", candidate.pool)).into());
    }

    let estimated_output_amount = compute_constant_product_output_amount(
        input_amount,
        input_reserve,
        output_reserve,
        config.trade_fee_rate,
    )?;
    let estimated_price_impact_bps = compute_price_impact_bps(
        input_amount,
        input_reserve,
        output_reserve,
        estimated_output_amount,
        config.trade_fee_rate,
    )?;
    let input_share_bps = ((u128::from(input_amount) * 10_000) / u128::from(input_reserve)) as u64;
    let estimated_tvl_usdc = 2.0 * (wsol_reserve as f64 / WSOL_DECIMALS_FACTOR) * wsol_usdc_price;

    Ok(RankedPoolCandidate {
        pool: candidate.pool,
        amm_config: candidate.amm_config,
        output_mint: candidate.output_mint,
        non_wsol_mint: candidate.non_wsol_mint,
        input_reserve,
        output_reserve,
        wsol_reserve,
        estimated_tvl_usdc,
        trade_fee_rate: config.trade_fee_rate,
        protocol_fee_rate: config.protocol_fee_rate,
        fund_fee_rate: config.fund_fee_rate,
        creator_fee_rate: config.creator_fee_rate,
        estimated_output_amount,
        estimated_price_impact_bps,
        input_share_bps,
    })
}

async fn fetch_accounts_map_chunked(
    rpc_client: &RpcClient,
    addresses: &[Address],
) -> Result<HashMap<Address, Account>, BoxError> {
    let unique_addresses = addresses
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut accounts_by_address = HashMap::with_capacity(unique_addresses.len());

    for chunk in unique_addresses.chunks(RPC_BATCH_SIZE) {
        let accounts = rpc_client.get_multiple_accounts(chunk).await?;
        for (address, account) in chunk.iter().copied().zip(accounts.into_iter()) {
            let account = account
                .ok_or_else(|| io::Error::other(format!("missing account data for {}", address)))?;
            accounts_by_address.insert(address, account);
        }
    }

    Ok(accounts_by_address)
}

async fn fetch_existing_accounts_map_chunked(
    rpc_client: &RpcClient,
    addresses: &[Address],
) -> Result<HashMap<Address, Account>, BoxError> {
    let unique_addresses = addresses
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut accounts_by_address = HashMap::with_capacity(unique_addresses.len());

    for chunk in unique_addresses.chunks(RPC_BATCH_SIZE) {
        let accounts = rpc_client.get_multiple_accounts(chunk).await?;
        for (address, account) in chunk.iter().copied().zip(accounts.into_iter()) {
            if let Some(account) = account {
                accounts_by_address.insert(address, account);
            }
        }
    }

    Ok(accounts_by_address)
}

fn parse_token_account_amount(account: &Account) -> Result<u64, BoxError> {
    let amount_bytes = account
        .data
        .get(TOKEN_ACCOUNT_AMOUNT_OFFSET..TOKEN_ACCOUNT_AMOUNT_OFFSET + 8)
        .ok_or_else(|| io::Error::other("token account data too short to read amount"))?;
    Ok(u64::from_le_bytes(amount_bytes.try_into().map_err(
        |_| io::Error::other("failed to decode token account amount bytes"),
    )?))
}

fn compute_constant_product_output_amount(
    input_amount: u64,
    input_reserve: u64,
    output_reserve: u64,
    trade_fee_rate: u64,
) -> Result<u64, BoxError> {
    let effective_input = (u128::from(input_amount)
        * u128::from(1_000_000_u64.saturating_sub(trade_fee_rate)))
        / 1_000_000_u128;

    if effective_input == 0 {
        return Ok(0);
    }

    let numerator = effective_input * u128::from(output_reserve);
    let denominator = u128::from(input_reserve) + effective_input;
    Ok((numerator / denominator) as u64)
}

fn compute_price_impact_bps(
    input_amount: u64,
    input_reserve: u64,
    output_reserve: u64,
    actual_output_amount: u64,
    trade_fee_rate: u64,
) -> Result<u64, BoxError> {
    let effective_input = (u128::from(input_amount)
        * u128::from(1_000_000_u64.saturating_sub(trade_fee_rate)))
        / 1_000_000_u128;
    if effective_input == 0 || input_reserve == 0 || output_reserve == 0 {
        return Ok(0);
    }

    let expected_output_amount =
        (effective_input * u128::from(output_reserve)) / u128::from(input_reserve);
    if expected_output_amount == 0 {
        return Ok(0);
    }

    let shortfall = expected_output_amount.saturating_sub(u128::from(actual_output_amount));
    Ok(((shortfall * 10_000) / expected_output_amount) as u64)
}

fn within_tolerance_bps(actual_amount: u64, expected_amount: u64, tolerance_bps: u64) -> bool {
    let tolerance_amount =
        ((u128::from(expected_amount) * u128::from(tolerance_bps)) + 9_999) / 10_000;
    let lower_bound = u128::from(expected_amount).saturating_sub(tolerance_amount);
    let upper_bound = u128::from(expected_amount) + tolerance_amount;
    (lower_bound..=upper_bound).contains(&u128::from(actual_amount))
}

fn defillama_token_allowlist_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(DEFI_LLAMA_SOLANA_TOP200_PROTOCOL_TOKEN_MAPPING_CSV)
}

fn jupiter_verified_tokens_csv_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(JUPITER_VERIFIED_TOKENS_CSV)
}

async fn load_token_allowlist(args: &ScanRaydiumCpSwapArgs) -> Result<TokenAllowlist, BoxError> {
    match args.token_allowlist_source {
        TokenAllowlistSource::JupiterVerified => {
            fetch_jupiter_verified_token_allowlist(
                args.jupiter_api_key.as_deref(),
                args.min_token_volume_24h_usd,
            )
            .await
        }
        TokenAllowlistSource::JupiterVerifiedCsv => {
            load_jupiter_verified_token_allowlist_csv(args.min_token_volume_24h_usd)
        }
        TokenAllowlistSource::DefillamaTop200 => load_defillama_solana_token_allowlist(),
        TokenAllowlistSource::Union => {
            let mut allowlist = load_defillama_solana_token_allowlist()?;
            let jupiter_allowlist = fetch_jupiter_verified_token_allowlist(
                args.jupiter_api_key.as_deref(),
                args.min_token_volume_24h_usd,
            )
            .await?;
            allowlist.mints.extend(jupiter_allowlist.mints);
            allowlist
                .symbols_by_mint
                .extend(jupiter_allowlist.symbols_by_mint);
            Ok(allowlist)
        }
    }
}

async fn load_dlmm_token_allowlist(args: &ScanMeteoraDlmmArgs) -> Result<TokenAllowlist, BoxError> {
    match args.token_allowlist_source {
        TokenAllowlistSource::JupiterVerified => {
            fetch_jupiter_verified_token_allowlist(
                args.jupiter_api_key.as_deref(),
                args.min_token_volume_24h_usd,
            )
            .await
        }
        TokenAllowlistSource::JupiterVerifiedCsv => {
            load_jupiter_verified_token_allowlist_csv(args.min_token_volume_24h_usd)
        }
        TokenAllowlistSource::DefillamaTop200 => load_defillama_solana_token_allowlist(),
        TokenAllowlistSource::Union => {
            let mut allowlist = load_defillama_solana_token_allowlist()?;
            let jupiter_allowlist = fetch_jupiter_verified_token_allowlist(
                args.jupiter_api_key.as_deref(),
                args.min_token_volume_24h_usd,
            )
            .await?;
            allowlist.mints.extend(jupiter_allowlist.mints);
            allowlist
                .symbols_by_mint
                .extend(jupiter_allowlist.symbols_by_mint);
            Ok(allowlist)
        }
    }
}

fn load_defillama_solana_token_allowlist() -> Result<TokenAllowlist, BoxError> {
    let path = defillama_token_allowlist_path();
    let mut reader = ReaderBuilder::new().trim(csv::Trim::All).from_path(&path)?;
    let headers = reader.headers()?.clone();
    let solana_token_mint_index = headers
        .iter()
        .position(|header| header == "solana_token_mint")
        .ok_or_else(|| {
            io::Error::other(format!(
                "csv {} is missing solana_token_mint column",
                path.display()
            ))
        })?;
    let mapped_token_symbol_index = headers.iter().position(|header| header == "mapped_token_symbol");
    let defillama_symbol_index = headers.iter().position(|header| header == "defillama_symbol");
    let asset_token_index = headers.iter().position(|header| header == "asset_token");
    let mut allowlist = TokenAllowlist::default();

    for record in reader.records() {
        let record = record?;
        let Some(solana_token_mint) = record.get(solana_token_mint_index) else {
            continue;
        };
        if solana_token_mint.is_empty() {
            continue;
        }

        let mint = solana_token_mint.parse::<Address>().map_err(|err| {
            io::Error::other(format!(
                "failed to parse solana_token_mint {} from {}: {err}",
                solana_token_mint,
                path.display()
            ))
        })?;
        allowlist.mints.insert(mint);
        if let Some(symbol) = first_non_empty_symbol([
            mapped_token_symbol_index.and_then(|index| record.get(index)),
            defillama_symbol_index.and_then(|index| record.get(index)),
            asset_token_index.and_then(|index| record.get(index)),
        ]) {
            allowlist.symbols_by_mint.entry(mint).or_insert(symbol);
        }
    }

    if allowlist.mints.is_empty() {
        return Err(io::Error::other(format!(
            "csv {} did not yield any solana token mints",
            path.display()
        ))
        .into());
    }

    Ok(allowlist)
}

fn load_jupiter_verified_token_allowlist_csv(
    min_token_volume_24h_usd: f64,
) -> Result<TokenAllowlist, BoxError> {
    let path = jupiter_verified_tokens_csv_path();
    let mut reader = ReaderBuilder::new().trim(csv::Trim::All).from_path(&path)?;
    let headers = reader.headers()?.clone();
    let id_index = headers
        .iter()
        .position(|header| header == "id")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing id column", path.display())))?;
    let symbol_index = headers
        .iter()
        .position(|header| header == "symbol")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing symbol column", path.display())))?;
    let buy_volume_index = headers
        .iter()
        .position(|header| header == "stats24h_buyVolume")
        .ok_or_else(|| {
            io::Error::other(format!(
                "csv {} is missing stats24h_buyVolume column",
                path.display()
            ))
        })?;
    let sell_volume_index = headers
        .iter()
        .position(|header| header == "stats24h_sellVolume")
        .ok_or_else(|| {
            io::Error::other(format!(
                "csv {} is missing stats24h_sellVolume column",
                path.display()
            ))
        })?;
    let mut allowlist = TokenAllowlist::default();

    for record in reader.records() {
        let record = record?;
        let Some(id) = record.get(id_index) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        if record_24h_volume(record.get(buy_volume_index), record.get(sell_volume_index))
            < min_token_volume_24h_usd
        {
            continue;
        }

        let mint = id.parse::<Address>().map_err(|err| {
            io::Error::other(format!(
                "failed to parse jupiter verified token mint {} from {}: {err}",
                id,
                path.display()
            ))
        })?;
        allowlist.mints.insert(mint);
        if let Some(symbol) = normalize_symbol(record.get(symbol_index)) {
            allowlist.symbols_by_mint.entry(mint).or_insert(symbol);
        }
    }

    if allowlist.mints.is_empty() {
        return Err(io::Error::other(format!(
            "csv {} did not yield any jupiter verified token mints",
            path.display()
        ))
        .into());
    }

    Ok(allowlist)
}

fn load_wsol_usdc_price_from_jupiter_verified_csv() -> Result<f64, BoxError> {
    let path = jupiter_verified_tokens_csv_path();
    let mut reader = ReaderBuilder::new().trim(csv::Trim::All).from_path(&path)?;
    let wsol_mint = WSOL_MINT.to_string();
    let headers = reader.headers()?.clone();
    let id_index = headers
        .iter()
        .position(|header| header == "id")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing id column", path.display())))?;
    let usd_price_index = headers
        .iter()
        .position(|header| header == "usdPrice")
        .ok_or_else(|| {
            io::Error::other(format!("csv {} is missing usdPrice column", path.display()))
        })?;

    for record in reader.records() {
        let record = record?;
        if record.get(id_index) != Some(wsol_mint.as_str()) {
            continue;
        }

        let usd_price = record
            .get(usd_price_index)
            .ok_or_else(|| io::Error::other("missing usdPrice field for WSOL row"))?;
        if usd_price.is_empty() {
            return Err(io::Error::other("empty usdPrice field for WSOL row").into());
        }

        return usd_price.parse::<f64>().map_err(|err| {
            io::Error::other(format!(
                "failed to parse WSOL usdPrice {} from {}: {err}",
                usd_price,
                path.display()
            ))
            .into()
        });
    }

    Err(io::Error::other(format!(
        "csv {} is missing WSOL row {}",
        path.display(),
        WSOL_MINT
    ))
    .into())
}

fn load_jupiter_verified_token_metadata_csv() -> Result<HashMap<Address, TokenMetadata>, BoxError> {
    let path = jupiter_verified_tokens_csv_path();
    let mut reader = ReaderBuilder::new().trim(csv::Trim::All).from_path(&path)?;
    let headers = reader.headers()?.clone();
    let id_index = headers
        .iter()
        .position(|header| header == "id")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing id column", path.display())))?;
    let symbol_index = headers
        .iter()
        .position(|header| header == "symbol")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing symbol column", path.display())))?;
    let decimals_index = headers
        .iter()
        .position(|header| header == "decimals")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing decimals column", path.display())))?;
    let usd_price_index = headers
        .iter()
        .position(|header| header == "usdPrice")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing usdPrice column", path.display())))?;
    let buy_volume_index = headers
        .iter()
        .position(|header| header == "stats24h_buyVolume")
        .ok_or_else(|| {
            io::Error::other(format!(
                "csv {} is missing stats24h_buyVolume column",
                path.display()
            ))
        })?;
    let sell_volume_index = headers
        .iter()
        .position(|header| header == "stats24h_sellVolume")
        .ok_or_else(|| {
            io::Error::other(format!(
                "csv {} is missing stats24h_sellVolume column",
                path.display()
            ))
        })?;
    let mut metadata = HashMap::new();

    for record in reader.records() {
        let record = record?;
        let Some(id) = record.get(id_index).filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(symbol) = normalize_symbol(record.get(symbol_index)) else {
            continue;
        };
        let decimals = record
            .get(decimals_index)
            .filter(|decimals| !decimals.is_empty())
            .and_then(|decimals| decimals.parse::<u8>().ok());
        let usd_price = record
            .get(usd_price_index)
            .filter(|price| !price.is_empty())
            .and_then(|price| price.parse::<f64>().ok());
        let (Some(decimals), Some(usd_price)) = (decimals, usd_price) else {
            continue;
        };
        if !(usd_price.is_finite() && usd_price > 0.0) {
            continue;
        }

        let mint = id.parse::<Address>().map_err(|err| {
            io::Error::other(format!(
                "failed to parse jupiter verified token mint {} from {}: {err}",
                id,
                path.display()
            ))
        })?;
        metadata.insert(
            mint,
            TokenMetadata {
                symbol,
                decimals,
                usd_price,
                volume_24h_usd: record_24h_volume(
                    record.get(buy_volume_index),
                    record.get(sell_volume_index),
                ),
            },
        );
    }

    if metadata.is_empty() {
        return Err(io::Error::other(format!(
            "csv {} did not yield any token metadata",
            path.display()
        ))
        .into());
    }

    Ok(metadata)
}

async fn fetch_jupiter_verified_token_allowlist(
    jupiter_api_key: Option<&str>,
    min_token_volume_24h_usd: f64,
) -> Result<TokenAllowlist, BoxError> {
    let client = Client::builder().timeout(JUPITER_HTTP_TIMEOUT).build()?;
    let (url, using_api_key) = match jupiter_api_key {
        Some(_) => (JUPITER_VERIFIED_TOKENS_API_URL, true),
        None => (JUPITER_VERIFIED_TOKENS_LITE_URL, false),
    };
    info!(
        url,
        using_api_key,
        "fetching jupiter verified token allowlist",
    );

    let mut request = client.get(url);
    if let Some(jupiter_api_key) = jupiter_api_key {
        request = request.header("x-api-key", jupiter_api_key);
    }

    let tokens = request
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<JupiterVerifiedToken>>()
        .await?;

    let mut allowlist = TokenAllowlist::default();
    for token in tokens {
        if token_24h_volume(&token) < min_token_volume_24h_usd {
            continue;
        }
        let mint = token.id.parse::<Address>().map_err(|err| {
            io::Error::other(format!(
                "failed to parse jupiter verified token mint {}: {err}",
                token.id
            ))
        })?;
        allowlist.mints.insert(mint);
        if let Some(symbol) = normalize_symbol(token.symbol.as_deref()) {
            allowlist.symbols_by_mint.insert(mint, symbol);
        }
    }

    if allowlist.mints.is_empty() {
        return Err(io::Error::other("jupiter verified token allowlist is empty").into());
    }

    info!(
        token_allowlist_count = allowlist.mints.len(),
        using_api_key,
        "fetched jupiter verified token allowlist",
    );

    Ok(allowlist)
}

fn pool_non_wsol_mint(raw: &PoolStateRaw) -> Option<Address> {
    match (raw.token_0_mint == WSOL_MINT, raw.token_1_mint == WSOL_MINT) {
        (true, false) => Some(raw.token_1_mint),
        (false, true) => Some(raw.token_0_mint),
        _ => None,
    }
}

fn write_ranked_candidates_csv(
    args: &ScanRaydiumCpSwapArgs,
    ranked_candidates: &[RankedPoolCandidate],
    token_symbols_by_mint: &HashMap<Address, String>,
    simulation_outcomes: &HashMap<Address, SimulationCsvOutcome>,
) -> Result<(), BoxError> {
    let mut writer = WriterBuilder::new().from_path(&args.output_csv)?;
    writer.write_record([
        "rank",
        "pool",
        "amm_config",
        "input_mint",
        "output_mint",
        "non_wsol_mint",
        "token_symbol",
        "input_amount",
        "input_reserve",
        "output_reserve",
        "wsol_reserve",
        "estimated_tvl_usdc",
        "trade_fee_rate",
        "protocol_fee_rate",
        "fund_fee_rate",
        "creator_fee_rate",
        "estimated_output_amount",
        "estimated_price_impact_bps",
        "input_share_bps",
        "input_spent",
        "returned_input_amount",
        "bundle_input_delta",
        "input_spent_within_tolerance",
        "returned_input_within_tolerance",
        "round_trip_verified",
        "simulation_error",
    ])?;

    for (rank, candidate) in ranked_candidates.iter().take(args.top).enumerate() {
        let simulation_outcome = simulation_outcomes.get(&candidate.pool);
        writer.write_record([
            (rank + 1).to_string(),
            candidate.pool.to_string(),
            candidate.amm_config.to_string(),
            args.input_mint.to_string(),
            candidate.output_mint.to_string(),
            candidate.non_wsol_mint.to_string(),
            token_symbols_by_mint
                .get(&candidate.non_wsol_mint)
                .cloned()
                .unwrap_or_default(),
            args.input_amount.to_string(),
            candidate.input_reserve.to_string(),
            candidate.output_reserve.to_string(),
            candidate.wsol_reserve.to_string(),
            format!("{:.6}", candidate.estimated_tvl_usdc),
            candidate.trade_fee_rate.to_string(),
            candidate.protocol_fee_rate.to_string(),
            candidate.fund_fee_rate.to_string(),
            candidate.creator_fee_rate.to_string(),
            candidate.estimated_output_amount.to_string(),
            candidate.estimated_price_impact_bps.to_string(),
            candidate.input_share_bps.to_string(),
            option_to_string(simulation_outcome.and_then(|outcome| outcome.input_spent)),
            option_to_string(
                simulation_outcome.and_then(|outcome| outcome.returned_input_amount),
            ),
            option_to_string(simulation_outcome.and_then(|outcome| outcome.bundle_input_delta)),
            option_to_string(
                simulation_outcome
                    .and_then(|outcome| outcome.input_spent_within_tolerance),
            ),
            option_to_string(
                simulation_outcome
                    .and_then(|outcome| outcome.returned_input_within_tolerance),
            ),
            option_to_string(simulation_outcome.and_then(|outcome| outcome.round_trip_verified)),
            simulation_outcome
                .and_then(|outcome| outcome.simulation_error.clone())
                .unwrap_or_default(),
        ])?;
    }
    writer.flush()?;

    info!(
        output_csv = %args.output_csv.display(),
        written_rows = ranked_candidates.len().min(args.top),
        simulated_rows = simulation_outcomes.len(),
        "wrote raydium cp-swap shortlist csv",
    );

    Ok(())
}

fn write_dlmm_candidates_csv(
    args: &ScanMeteoraDlmmArgs,
    ranked_candidates: &[RankedDlmmPairCandidate],
) -> Result<(), BoxError> {
    let mut writer = WriterBuilder::new().from_path(&args.output_csv)?;
    writer.write_record([
        "rank",
        "pair",
        "token_x_mint",
        "token_y_mint",
        "non_wsol_mint",
        "non_wsol_symbol",
        "active_id",
        "target_bin_id",
        "bin_step",
        "status",
        "target_bin_array",
        "target_bin_array_index",
        "target_bin_index",
        "wsol_side",
        "target_bin_amount_x",
        "target_bin_amount_y",
        "target_bin_liquidity_supply",
        "target_bin_open_order_amount",
        "target_bin_is_empty",
        "reserve_x_ui_amount",
        "reserve_y_ui_amount",
        "wsol_reserve_ui_amount",
        "estimated_tvl_usdc",
        "active_price_y_per_x",
        "fair_price_y_per_x",
        "implied_wsol_usdc_price",
        "reference_wsol_usdc_price",
        "wsol_discount_bps",
        "wsol_is_cheap_for_arbers",
        "token_volume_24h_usd",
    ])?;

    for (rank, candidate) in ranked_candidates.iter().take(args.top).enumerate() {
        writer.write_record([
            (rank + 1).to_string(),
            candidate.pair.to_string(),
            candidate.token_x_mint.to_string(),
            candidate.token_y_mint.to_string(),
            candidate.non_wsol_mint.to_string(),
            candidate.non_wsol_symbol.clone(),
            candidate.active_id.to_string(),
            candidate.target_bin_id.to_string(),
            candidate.bin_step.to_string(),
            candidate.status.to_string(),
            candidate.target_bin_array.to_string(),
            candidate.target_bin_array_index.to_string(),
            candidate.target_bin_index.to_string(),
            candidate.wsol_side.to_string(),
            candidate.target_bin_amount_x.to_string(),
            candidate.target_bin_amount_y.to_string(),
            candidate.target_bin_liquidity_supply.to_string(),
            candidate.target_bin_open_order_amount.to_string(),
            candidate.target_bin_is_empty.to_string(),
            format!("{:.12}", candidate.reserve_x_ui_amount),
            format!("{:.12}", candidate.reserve_y_ui_amount),
            format!("{:.12}", candidate.wsol_reserve_ui_amount),
            format!("{:.6}", candidate.estimated_tvl_usdc),
            format!("{:.12}", candidate.active_price_y_per_x),
            format!("{:.12}", candidate.fair_price_y_per_x),
            format!("{:.6}", candidate.implied_wsol_usdc_price),
            format!("{:.6}", candidate.reference_wsol_usdc_price),
            candidate.wsol_discount_bps.to_string(),
            candidate.wsol_is_cheap_for_arbers.to_string(),
            format!("{:.6}", candidate.token_volume_24h_usd),
        ])?;
    }
    writer.flush()?;

    info!(
        output_csv = %args.output_csv.display(),
        written_candidates = ranked_candidates.len().min(args.top),
        "wrote meteora dlmm candidate csv",
    );
    Ok(())
}

fn option_to_string<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn token_24h_volume(token: &JupiterVerifiedToken) -> f64 {
    token
        .stats_24h
        .as_ref()
        .map(|stats| stats.buy_volume.unwrap_or_default() + stats.sell_volume.unwrap_or_default())
        .unwrap_or_default()
}

fn normalize_symbol(symbol: Option<&str>) -> Option<String> {
    let symbol = symbol?.trim();
    if symbol.is_empty() || symbol == "-" {
        return None;
    }
    Some(symbol.to_string())
}

fn first_non_empty_symbol<'a>(symbols: [Option<&'a str>; 3]) -> Option<String> {
    symbols.into_iter().find_map(normalize_symbol)
}

fn record_24h_volume(buy_volume: Option<&str>, sell_volume: Option<&str>) -> f64 {
    parse_optional_f64(buy_volume) + parse_optional_f64(sell_volume)
}

fn parse_optional_f64(value: Option<&str>) -> f64 {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or_default()
}

use {
    crate::{
        cli::ProvisionRaydiumCpSwapPoolArgs,
        error::BoxError,
        raydium_cp_swap_constants::{
            AMM_CONFIG_DISCRIMINATOR, DEPOSIT_DISCRIMINATOR, INITIALIZE_DISCRIMINATOR,
            POOL_STATE_DISCRIMINATOR, RAYDIUM_AUTH_SEED, RAYDIUM_CP_SWAP_PROGRAM_ID,
            RAYDIUM_CREATE_POOL_FEE_RECEIVER, RAYDIUM_OBSERVATION_SEED,
            RAYDIUM_POOL_LP_MINT_SEED, RAYDIUM_POOL_SEED, RAYDIUM_POOL_VAULT_SEED,
        },
    },
    bytemuck::{Pod, Zeroable},
    csv::ReaderBuilder,
    solana_account::Account,
    solana_address::{address, Address},
    solana_commitment_config::CommitmentConfig,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{
        config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        filter::{Memcmp, RpcFilterType},
        response::UiAccountEncoding,
    },
    solana_signer::Signer,
    solana_transaction::Transaction,
    spl_associated_token_account_interface::{
        address::get_associated_token_address_with_program_id,
        instruction::create_associated_token_account_idempotent,
    },
    std::{collections::HashMap, io, mem::size_of, path::PathBuf},
    tracing::info,
};

const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");
const RENT_SYSVAR_ID: Address = address!("SysvarRent111111111111111111111111111111111");
const JUPITER_VERIFIED_TOKENS_CSV: &str = "jupiter_verified_tokens.csv";
const TOKEN_ACCOUNT_AMOUNT_OFFSET: usize = 64;
const USD_EPSILON: f64 = 1e-9;

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
    lp_mint_decimals: u8,
    mint_0_decimals: u8,
    mint_1_decimals: u8,
    lp_supply: u64,
    protocol_fees_token_0: u64,
    protocol_fees_token_1: u64,
    fund_fees_token_0: u64,
    fund_fees_token_1: u64,
    open_time: u64,
    recent_epoch: u64,
    creator_fee_on: u8,
    enable_creator_fee: u8,
    padding1: [u8; 6],
    creator_fees_token_0: u64,
    creator_fees_token_1: u64,
    padding: [u64; 28],
}

#[derive(Clone, Debug)]
struct TokenPriceInfo {
    usd_price: f64,
    symbol: String,
}

#[derive(Clone, Debug)]
struct MintMetadata {
    mint: Address,
    token_program: Address,
    decimals: u8,
    usd_price: f64,
    symbol: String,
}

#[derive(Clone, Copy, Debug)]
struct AmmConfigInfo {
    address: Address,
    index: u16,
    create_pool_fee: u64,
}

#[derive(Clone, Copy, Debug)]
struct PoolAddresses {
    authority: Address,
    pool: Address,
    lp_mint: Address,
    token_0_vault: Address,
    token_1_vault: Address,
    observation_state: Address,
}

#[derive(Clone, Copy, Debug)]
struct PoolSnapshot {
    addresses: PoolAddresses,
    lp_supply: u64,
    token_0_usable_reserve: u64,
    token_1_usable_reserve: u64,
    estimated_tvl_usdc: f64,
}

#[derive(Clone, Debug)]
struct SelectedPool {
    config: AmmConfigInfo,
    addresses: PoolAddresses,
    existing_snapshot: Option<PoolSnapshot>,
}

pub async fn provision_raydium_cp_swap_pool(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ProvisionRaydiumCpSwapPoolArgs,
) -> Result<(), BoxError> {
    let price_map = load_jupiter_price_map()?;
    let (token_0_mint, token_1_mint, token_0_override, token_1_override) =
        if args.token_a_mint < args.token_b_mint {
            (
                args.token_a_mint,
                args.token_b_mint,
                args.token_a_usd_price,
                args.token_b_usd_price,
            )
        } else {
            (
                args.token_b_mint,
                args.token_a_mint,
                args.token_b_usd_price,
                args.token_a_usd_price,
            )
        };

    let token_0 = load_mint_metadata(rpc_client, token_0_mint, token_0_override, &price_map).await?;
    let token_1 = load_mint_metadata(rpc_client, token_1_mint, token_1_override, &price_map).await?;
    let amm_configs = fetch_amm_configs(rpc_client).await?;
    let selected_pool = select_pool_for_pair(
        rpc_client,
        &amm_configs,
        &token_0,
        &token_1,
        args.max_existing_tvl_usdc,
        args.max_price_deviation_bps,
    )
    .await?;

    info!(
        token_0_mint = %token_0.mint,
        token_0_symbol = token_0.symbol,
        token_0_price = token_0.usd_price,
        token_1_mint = %token_1.mint,
        token_1_symbol = token_1.symbol,
        token_1_price = token_1.usd_price,
        selected_config = %selected_pool.config.address,
        selected_config_index = selected_pool.config.index,
        selected_pool = %selected_pool.addresses.pool,
        create_pool_fee_lamports = selected_pool.config.create_pool_fee,
        existing_pool = selected_pool.existing_snapshot.is_some(),
        "selected raydium 0.25% pool provisioning target",
    );

    let mut remaining_tvl_usdc = args.total_tvl_usdc;
    let mut chunk_index = 0usize;
    let mut current_snapshot = selected_pool.existing_snapshot;

    while remaining_tvl_usdc > USD_EPSILON {
        let chunk_tvl_usdc = remaining_tvl_usdc.min(args.chunk_tvl_usdc);
        let desired_token_0_amount =
            usd_to_raw_amount(chunk_tvl_usdc / 2.0, token_0.usd_price, token_0.decimals)?;
        let desired_token_1_amount =
            usd_to_raw_amount(chunk_tvl_usdc / 2.0, token_1.usd_price, token_1.decimals)?;
        let blockhash = rpc_client.get_latest_blockhash().await?;

        if desired_token_0_amount == 0 || desired_token_1_amount == 0 {
            return Err(io::Error::other(format!(
                "chunk {} computed zero raw amount for token pair at chunk_tvl_usdc={chunk_tvl_usdc}",
                chunk_index + 1
            ))
            .into());
        }

        let signature = if let Some(snapshot) = current_snapshot {
            validate_pool_price(snapshot, &token_0, &token_1, args.max_price_deviation_bps)?;
            let lp_token_amount =
                quote_lp_token_amount(snapshot, desired_token_0_amount, desired_token_1_amount)?;
            let maximum_token_0_amount = inflate_amount_by_five_percent(desired_token_0_amount)?;
            let maximum_token_1_amount = inflate_amount_by_five_percent(desired_token_1_amount)?;
            let spl_token_program_id = spl_token_program_id();
            let owner_lp_token =
                get_associated_token_address_with_program_id(
                    &signer.pubkey(),
                    &snapshot.addresses.lp_mint,
                    &spl_token_program_id,
                );
            let token_0_account = get_associated_token_address_with_program_id(
                &signer.pubkey(),
                &token_0.mint,
                &token_0.token_program,
            );
            let token_1_account = get_associated_token_address_with_program_id(
                &signer.pubkey(),
                &token_1.mint,
                &token_1.token_program,
            );
            let setup_instructions = vec![
                create_associated_token_account_idempotent(
                    &signer.pubkey(),
                    &signer.pubkey(),
                    &token_0.mint,
                    &token_0.token_program,
                ),
                create_associated_token_account_idempotent(
                    &signer.pubkey(),
                    &signer.pubkey(),
                    &token_1.mint,
                    &token_1.token_program,
                ),
                create_associated_token_account_idempotent(
                    &signer.pubkey(),
                    &signer.pubkey(),
                    &snapshot.addresses.lp_mint,
                    &spl_token_program_id,
                ),
            ];
            let mut instructions = setup_instructions;
            instructions.push(build_deposit_instruction(
                signer.pubkey(),
                snapshot,
                owner_lp_token,
                token_0_account,
                token_1_account,
                token_0.mint,
                token_1.mint,
                lp_token_amount,
                maximum_token_0_amount,
                maximum_token_1_amount,
            ));
            let transaction = build_signed_transaction(signer, blockhash, &instructions);
            let signature = rpc_client.send_and_confirm_transaction(&transaction).await?;
            info!(
                chunk_index = chunk_index + 1,
                chunk_tvl_usdc,
                lp_token_amount,
                maximum_token_0_amount,
                maximum_token_1_amount,
                signature = %signature,
                "deposited chunk into raydium cp-swap pool",
            );
            signature
        } else {
            let creator_token_0 = get_associated_token_address_with_program_id(
                &signer.pubkey(),
                &token_0.mint,
                &token_0.token_program,
            );
            let creator_token_1 = get_associated_token_address_with_program_id(
                &signer.pubkey(),
                &token_1.mint,
                &token_1.token_program,
            );
            let creator_lp_token = get_associated_token_address_with_program_id(
                &signer.pubkey(),
                &selected_pool.addresses.lp_mint,
                &spl_token_program_id(),
            );
            let mut instructions = vec![
                create_associated_token_account_idempotent(
                    &signer.pubkey(),
                    &signer.pubkey(),
                    &token_0.mint,
                    &token_0.token_program,
                ),
                create_associated_token_account_idempotent(
                    &signer.pubkey(),
                    &signer.pubkey(),
                    &token_1.mint,
                    &token_1.token_program,
                ),
            ];
            instructions.push(build_initialize_instruction(
                signer.pubkey(),
                selected_pool.config.address,
                selected_pool.addresses,
                creator_token_0,
                creator_token_1,
                creator_lp_token,
                &token_0,
                &token_1,
                desired_token_0_amount,
                desired_token_1_amount,
            ));
            let transaction = build_signed_transaction(signer, blockhash, &instructions);
            let signature = rpc_client.send_and_confirm_transaction(&transaction).await?;
            info!(
                chunk_index = chunk_index + 1,
                chunk_tvl_usdc,
                init_token_0_amount = desired_token_0_amount,
                init_token_1_amount = desired_token_1_amount,
                signature = %signature,
                "created and seeded new raydium cp-swap pool",
            );
            signature
        };

        let refreshed_snapshot =
            load_pool_snapshot(rpc_client, selected_pool.addresses, &token_0, &token_1).await?;
        validate_pool_price(refreshed_snapshot, &token_0, &token_1, args.max_price_deviation_bps)?;
        info!(
            chunk_index = chunk_index + 1,
            chunk_signature = %signature,
            pool = %selected_pool.addresses.pool,
            pool_tvl_usdc = refreshed_snapshot.estimated_tvl_usdc,
            token_0_reserve = refreshed_snapshot.token_0_usable_reserve,
            token_1_reserve = refreshed_snapshot.token_1_usable_reserve,
            lp_supply = refreshed_snapshot.lp_supply,
            "refreshed pool state after chunk",
        );

        current_snapshot = Some(refreshed_snapshot);
        remaining_tvl_usdc -= chunk_tvl_usdc;
        chunk_index += 1;
    }

    Ok(())
}

fn load_jupiter_price_map() -> Result<HashMap<Address, TokenPriceInfo>, BoxError> {
    let path = jupiter_verified_tokens_csv_path();
    let mut reader = ReaderBuilder::new().trim(csv::Trim::All).from_path(&path)?;
    let headers = reader.headers()?.clone();
    let id_index = headers
        .iter()
        .position(|header| header == "id")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing id column", path.display())))?;
    let symbol_index = headers.iter().position(|header| header == "symbol").ok_or_else(|| {
        io::Error::other(format!("csv {} is missing symbol column", path.display()))
    })?;
    let usd_price_index = headers
        .iter()
        .position(|header| header == "usdPrice")
        .ok_or_else(|| io::Error::other(format!("csv {} is missing usdPrice column", path.display())))?;
    let mut prices = HashMap::new();

    for record in reader.records() {
        let record = record?;
        let Some(id) = record.get(id_index) else {
            continue;
        };
        let Some(usd_price) = record.get(usd_price_index) else {
            continue;
        };
        let Some(symbol) = record.get(symbol_index) else {
            continue;
        };
        if id.is_empty() || usd_price.is_empty() {
            continue;
        }
        let Ok(mint) = id.parse::<Address>() else {
            continue;
        };
        let Ok(usd_price) = usd_price.parse::<f64>() else {
            continue;
        };
        if !(usd_price.is_finite() && usd_price > 0.0) {
            continue;
        }
        prices.insert(
            mint,
            TokenPriceInfo {
                usd_price,
                symbol: if symbol.is_empty() { mint.to_string() } else { symbol.to_string() },
            },
        );
    }

    Ok(prices)
}

async fn load_mint_metadata(
    rpc_client: &RpcClient,
    mint: Address,
    usd_price_override: Option<f64>,
    price_map: &HashMap<Address, TokenPriceInfo>,
) -> Result<MintMetadata, BoxError> {
    let mint_account = rpc_client.get_account(&mint).await?;
    let supply = rpc_client.get_token_supply(&mint).await?;
    let price_info = if let Some(usd_price) = usd_price_override {
        TokenPriceInfo {
            usd_price,
            symbol: price_map
                .get(&mint)
                .map(|info| info.symbol.clone())
                .unwrap_or_else(|| mint.to_string()),
        }
    } else {
        price_map.get(&mint).cloned().ok_or_else(|| {
            io::Error::other(format!(
                "mint {} is missing from jupiter_verified_tokens.csv; provide an explicit usd price override",
                mint
            ))
        })?
    };

    Ok(MintMetadata {
        mint,
        token_program: mint_account.owner,
        decimals: supply.decimals,
        usd_price: price_info.usd_price,
        symbol: price_info.symbol,
    })
}

async fn fetch_amm_configs(rpc_client: &RpcClient) -> Result<Vec<AmmConfigInfo>, BoxError> {
    let config_accounts = rpc_client
        .get_program_ui_accounts_with_config(
            &RAYDIUM_CP_SWAP_PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                    0,
                    AMM_CONFIG_DISCRIMINATOR.to_vec(),
                ))]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    commitment: Some(CommitmentConfig::processed()),
                    data_slice: None,
                    min_context_slot: None,
                },
                with_context: None,
                sort_results: None,
            },
        )
        .await?;

    let mut configs = config_accounts
        .into_iter()
        .filter_map(|(address, account)| {
            account
                .decode()
                .and_then(|account| decode_amm_config(address, &account).transpose())
        })
        .collect::<Result<Vec<_>, _>>()?;
    configs.sort_by_key(|config| config.index);
    Ok(configs)
}

fn decode_amm_config(address: Address, account: &Account) -> Result<Option<AmmConfigInfo>, BoxError> {
    let raw = bytemuck::try_pod_read_unaligned::<AmmConfigRaw>(
        account
            .data
            .get(..size_of::<AmmConfigRaw>())
            .ok_or_else(|| io::Error::other(format!("amm config {} data too short", address)))?,
    )
    .map_err(|err| io::Error::other(format!("failed to decode amm config {}: {err}", address)))?;

    if raw.discriminator != AMM_CONFIG_DISCRIMINATOR || raw.trade_fee_rate != 2_500 {
        return Ok(None);
    }
    if raw.disable_create_pool != 0 {
        return Ok(None);
    }

    Ok(Some(AmmConfigInfo {
        address,
        index: raw.index,
        create_pool_fee: raw.create_pool_fee,
    }))
}

async fn select_pool_for_pair(
    rpc_client: &RpcClient,
    amm_configs: &[AmmConfigInfo],
    token_0: &MintMetadata,
    token_1: &MintMetadata,
    max_existing_tvl_usdc: f64,
    max_price_deviation_bps: u64,
) -> Result<SelectedPool, BoxError> {
    let mut first_missing = None;

    for config in amm_configs {
        let addresses = derive_pool_addresses(config.address, token_0.mint, token_1.mint);
        let account = rpc_client
            .get_account_with_commitment(&addresses.pool, CommitmentConfig::processed())
            .await?
            .value;

        if account.is_none() {
            if first_missing.is_none() {
                first_missing = Some(SelectedPool {
                    config: *config,
                    addresses,
                    existing_snapshot: None,
                });
            }
            continue;
        }

        let snapshot = load_pool_snapshot(rpc_client, addresses, token_0, token_1).await?;
        if snapshot.estimated_tvl_usdc > max_existing_tvl_usdc {
            continue;
        }
        validate_pool_price(snapshot, token_0, token_1, max_price_deviation_bps)?;
        return Ok(SelectedPool {
            config: *config,
            addresses,
            existing_snapshot: Some(snapshot),
        });
    }

    first_missing.ok_or_else(|| {
        io::Error::other(format!(
            "no suitable 0.25% raydium cp-swap config/pool found for pair {} / {}",
            token_0.mint, token_1.mint
        ))
        .into()
    })
}

fn derive_pool_addresses(
    amm_config: Address,
    token_0_mint: Address,
    token_1_mint: Address,
) -> PoolAddresses {
    let (authority, _) = Address::find_program_address(&[RAYDIUM_AUTH_SEED], &RAYDIUM_CP_SWAP_PROGRAM_ID);
    let (pool, _) = Address::find_program_address(
        &[RAYDIUM_POOL_SEED, amm_config.as_ref(), token_0_mint.as_ref(), token_1_mint.as_ref()],
        &RAYDIUM_CP_SWAP_PROGRAM_ID,
    );
    let (lp_mint, _) = Address::find_program_address(
        &[RAYDIUM_POOL_LP_MINT_SEED, pool.as_ref()],
        &RAYDIUM_CP_SWAP_PROGRAM_ID,
    );
    let (token_0_vault, _) = Address::find_program_address(
        &[RAYDIUM_POOL_VAULT_SEED, pool.as_ref(), token_0_mint.as_ref()],
        &RAYDIUM_CP_SWAP_PROGRAM_ID,
    );
    let (token_1_vault, _) = Address::find_program_address(
        &[RAYDIUM_POOL_VAULT_SEED, pool.as_ref(), token_1_mint.as_ref()],
        &RAYDIUM_CP_SWAP_PROGRAM_ID,
    );
    let (observation_state, _) = Address::find_program_address(
        &[RAYDIUM_OBSERVATION_SEED, pool.as_ref()],
        &RAYDIUM_CP_SWAP_PROGRAM_ID,
    );

    PoolAddresses {
        authority,
        pool,
        lp_mint,
        token_0_vault,
        token_1_vault,
        observation_state,
    }
}

async fn load_pool_snapshot(
    rpc_client: &RpcClient,
    addresses: PoolAddresses,
    token_0: &MintMetadata,
    token_1: &MintMetadata,
) -> Result<PoolSnapshot, BoxError> {
    let accounts = rpc_client
        .get_multiple_accounts(&[addresses.pool, addresses.token_0_vault, addresses.token_1_vault])
        .await?;
    let pool_account = accounts[0]
        .as_ref()
        .ok_or_else(|| io::Error::other(format!("missing pool account {}", addresses.pool)))?;
    let token_0_vault_account = accounts[1].as_ref().ok_or_else(|| {
        io::Error::other(format!("missing token_0 vault account {}", addresses.token_0_vault))
    })?;
    let token_1_vault_account = accounts[2].as_ref().ok_or_else(|| {
        io::Error::other(format!("missing token_1 vault account {}", addresses.token_1_vault))
    })?;

    let pool_state = bytemuck::try_pod_read_unaligned::<PoolStateRaw>(
        pool_account
            .data
            .get(..size_of::<PoolStateRaw>())
            .ok_or_else(|| io::Error::other(format!("pool {} data too short", addresses.pool)))?,
    )
    .map_err(|err| io::Error::other(format!("failed to decode pool {}: {err}", addresses.pool)))?;

    if pool_state.discriminator != POOL_STATE_DISCRIMINATOR {
        return Err(io::Error::other(format!(
            "pool {} has unexpected discriminator",
            addresses.pool
        ))
        .into());
    }

    let token_0_vault_amount = parse_token_account_amount(token_0_vault_account)?;
    let token_1_vault_amount = parse_token_account_amount(token_1_vault_account)?;
    let token_0_usable_reserve = token_0_vault_amount
        .saturating_sub(pool_state.protocol_fees_token_0)
        .saturating_sub(pool_state.fund_fees_token_0)
        .saturating_sub(pool_state.creator_fees_token_0);
    let token_1_usable_reserve = token_1_vault_amount
        .saturating_sub(pool_state.protocol_fees_token_1)
        .saturating_sub(pool_state.fund_fees_token_1)
        .saturating_sub(pool_state.creator_fees_token_1);

    let estimated_tvl_usdc =
        raw_amount_to_ui(token_0_usable_reserve, token_0.decimals) * token_0.usd_price
            + raw_amount_to_ui(token_1_usable_reserve, token_1.decimals) * token_1.usd_price;

    Ok(PoolSnapshot {
        addresses,
        lp_supply: pool_state.lp_supply,
        token_0_usable_reserve,
        token_1_usable_reserve,
        estimated_tvl_usdc,
    })
}

fn validate_pool_price(
    snapshot: PoolSnapshot,
    token_0: &MintMetadata,
    token_1: &MintMetadata,
    max_price_deviation_bps: u64,
) -> Result<(), BoxError> {
    if snapshot.token_0_usable_reserve == 0 || snapshot.token_1_usable_reserve == 0 {
        return Ok(());
    }

    let pool_price = raw_amount_to_ui(snapshot.token_1_usable_reserve, token_1.decimals)
        / raw_amount_to_ui(snapshot.token_0_usable_reserve, token_0.decimals);
    let fair_price = token_0.usd_price / token_1.usd_price;
    let deviation_bps = ratio_deviation_bps(pool_price, fair_price)?;

    if deviation_bps > max_price_deviation_bps {
        return Err(io::Error::other(format!(
            "pool {} price deviates too much from reference price: deviation_bps={} max_allowed_bps={}",
            snapshot.addresses.pool, deviation_bps, max_price_deviation_bps
        ))
        .into());
    }

    Ok(())
}

fn ratio_deviation_bps(actual: f64, expected: f64) -> Result<u64, BoxError> {
    if !(actual.is_finite() && actual > 0.0 && expected.is_finite() && expected > 0.0) {
        return Err(io::Error::other("cannot compute price deviation with non-positive ratio").into());
    }
    Ok((((actual / expected) - 1.0).abs() * 10_000.0).round() as u64)
}

fn quote_lp_token_amount(
    snapshot: PoolSnapshot,
    desired_token_0_amount: u64,
    desired_token_1_amount: u64,
) -> Result<u64, BoxError> {
    if snapshot.lp_supply == 0
        || snapshot.token_0_usable_reserve == 0
        || snapshot.token_1_usable_reserve == 0
    {
        return Err(io::Error::other("cannot quote LP amount for empty pool").into());
    }

    let lp_from_token_0 = (u128::from(desired_token_0_amount) * u128::from(snapshot.lp_supply))
        / u128::from(snapshot.token_0_usable_reserve);
    let lp_from_token_1 = (u128::from(desired_token_1_amount) * u128::from(snapshot.lp_supply))
        / u128::from(snapshot.token_1_usable_reserve);
    let lp_amount = lp_from_token_0.min(lp_from_token_1);
    if lp_amount == 0 {
        return Err(io::Error::other("desired deposit chunk is too small to mint LP").into());
    }
    Ok(lp_amount as u64)
}

fn build_initialize_instruction(
    payer: Address,
    amm_config: Address,
    addresses: PoolAddresses,
    creator_token_0: Address,
    creator_token_1: Address,
    creator_lp_token: Address,
    token_0: &MintMetadata,
    token_1: &MintMetadata,
    init_token_0_amount: u64,
    init_token_1_amount: u64,
) -> Instruction {
    let spl_token_program_id = spl_token_program_id();
    let mut data = Vec::with_capacity(32);
    data.extend_from_slice(&INITIALIZE_DISCRIMINATOR);
    data.extend_from_slice(&init_token_0_amount.to_le_bytes());
    data.extend_from_slice(&init_token_1_amount.to_le_bytes());
    data.extend_from_slice(&0_u64.to_le_bytes());

    Instruction {
        program_id: RAYDIUM_CP_SWAP_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(amm_config, false),
            AccountMeta::new_readonly(addresses.authority, false),
            AccountMeta::new(addresses.pool, false),
            AccountMeta::new_readonly(token_0.mint, false),
            AccountMeta::new_readonly(token_1.mint, false),
            AccountMeta::new(addresses.lp_mint, false),
            AccountMeta::new(creator_token_0, false),
            AccountMeta::new(creator_token_1, false),
            AccountMeta::new(creator_lp_token, false),
            AccountMeta::new(addresses.token_0_vault, false),
            AccountMeta::new(addresses.token_1_vault, false),
            AccountMeta::new(RAYDIUM_CREATE_POOL_FEE_RECEIVER, false),
            AccountMeta::new(addresses.observation_state, false),
            AccountMeta::new_readonly(spl_token_program_id, false),
            AccountMeta::new_readonly(token_0.token_program, false),
            AccountMeta::new_readonly(token_1.token_program, false),
            AccountMeta::new_readonly(associated_token_program_id(), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
        ],
        data,
    }
}

fn build_deposit_instruction(
    owner: Address,
    snapshot: PoolSnapshot,
    owner_lp_token: Address,
    token_0_account: Address,
    token_1_account: Address,
    token_0_mint: Address,
    token_1_mint: Address,
    lp_token_amount: u64,
    maximum_token_0_amount: u64,
    maximum_token_1_amount: u64,
) -> Instruction {
    let spl_token_program_id = spl_token_program_id();
    let spl_token_2022_program_id = spl_token_2022_program_id();
    let mut data = Vec::with_capacity(32);
    data.extend_from_slice(&DEPOSIT_DISCRIMINATOR);
    data.extend_from_slice(&lp_token_amount.to_le_bytes());
    data.extend_from_slice(&maximum_token_0_amount.to_le_bytes());
    data.extend_from_slice(&maximum_token_1_amount.to_le_bytes());

    Instruction {
        program_id: RAYDIUM_CP_SWAP_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new_readonly(snapshot.addresses.authority, false),
            AccountMeta::new(snapshot.addresses.pool, false),
            AccountMeta::new(owner_lp_token, false),
            AccountMeta::new(token_0_account, false),
            AccountMeta::new(token_1_account, false),
            AccountMeta::new(snapshot.addresses.token_0_vault, false),
            AccountMeta::new(snapshot.addresses.token_1_vault, false),
            AccountMeta::new_readonly(spl_token_program_id, false),
            AccountMeta::new_readonly(spl_token_2022_program_id, false),
            AccountMeta::new_readonly(token_0_mint, false),
            AccountMeta::new_readonly(token_1_mint, false),
            AccountMeta::new(snapshot.addresses.lp_mint, false),
        ],
        data,
    }
}

fn spl_token_program_id() -> Address {
    Address::from(spl_token_interface::id().to_bytes())
}

fn spl_token_2022_program_id() -> Address {
    Address::from(spl_token_2022_interface::id().to_bytes())
}

fn associated_token_program_id() -> Address {
    Address::from(spl_associated_token_account_interface::program::id().to_bytes())
}

fn build_signed_transaction(
    signer: &Keypair,
    blockhash: solana_hash::Hash,
    instructions: &[Instruction],
) -> Transaction {
    Transaction::new_signed_with_payer(instructions, Some(&signer.pubkey()), &[signer], blockhash)
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

fn usd_to_raw_amount(usd_amount: f64, usd_price: f64, decimals: u8) -> Result<u64, BoxError> {
    if !(usd_amount.is_finite() && usd_amount > 0.0 && usd_price.is_finite() && usd_price > 0.0) {
        return Err(io::Error::other("usd amount and usd price must be positive finite values").into());
    }
    let decimals_factor = 10_f64.powi(i32::from(decimals));
    let raw_amount = (usd_amount / usd_price) * decimals_factor;
    if !raw_amount.is_finite() || raw_amount <= 0.0 {
        return Err(io::Error::other("failed to compute positive raw token amount").into());
    }
    let raw_amount = raw_amount.round();
    if raw_amount > u64::MAX as f64 {
        return Err(io::Error::other("computed raw token amount exceeds u64").into());
    }
    Ok(raw_amount as u64)
}

fn raw_amount_to_ui(raw_amount: u64, decimals: u8) -> f64 {
    raw_amount as f64 / 10_f64.powi(i32::from(decimals))
}

fn inflate_amount_by_five_percent(amount: u64) -> Result<u64, BoxError> {
    amount
        .checked_mul(105)
        .and_then(|value| value.checked_add(99))
        .map(|value| value / 100)
        .filter(|value| *value > 0)
        .ok_or_else(|| io::Error::other("failed to compute 5% slippage-adjusted amount").into())
}

fn jupiter_verified_tokens_csv_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(JUPITER_VERIFIED_TOKENS_CSV)
}

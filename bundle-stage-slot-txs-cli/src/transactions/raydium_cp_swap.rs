use {
    crate::{
        error::BoxError,
        slot_assert::build_assert_slot_instruction,
        transactions::ResolvedRaydiumCpSwapArgs,
    },
    bytemuck::{Pod, Zeroable},
    solana_address::{address, Address},
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{
        bundles::{
            RpcBundleSimulationSummary, RpcSimulateBundleConfig, RpcSimulateBundleResult,
            SimulationSlotConfig,
        },
        config::RpcSimulateTransactionAccountsConfig,
        response::{UiAccount, UiAccountData, UiAccountEncoding},
    },
    solana_signer::Signer,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
    spl_associated_token_account_interface::{
        address::get_associated_token_address_with_program_id,
        instruction::create_associated_token_account_idempotent,
    },
    std::{io, mem::size_of},
    tracing::info,
};

const RAYDIUM_CP_SWAP_PROGRAM_ID: Address = address!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const RAYDIUM_AUTH_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";
const POOL_STATE_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];
const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
const SWAP_BASE_OUTPUT_DISCRIMINATOR: [u8; 8] =
    [0x37, 0xd9, 0x62, 0x56, 0xa3, 0x4a, 0xb4, 0xad];
const POOL_STATE_RAW_LEN: usize = size_of::<PoolStateRaw>();

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

pub(crate) struct PreparedRaydiumCpSwap {
    pool_info: RaydiumCpSwapPoolInfo,
    route: RoundTripRoute,
    first_leg_simulation: FirstLegSimulation,
    first_leg_max_input_amount: u64,
}

pub(super) async fn prepare_transactions(
    rpc_client: &RpcClient,
    signer: &Keypair,
    blockhash: Hash,
    args: &ResolvedRaydiumCpSwapArgs,
    target_slot: Slot,
) -> Result<PreparedRaydiumCpSwap, BoxError> {
    let pool_info = RaydiumCpSwapPoolInfo::load(rpc_client, args.pool).await?;
    let route = pool_info.round_trip_route(signer.pubkey(), args.input_mint)?;
    let first_leg_uniquifier = slot_uniquifier_memo(target_slot, 1);
    let simulated_first_leg_transaction = build_swap_base_input_transaction(
        signer,
        blockhash,
        &pool_info,
        &route.first_leg,
        args.input_amount,
        0,
        &first_leg_uniquifier,
        target_slot,
        false,
    )?;
    let first_leg_simulation =
        simulate_first_leg(rpc_client, &simulated_first_leg_transaction, &route.first_leg).await?;
    let first_leg_max_input_amount = inflate_amount_by_five_percent(args.input_amount)?;

    info!(
        pool = %pool_info.pool,
        amm_config = %pool_info.amm_config,
        first_input_mint = %route.first_leg.input_mint,
        first_output_mint = %route.first_leg.output_mint,
        input_amount = args.input_amount,
        simulated_output_amount = first_leg_simulation.user_output_amount,
        pool_input_amount = first_leg_simulation.pool_input_amount,
        pool_output_amount = first_leg_simulation.pool_output_amount,
        price_impact_bps = first_leg_simulation.price_impact_bps,
        first_leg_max_input_amount,
        "simulated first raydium base-input trade impact",
    );

    Ok(PreparedRaydiumCpSwap {
        pool_info,
        route,
        first_leg_simulation,
        first_leg_max_input_amount,
    })
}

pub(super) fn build_transactions(
    prepared: &PreparedRaydiumCpSwap,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<Vec<VersionedTransaction>, BoxError> {
    let first_leg_uniquifier = slot_uniquifier_memo(target_slot, 1);
    let second_leg_uniquifier = slot_uniquifier_memo(target_slot, 2);

    Ok(vec![
        build_swap_base_output_transaction(
            signer,
            blockhash,
            &prepared.pool_info,
            &prepared.route.first_leg,
            prepared.first_leg_max_input_amount,
            prepared.first_leg_simulation.user_output_amount,
            &first_leg_uniquifier,
            target_slot,
            include_slot_assert,
        )?,
        build_swap_base_input_transaction(
            signer,
            blockhash,
            &prepared.pool_info,
            &prepared.route.second_leg,
            prepared.first_leg_simulation.user_output_amount,
            0,
            &second_leg_uniquifier,
            target_slot,
            include_slot_assert,
        )?,
    ])
}

pub async fn run_startup_setup(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ResolvedRaydiumCpSwapArgs,
) -> Result<(), BoxError> {
    let pool_info = RaydiumCpSwapPoolInfo::load(rpc_client, args.pool).await?;
    let route = pool_info.round_trip_route(signer.pubkey(), args.input_mint)?;
    let initialize_user_accounts_instructions =
        build_initialize_user_token_account_instructions(rpc_client, signer.pubkey(), &route)
            .await?;

    if initialize_user_accounts_instructions.is_empty() {
        info!(
            pool = %pool_info.pool,
            "raydium startup setup found no missing user token accounts",
        );
        return Ok(());
    }

    let blockhash = rpc_client.get_latest_blockhash().await?;
    let setup_transaction = build_transaction(signer, blockhash, &initialize_user_accounts_instructions);
    let signature = rpc_client.send_and_confirm_transaction(&setup_transaction).await?;
    info!(
        pool = %pool_info.pool,
        signature = %signature,
        initialize_user_accounts = initialize_user_accounts_instructions.len(),
        "created missing raydium user token accounts during startup setup",
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct RaydiumCpSwapPoolInfo {
    pool: Address,
    amm_config: Address,
    token_0_vault: Address,
    token_1_vault: Address,
    token_0_mint: Address,
    token_1_mint: Address,
    token_0_program: Address,
    token_1_program: Address,
    observation_key: Address,
    auth_bump: u8,
}

impl RaydiumCpSwapPoolInfo {
    async fn load(rpc_client: &RpcClient, pool: Address) -> Result<Self, BoxError> {
        let account = rpc_client.get_account(&pool).await?;
        if account.owner != RAYDIUM_CP_SWAP_PROGRAM_ID {
            return Err(io::Error::other(format!(
                "pool {} is owned by {}, expected raydium cp-swap program {}",
                pool, account.owner, RAYDIUM_CP_SWAP_PROGRAM_ID
            ))
            .into());
        }

        Self::from_account_data(pool, &account.data)
    }

    fn from_account_data(pool: Address, data: &[u8]) -> Result<Self, BoxError> {
        if data.len() < POOL_STATE_RAW_LEN {
            return Err(io::Error::other(format!(
                "pool {} account data is too short: {} bytes",
                pool,
                data.len()
            ))
            .into());
        }

        let pool_state = bytemuck::try_pod_read_unaligned::<PoolStateRaw>(&data[..POOL_STATE_RAW_LEN])
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to decode raydium pool {} with bytemuck: {err}",
                    pool
                ))
            })?;

        if pool_state.discriminator != POOL_STATE_DISCRIMINATOR {
            return Err(io::Error::other(format!(
                "pool {} does not have the raydium cp-swap PoolState discriminator",
                pool
            ))
            .into());
        }

        Ok(Self {
            pool,
            amm_config: pool_state.amm_config,
            token_0_vault: pool_state.token_0_vault,
            token_1_vault: pool_state.token_1_vault,
            token_0_mint: pool_state.token_0_mint,
            token_1_mint: pool_state.token_1_mint,
            token_0_program: pool_state.token_0_program,
            token_1_program: pool_state.token_1_program,
            observation_key: pool_state.observation_key,
            auth_bump: pool_state.auth_bump,
        })
    }

    fn authority(&self) -> Result<Address, BoxError> {
        Address::create_program_address(
            &[RAYDIUM_AUTH_SEED, &[self.auth_bump]],
            &RAYDIUM_CP_SWAP_PROGRAM_ID,
        )
        .map_err(|err| {
            io::Error::other(format!(
                "failed to derive raydium authority for pool {} with bump {}: {err}",
                self.pool, self.auth_bump
            ))
            .into()
        })
    }

    fn round_trip_route(
        &self,
        user: Address,
        first_leg_input_mint: Address,
    ) -> Result<RoundTripRoute, BoxError> {
        let first_leg = if first_leg_input_mint == self.token_0_mint {
            SwapLeg::new(
                user,
                self.token_0_mint,
                self.token_1_mint,
                self.token_0_vault,
                self.token_1_vault,
                self.token_0_program,
                self.token_1_program,
            )
        } else if first_leg_input_mint == self.token_1_mint {
            SwapLeg::new(
                user,
                self.token_1_mint,
                self.token_0_mint,
                self.token_1_vault,
                self.token_0_vault,
                self.token_1_program,
                self.token_0_program,
            )
        } else {
            return Err(io::Error::other(format!(
                "input mint {} is not part of raydium pool {}",
                first_leg_input_mint, self.pool
            ))
            .into());
        };

        Ok(RoundTripRoute {
            user_input_token_account: first_leg.input_token_account,
            user_output_token_account: first_leg.output_token_account,
            second_leg: first_leg.reversed(),
            first_leg,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct SwapLeg {
    input_token_account: Address,
    output_token_account: Address,
    input_vault: Address,
    output_vault: Address,
    input_token_program: Address,
    output_token_program: Address,
    input_mint: Address,
    output_mint: Address,
}

impl SwapLeg {
    fn new(
        user: Address,
        input_mint: Address,
        output_mint: Address,
        input_vault: Address,
        output_vault: Address,
        input_token_program: Address,
        output_token_program: Address,
    ) -> Self {
        Self {
            input_token_account: get_associated_token_address_with_program_id(
                &user,
                &input_mint,
                &input_token_program,
            ),
            output_token_account: get_associated_token_address_with_program_id(
                &user,
                &output_mint,
                &output_token_program,
            ),
            input_vault,
            output_vault,
            input_token_program,
            output_token_program,
            input_mint,
            output_mint,
        }
    }

    fn reversed(self) -> Self {
        Self {
            input_token_account: self.output_token_account,
            output_token_account: self.input_token_account,
            input_vault: self.output_vault,
            output_vault: self.input_vault,
            input_token_program: self.output_token_program,
            output_token_program: self.input_token_program,
            input_mint: self.output_mint,
            output_mint: self.input_mint,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RoundTripRoute {
    user_input_token_account: Address,
    user_output_token_account: Address,
    first_leg: SwapLeg,
    second_leg: SwapLeg,
}

async fn build_initialize_user_token_account_instructions(
    rpc_client: &RpcClient,
    payer: Address,
    route: &RoundTripRoute,
) -> Result<Vec<Instruction>, BoxError> {
    let accounts = rpc_client.get_multiple_accounts(&[
        route.user_input_token_account,
        route.user_output_token_account,
    ])
    .await?;
    let mut instructions = Vec::new();

    if accounts[0].is_none() {
        info!(
            token_account = %route.user_input_token_account,
            mint = %route.first_leg.input_mint,
            "initializing missing input token account",
        );
        instructions.push(create_associated_token_account_idempotent(
            &payer,
            &payer,
            &route.first_leg.input_mint,
            &route.first_leg.input_token_program,
        ));
    }

    if accounts[1].is_none() {
        info!(
            token_account = %route.user_output_token_account,
            mint = %route.first_leg.output_mint,
            "initializing missing output token account",
        );
        instructions.push(create_associated_token_account_idempotent(
            &payer,
            &payer,
            &route.first_leg.output_mint,
            &route.first_leg.output_token_program,
        ));
    }

    Ok(instructions)
}

fn build_swap_base_output_transaction(
    signer: &Keypair,
    blockhash: Hash,
    pool_info: &RaydiumCpSwapPoolInfo,
    leg: &SwapLeg,
    max_input_amount: u64,
    exact_output_amount: u64,
    uniquifier_memo: &str,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<VersionedTransaction, BoxError> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SWAP_BASE_OUTPUT_DISCRIMINATOR);
    data.extend_from_slice(&max_input_amount.to_le_bytes());
    data.extend_from_slice(&exact_output_amount.to_le_bytes());

    let mut instructions = Vec::with_capacity(3);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.push(build_swap_instruction(signer.pubkey(), pool_info, leg, data)?);
    instructions.push(build_uniquifier_memo_instruction(
        signer.pubkey(),
        uniquifier_memo,
    ));

    Ok(build_transaction(signer, blockhash, &instructions))
}

fn build_swap_base_input_transaction(
    signer: &Keypair,
    blockhash: Hash,
    pool_info: &RaydiumCpSwapPoolInfo,
    leg: &SwapLeg,
    exact_input_amount: u64,
    minimum_output_amount: u64,
    uniquifier_memo: &str,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<VersionedTransaction, BoxError> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SWAP_BASE_INPUT_DISCRIMINATOR);
    data.extend_from_slice(&exact_input_amount.to_le_bytes());
    data.extend_from_slice(&minimum_output_amount.to_le_bytes());

    let mut instructions = Vec::with_capacity(3);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.push(build_swap_instruction(signer.pubkey(), pool_info, leg, data)?);
    instructions.push(build_uniquifier_memo_instruction(
        signer.pubkey(),
        uniquifier_memo,
    ));

    Ok(build_transaction(signer, blockhash, &instructions))
}

fn build_swap_instruction(
    payer: Address,
    pool_info: &RaydiumCpSwapPoolInfo,
    leg: &SwapLeg,
    data: Vec<u8>,
) -> Result<Instruction, BoxError> {
    let authority = pool_info.authority()?;

    Ok(Instruction {
        program_id: RAYDIUM_CP_SWAP_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(authority, false),
            AccountMeta::new_readonly(pool_info.amm_config, false),
            AccountMeta::new(pool_info.pool, false),
            AccountMeta::new(leg.input_token_account, false),
            AccountMeta::new(leg.output_token_account, false),
            AccountMeta::new(leg.input_vault, false),
            AccountMeta::new(leg.output_vault, false),
            AccountMeta::new_readonly(leg.input_token_program, false),
            AccountMeta::new_readonly(leg.output_token_program, false),
            AccountMeta::new_readonly(leg.input_mint, false),
            AccountMeta::new_readonly(leg.output_mint, false),
            AccountMeta::new(pool_info.observation_key, false),
        ],
        data,
    })
}

fn build_transaction(
    signer: &Keypair,
    blockhash: Hash,
    instructions: &[Instruction],
) -> VersionedTransaction {
    VersionedTransaction::from(Transaction::new_signed_with_payer(
        instructions,
        Some(&signer.pubkey()),
        &[signer],
        blockhash,
    ))
}

fn build_uniquifier_memo_instruction(signer: Address, memo: &str) -> Instruction {
    spl_memo_interface::instruction::build_memo(
        &spl_memo_interface::v3::id(),
        memo.as_bytes(),
        &[&signer],
    )
}

fn slot_uniquifier_memo(target_slot: Slot, transaction_index: usize) -> String {
    format!("slot:{target_slot}:tx:{transaction_index}")
}

#[derive(Clone, Copy, Debug)]
struct FirstLegSimulation {
    user_output_amount: u64,
    pool_input_amount: u64,
    pool_output_amount: u64,
    price_impact_bps: u64,
}

async fn simulate_first_leg(
    rpc_client: &RpcClient,
    first_leg_transaction: &VersionedTransaction,
    first_leg: &SwapLeg,
) -> Result<FirstLegSimulation, BoxError> {
    let account_config = RpcSimulateTransactionAccountsConfig {
        encoding: Some(UiAccountEncoding::Base64),
        addresses: vec![
            first_leg.output_token_account.to_string(),
            first_leg.input_vault.to_string(),
            first_leg.output_vault.to_string(),
        ],
    };
    let simulation_result = rpc_client
        .simulate_bundle_with_config(
            std::slice::from_ref(first_leg_transaction),
            RpcSimulateBundleConfig {
                pre_execution_accounts_configs: vec![Some(account_config.clone())],
                post_execution_accounts_configs: vec![Some(account_config)],
                simulation_bank: Some(SimulationSlotConfig::Commitment(
                    CommitmentConfig::processed(),
                )),
                replace_recent_blockhash: false,
                skip_sig_verify: false,
                ..RpcSimulateBundleConfig::default()
            },
        )
        .await?
        .value;

    match simulation_result.summary {
        RpcBundleSimulationSummary::Succeeded => {}
        _ => {
            return Err(io::Error::other(format!(
                "failed to simulate first raydium leg: summary={:?} transaction_results={:?}",
                simulation_result.summary, simulation_result.transaction_results
            ))
            .into())
        }
    }

    let transaction_result = simulation_result
        .transaction_results
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("missing simulation result for first raydium leg"))?;
    let user_output_before = parse_token_amount(
        transaction_result
            .pre_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.first()),
    )?;
    let user_output_after = parse_token_amount(
        transaction_result
            .post_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.first()),
    )?;
    let input_vault_before = parse_token_amount(
        transaction_result
            .pre_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.get(1)),
    )?;
    let input_vault_after = parse_token_amount(
        transaction_result
            .post_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.get(1)),
    )?;
    let output_vault_before = parse_token_amount(
        transaction_result
            .pre_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.get(2)),
    )?;
    let output_vault_after = parse_token_amount(
        transaction_result
            .post_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.get(2)),
    )?;

    let user_output_amount = user_output_after.checked_sub(user_output_before).ok_or_else(|| {
        io::Error::other(format!(
            "simulated first raydium leg decreased output token account unexpectedly: pre={} post={}",
            user_output_before, user_output_after
        ))
    })?;
    let pool_input_amount = input_vault_after.checked_sub(input_vault_before).ok_or_else(|| {
        io::Error::other(format!(
            "simulated first raydium leg decreased input vault unexpectedly: pre={} post={}",
            input_vault_before, input_vault_after
        ))
    })?;
    let pool_output_amount = output_vault_before.checked_sub(output_vault_after).ok_or_else(|| {
        io::Error::other(format!(
            "simulated first raydium leg increased output vault unexpectedly: pre={} post={}",
            output_vault_before, output_vault_after
        ))
    })?;
    let price_impact_bps = compute_price_impact_bps(
        input_vault_before,
        output_vault_before,
        pool_input_amount,
        pool_output_amount,
    )?;

    Ok(FirstLegSimulation {
        user_output_amount,
        pool_input_amount,
        pool_output_amount,
        price_impact_bps,
    })
}

fn parse_token_amount(ui_account: Option<&UiAccount>) -> Result<u64, BoxError> {
    let ui_account = ui_account
        .ok_or_else(|| io::Error::other("missing token account state in simulation response"))?;
    if let UiAccountData::Json(parsed_account) = &ui_account.data {
        let amount = parsed_account
            .parsed
            .get("info")
            .and_then(|value| value.get("tokenAmount"))
            .and_then(|value| value.get("amount"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| io::Error::other("missing token amount in json simulation response"))?;
        return amount.parse::<u64>().map_err(|err| {
            io::Error::other(format!("invalid token amount in json simulation response: {err}"))
                .into()
        });
    }

    let data = ui_account
        .data
        .decode()
        .ok_or_else(|| io::Error::other("failed to decode binary token account data from simulation response"))?;
    let amount_bytes = data
        .get(64..72)
        .ok_or_else(|| io::Error::other("token account data too short to read amount"))?;

    Ok(u64::from_le_bytes(amount_bytes.try_into().map_err(|_| {
        io::Error::other("failed to parse token account amount bytes from simulation response")
    })?))
}

fn inflate_amount_by_five_percent(amount: u64) -> Result<u64, BoxError> {
    amount
        .checked_mul(105)
        .and_then(|value| value.checked_add(99))
        .map(|value| value / 100)
        .filter(|value| *value > 0)
        .ok_or_else(|| io::Error::other("failed to compute 5% slippage-adjusted amount").into())
}

fn compute_price_impact_bps(
    input_vault_before: u64,
    output_vault_before: u64,
    pool_input_amount: u64,
    pool_output_amount: u64,
) -> Result<u64, BoxError> {
    if input_vault_before == 0 || output_vault_before == 0 {
        return Err(io::Error::other("raydium pool reserves must be non-zero").into());
    }

    let expected_output_amount = (u128::from(pool_input_amount) * u128::from(output_vault_before))
        / u128::from(input_vault_before);
    if expected_output_amount == 0 {
        return Ok(0);
    }

    let shortfall = expected_output_amount.saturating_sub(u128::from(pool_output_amount));
    Ok(((shortfall * 10_000) / expected_output_amount) as u64)
}

pub fn simulation_account_configs(
    prepared: &PreparedRaydiumCpSwap,
    transaction_count: usize,
) -> Result<
    (
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    ),
    BoxError,
> {
    let account_config = Some(RpcSimulateTransactionAccountsConfig {
        encoding: Some(UiAccountEncoding::Base64),
        addresses: vec![prepared.route.user_input_token_account.to_string()],
    });

    Ok((
        vec![account_config.clone(); transaction_count],
        vec![account_config; transaction_count],
    ))
}

pub fn log_post_simulation(
    prepared: &PreparedRaydiumCpSwap,
    simulation_result: &RpcSimulateBundleResult,
) -> Result<(), BoxError> {
    let first_transaction = simulation_result
        .transaction_results
        .first()
        .ok_or_else(|| io::Error::other("missing first transaction simulation result"))?;
    let last_transaction = simulation_result
        .transaction_results
        .last()
        .ok_or_else(|| io::Error::other("missing last transaction simulation result"))?;
    let first_leg_input_before = parse_token_amount(
        first_transaction
            .pre_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.first()),
    )?;
    let second_leg_input_before = parse_token_amount(
        last_transaction
            .pre_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.first()),
    )?;
    let second_leg_input_after = parse_token_amount(
        last_transaction
            .post_execution_accounts
            .as_ref()
            .and_then(|accounts| accounts.first()),
    )?;
    let returned_input_amount = second_leg_input_after
        .checked_sub(second_leg_input_before)
        .ok_or_else(|| {
            io::Error::other(format!(
                "simulated second raydium leg reduced input token account unexpectedly: before={} after={}",
                second_leg_input_before, second_leg_input_after
            ))
        })?;
    let bundle_input_delta =
        i128::from(second_leg_input_after) - i128::from(first_leg_input_before);

    info!(
        pool = %prepared.pool_info.pool,
        input_mint = %prepared.route.first_leg.input_mint,
        returned_input_amount,
        bundle_input_delta,
        "simulated raydium bundle input returned after unwinding intermediate output",
    );

    Ok(())
}

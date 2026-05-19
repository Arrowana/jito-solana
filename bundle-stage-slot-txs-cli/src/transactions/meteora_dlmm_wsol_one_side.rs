use {
    crate::{
        error::BoxError,
        slot_assert::build_assert_slot_instruction,
        transactions::ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs,
    },
    anchor_lang::{
        declare_program, solana_program::pubkey::Pubkey as AnchorPubkey, AnchorSerialize,
        Discriminator,
    },
    solana_account::Account,
    solana_address::{address, Address},
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_signer::Signer,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
    spl_associated_token_account_interface::{
        address::get_associated_token_address_with_program_id,
        instruction::create_associated_token_account_idempotent,
    },
    std::{io, mem::size_of},
    tracing::info,
};

declare_program!(dlmm);

const WSOL_MINT: Address = address!("So11111111111111111111111111111111111111112");
const RAY_MINT: Address = address!("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R");
const METEORA_DLMM_PROGRAM_ID: Address = address!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");
const RENT_SYSVAR_ID: Address = address!("SysvarRent111111111111111111111111111111111");
const MEMO_PROGRAM_ID: Address = address!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const DLMM_BINS_PER_ARRAY: i32 = 70;
const BASIS_POINTS: u16 = 10_000;
const TOKEN_ACCOUNT_AMOUNT_OFFSET: usize = 64;
const DEFAULT_WSOL_USDC_PRICE: f64 = 90.0;
const DEFAULT_RAY_USDC_PRICE: f64 = 0.631;

pub(crate) struct PreparedMeteoraDlmmAddRemoveWsolLiquidity {
    pair: DlmmPairInfo,
    position: Address,
    base: Address,
    target_bin_id: i32,
    target_bin_array: Address,
    wsol_amount: u64,
    target_implied_wsol_usdc_price: f64,
    reference_wsol_usdc_price: f64,
    target_wsol_discount_bps: i64,
    initialize_position: bool,
    token_x_program: Address,
    token_y_program: Address,
    user_token_x: Address,
    user_token_y: Address,
}

#[derive(Clone, Copy, Debug)]
struct DlmmPairInfo {
    pair: Address,
    token_x_mint: Address,
    token_y_mint: Address,
    reserve_x: Address,
    reserve_y: Address,
    active_id: i32,
    bin_step: u16,
}

pub(super) async fn prepare_transactions(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs,
    _target_slot: Slot,
) -> Result<PreparedMeteoraDlmmAddRemoveWsolLiquidity, BoxError> {
    prepare_transactions_inner(rpc_client, signer, args, true, false).await
}

pub(super) async fn run_target_setup(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs,
) -> Result<(), BoxError> {
    let pair = DlmmPairInfo::load(rpc_client, args.pair).await?;
    let target_bin_id = target_bin_id_for_wsol_side(&pair)?;
    let target_bin_array = derive_bin_array_address(pair.pair, target_bin_id);
    let target_bin_array_index = bin_array_index_for_bin_id(target_bin_id);
    ensure_bin_array_exists(rpc_client, &pair, target_bin_array, target_bin_array_index).await?;

    let position = derive_position_address(pair.pair, signer.pubkey(), target_bin_id, 1);
    ensure_position_initialized(rpc_client, signer, &pair, position, target_bin_id).await?;
    Ok(())
}

async fn prepare_transactions_inner(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs,
    require_existing_wsol: bool,
    initialize_position_in_setup_phase: bool,
) -> Result<PreparedMeteoraDlmmAddRemoveWsolLiquidity, BoxError> {
    let pair = DlmmPairInfo::load(rpc_client, args.pair).await?;
    let wsol_side = pair.wsol_side()?;
    let target_bin_id = target_bin_id_for_wsol_side(&pair)?;
    let target_implied_wsol_usdc_price =
        pair.implied_wsol_usdc_price_at_bin(rpc_client, target_bin_id).await?;
    let reference_wsol_usdc_price = default_token_usdc_price(WSOL_MINT)?;
    let target_wsol_discount_bps =
        (((target_implied_wsol_usdc_price / reference_wsol_usdc_price) - 1.0) * 10_000.0).round()
            as i64;
    if target_wsol_discount_bps > -args.min_wsol_discount_bps {
        return Err(io::Error::other(format!(
            "target bin {} does not make WSOL cheap enough: implied=${:.6}, reference=${:.6}, discount_bps={}, required <= -{}",
            target_bin_id,
            target_implied_wsol_usdc_price,
            reference_wsol_usdc_price,
            target_wsol_discount_bps,
            args.min_wsol_discount_bps,
        ))
        .into());
    }

    let target_bin_array = derive_bin_array_address(pair.pair, target_bin_id);
    let target_bin_array_index = bin_array_index_for_bin_id(target_bin_id);
    let target_bin_index = bin_index_in_array(target_bin_id);
    ensure_bin_array_exists(rpc_client, &pair, target_bin_array, target_bin_array_index).await?;
    ensure_target_bin_empty(rpc_client, target_bin_array, target_bin_index).await?;

    let base = signer.pubkey();
    let position = derive_position_address(pair.pair, base, target_bin_id, 1);
    let token_x_program = mint_owner(rpc_client, pair.token_x_mint).await?;
    let token_y_program = mint_owner(rpc_client, pair.token_y_mint).await?;
    let user_token_x =
        get_associated_token_address_with_program_id(&signer.pubkey(), &pair.token_x_mint, &token_x_program);
    let user_token_y =
        get_associated_token_address_with_program_id(&signer.pubkey(), &pair.token_y_mint, &token_y_program);
    let user_wsol = match wsol_side {
        WsolSide::X => user_token_x,
        WsolSide::Y => user_token_y,
    };
    let existing_wsol_amount = get_optional_token_account_amount(rpc_client, user_wsol).await?;
    if require_existing_wsol && existing_wsol_amount < args.wsol_amount {
        return Err(io::Error::other(format!(
            "insufficient existing WSOL for DLMM liquidity: user_wsol={}, available={}, required={}",
            user_wsol, existing_wsol_amount, args.wsol_amount
        ))
            .into());
    }
    let initialized_position = if initialize_position_in_setup_phase {
        ensure_position_initialized(rpc_client, signer, &pair, position, target_bin_id).await?
    } else if require_existing_wsol {
        ensure_position_exists(rpc_client, &pair, position, target_bin_id).await?;
        false
    } else {
        false
    };

    info!(
        pair = %pair.pair,
        active_id = pair.active_id,
        target_bin_id,
        target_bin_array = %target_bin_array,
        target_bin_array_index,
        target_bin_index,
        wsol_side = ?wsol_side,
        position = %position,
        initialized_position,
        target_bin_array_exists = true,
        wsol_amount = args.wsol_amount,
        existing_wsol_amount,
        target_implied_wsol_usdc_price,
        reference_wsol_usdc_price,
        target_wsol_discount_bps,
        "prepared meteora dlmm one-sided wsol liquidity",
    );

    Ok(PreparedMeteoraDlmmAddRemoveWsolLiquidity {
        pair,
        position,
        base,
        target_bin_id,
        target_bin_array,
        wsol_amount: args.wsol_amount,
        target_implied_wsol_usdc_price,
        reference_wsol_usdc_price,
        target_wsol_discount_bps,
        initialize_position: false,
        token_x_program,
        token_y_program,
        user_token_x,
        user_token_y,
    })
}

pub(super) fn build_transactions(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<Vec<VersionedTransaction>, BoxError> {
    Ok(vec![
        build_add_wsol_liquidity_transaction(
            prepared,
            signer,
            blockhash,
            target_slot,
            include_slot_assert,
        )?,
        build_remove_liquidity_transaction(
            prepared,
            signer,
            blockhash,
            target_slot,
            include_slot_assert,
        )?,
    ])
}

pub(super) fn log_post_simulation(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    _simulation_result: &solana_rpc_client_api::bundles::RpcSimulateBundleResult,
) -> Result<(), BoxError> {
    info!(
        pair = %prepared.pair.pair,
        position = %prepared.position,
        target_bin_id = prepared.target_bin_id,
        target_implied_wsol_usdc_price = prepared.target_implied_wsol_usdc_price,
        reference_wsol_usdc_price = prepared.reference_wsol_usdc_price,
        target_wsol_discount_bps = prepared.target_wsol_discount_bps,
        "simulated meteora dlmm add/remove one-sided wsol liquidity",
    );
    Ok(())
}

fn build_add_wsol_liquidity_transaction(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<VersionedTransaction, BoxError> {
    let wsol_side = prepared.pair.wsol_side()?;
    let (user_wsol, reserve_wsol, token_mint, token_program) = match wsol_side {
        WsolSide::X => (
            prepared.user_token_x,
            prepared.pair.reserve_x,
            prepared.pair.token_x_mint,
            prepared.token_x_program,
        ),
        WsolSide::Y => (
            prepared.user_token_y,
            prepared.pair.reserve_y,
            prepared.pair.token_y_mint,
            prepared.token_y_program,
        ),
    };

    let mut instructions = Vec::with_capacity(12);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.extend(build_setup_instructions(prepared, signer.pubkey())?);
    instructions.push(build_add_liquidity_one_side_precise2_instruction(
        prepared,
        signer.pubkey(),
        user_wsol,
        reserve_wsol,
        token_mint,
        token_program,
    )?);
    instructions.push(build_uniquifier_memo_instruction(
        signer.pubkey(),
        &format!("slot:{target_slot}:dlmm:add"),
    ));

    Ok(build_transaction(signer, blockhash, &instructions))
}

fn build_remove_liquidity_transaction(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<VersionedTransaction, BoxError> {
    let mut instructions = Vec::with_capacity(4);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.push(build_remove_liquidity_by_range2_instruction(
        prepared,
        signer.pubkey(),
    )?);
    instructions.push(build_uniquifier_memo_instruction(
        signer.pubkey(),
        &format!("slot:{target_slot}:dlmm:remove"),
    ));

    Ok(build_transaction(signer, blockhash, &instructions))
}

fn build_setup_instructions(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: Address,
) -> Result<Vec<Instruction>, BoxError> {
    let mut instructions = Vec::new();
    instructions.push(create_associated_token_account_idempotent(
        &signer,
        &signer,
        &prepared.pair.token_x_mint,
        &prepared.token_x_program,
    ));
    instructions.push(create_associated_token_account_idempotent(
        &signer,
        &signer,
        &prepared.pair.token_y_mint,
        &prepared.token_y_program,
    ));
    if prepared.initialize_position {
        instructions.push(build_initialize_position_pda_instruction(prepared, signer)?);
    }
    Ok(instructions)
}

fn build_initialize_position_pda_instruction(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: Address,
) -> Result<Instruction, BoxError> {
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(dlmm::client::args::InitializePositionPda::DISCRIMINATOR);
    data.extend_from_slice(&prepared.target_bin_id.to_le_bytes());
    data.extend_from_slice(&1_i32.to_le_bytes());

    Ok(Instruction {
        program_id: METEORA_DLMM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new_readonly(prepared.base, true),
            AccountMeta::new(prepared.position, false),
            AccountMeta::new_readonly(prepared.pair.pair, false),
            AccountMeta::new(signer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
            AccountMeta::new_readonly(event_authority(), false),
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM_ID, false),
        ],
        data,
    })
}

fn build_add_liquidity_one_side_precise2_instruction(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: Address,
    user_token: Address,
    reserve: Address,
    token_mint: Address,
    token_program: Address,
) -> Result<Instruction, BoxError> {
    let (compressed_amount, decompress_multiplier) = compressed_deposit_amount(prepared.wsol_amount);
    let liquidity_parameter = dlmm::types::AddLiquiditySingleSidePreciseParameter2 {
        bins: vec![dlmm::types::CompressedBinDepositAmount {
            bin_id: prepared.target_bin_id,
            amount: compressed_amount,
        }],
        decompress_multiplier,
        max_amount: prepared.wsol_amount,
    };
    let remaining_accounts_info = dlmm::types::RemainingAccountsInfo { slices: Vec::new() };
    let mut data = Vec::new();
    data.extend_from_slice(dlmm::client::args::AddLiquidityOneSidePrecise2::DISCRIMINATOR);
    liquidity_parameter.serialize(&mut data)?;
    remaining_accounts_info.serialize(&mut data)?;

    Ok(Instruction {
        program_id: METEORA_DLMM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(prepared.position, false),
            AccountMeta::new(prepared.pair.pair, false),
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM_ID, false),
            AccountMeta::new(user_token, false),
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(token_mint, false),
            AccountMeta::new(signer, true),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(event_authority(), false),
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM_ID, false),
            AccountMeta::new(prepared.target_bin_array, false),
        ],
        data,
    })
}

fn build_remove_liquidity_by_range2_instruction(
    prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    signer: Address,
) -> Result<Instruction, BoxError> {
    let remaining_accounts_info = dlmm::types::RemainingAccountsInfo { slices: Vec::new() };
    let mut data = Vec::new();
    data.extend_from_slice(dlmm::client::args::RemoveLiquidityByRange2::DISCRIMINATOR);
    data.extend_from_slice(&prepared.target_bin_id.to_le_bytes());
    data.extend_from_slice(&prepared.target_bin_id.to_le_bytes());
    data.extend_from_slice(&BASIS_POINTS.to_le_bytes());
    remaining_accounts_info.serialize(&mut data)?;

    Ok(Instruction {
        program_id: METEORA_DLMM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(prepared.position, false),
            AccountMeta::new(prepared.pair.pair, false),
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM_ID, false),
            AccountMeta::new(prepared.user_token_x, false),
            AccountMeta::new(prepared.user_token_y, false),
            AccountMeta::new(prepared.pair.reserve_x, false),
            AccountMeta::new(prepared.pair.reserve_y, false),
            AccountMeta::new_readonly(prepared.pair.token_x_mint, false),
            AccountMeta::new_readonly(prepared.pair.token_y_mint, false),
            AccountMeta::new(signer, true),
            AccountMeta::new_readonly(prepared.token_x_program, false),
            AccountMeta::new_readonly(prepared.token_y_program, false),
            AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
            AccountMeta::new_readonly(event_authority(), false),
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM_ID, false),
            AccountMeta::new(prepared.target_bin_array, false),
        ],
        data,
    })
}

fn compressed_deposit_amount(amount: u64) -> (u32, u64) {
    let decompress_multiplier = (amount / u64::from(u32::MAX)).saturating_add(1);
    let compressed_amount = (amount / decompress_multiplier)
        .try_into()
        .expect("decompress multiplier keeps amount within u32");
    (compressed_amount, decompress_multiplier)
}

fn target_bin_id_for_wsol_side(pair: &DlmmPairInfo) -> Result<i32, BoxError> {
    Ok(match pair.wsol_side()? {
        WsolSide::X => pair.active_id + 1,
        WsolSide::Y => pair.active_id - 1,
    })
}

impl DlmmPairInfo {
    async fn load(rpc_client: &RpcClient, pair: Address) -> Result<Self, BoxError> {
        let account = rpc_client.get_account(&pair).await?;
        if account.owner != METEORA_DLMM_PROGRAM_ID {
            return Err(io::Error::other(format!(
                "dlmm pair {} is owned by {}, expected {}",
                pair, account.owner, METEORA_DLMM_PROGRAM_ID
            ))
            .into());
        }
        let lb_pair = bytemuck::try_pod_read_unaligned::<dlmm::accounts::LbPair>(
            account
                .data
                .get(8..8 + size_of::<dlmm::accounts::LbPair>())
                .ok_or_else(|| io::Error::other(format!("dlmm pair {pair} data is too short")))?,
        )
        .map_err(|err| io::Error::other(format!("failed to decode dlmm pair {pair}: {err}")))?;

        Ok(Self {
            pair,
            token_x_mint: pubkey_to_address(lb_pair.token_x_mint),
            token_y_mint: pubkey_to_address(lb_pair.token_y_mint),
            reserve_x: pubkey_to_address(lb_pair.reserve_x),
            reserve_y: pubkey_to_address(lb_pair.reserve_y),
            active_id: lb_pair.active_id,
            bin_step: lb_pair.bin_step,
        })
    }

    fn wsol_side(&self) -> Result<WsolSide, BoxError> {
        match (self.token_x_mint == WSOL_MINT, self.token_y_mint == WSOL_MINT) {
            (true, false) => Ok(WsolSide::X),
            (false, true) => Ok(WsolSide::Y),
            _ => Err(io::Error::other(format!("pair {} does not contain WSOL", self.pair)).into()),
        }
    }

    async fn implied_wsol_usdc_price_at_bin(
        &self,
        rpc_client: &RpcClient,
        bin_id: i32,
    ) -> Result<f64, BoxError> {
        let x_metadata = token_metadata(rpc_client, self.token_x_mint).await?;
        let y_metadata = token_metadata(rpc_client, self.token_y_mint).await?;
        let raw_price_y_per_x = (1.0 + (f64::from(self.bin_step) / 10_000.0)).powi(bin_id);
        let active_price_y_per_x = raw_price_y_per_x
            * 10_f64.powi(i32::from(x_metadata.decimals) - i32::from(y_metadata.decimals));
        let implied = if self.token_x_mint == WSOL_MINT {
            active_price_y_per_x * y_metadata.usd_price
        } else {
            x_metadata.usd_price / active_price_y_per_x
        };
        Ok(implied)
    }
}

#[derive(Clone, Copy, Debug)]
enum WsolSide {
    X,
    Y,
}

#[derive(Clone, Copy)]
struct TokenMetadata {
    decimals: u8,
    usd_price: f64,
}

async fn token_metadata(rpc_client: &RpcClient, mint: Address) -> Result<TokenMetadata, BoxError> {
    let decimals = rpc_client
        .get_token_supply(&mint)
        .await
        .map_err(|err| io::Error::other(format!("failed to get token supply for {mint}: {err}")))?
        .decimals;
    let usd_price = default_token_usdc_price(mint)?;
    Ok(TokenMetadata { decimals, usd_price })
}

async fn mint_owner(rpc_client: &RpcClient, mint: Address) -> Result<Address, BoxError> {
    Ok(rpc_client.get_account(&mint).await?.owner)
}

async fn ensure_target_bin_empty(
    rpc_client: &RpcClient,
    bin_array_address: Address,
    bin_index: usize,
) -> Result<(), BoxError> {
    let Ok(account) = rpc_client.get_account(&bin_array_address).await else {
        return Ok(());
    };
    let bin_array = decode_bin_array(bin_array_address, &account)?;
    let bin = bin_array.bins[bin_index];
    if bin.amount_x != 0
        || bin.amount_y != 0
        || bin.liquidity_supply != 0
        || bin.open_order_amount != 0
        || bin.total_processing_order_amount != 0
        || bin.processed_order_remaining_amount != 0
    {
        return Err(io::Error::other(format!(
            "target bin array {} bin {} is not empty: amount_x={}, amount_y={}, liquidity={}",
            bin_array_address, bin_index, bin.amount_x, bin.amount_y, bin.liquidity_supply,
        ))
        .into());
    }
    Ok(())
}

async fn get_optional_token_account_amount(
    rpc_client: &RpcClient,
    token_account: Address,
) -> Result<u64, BoxError> {
    let account = rpc_client
        .get_account_with_commitment(&token_account, CommitmentConfig::processed())
        .await?
        .value;
    account
        .as_ref()
        .map(parse_token_account_amount)
        .transpose()
        .map(|amount| amount.unwrap_or(0))
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

async fn ensure_position_initialized(
    rpc_client: &RpcClient,
    signer: &Keypair,
    pair: &DlmmPairInfo,
    position: Address,
    target_bin_id: i32,
) -> Result<bool, BoxError> {
    if rpc_client.get_account(&position).await.is_ok() {
        info!(
            pair = %pair.pair,
            position = %position,
            target_bin_id,
            "meteora dlmm position already exists during setup phase",
        );
        return Ok(false);
    }

    let setup_prepared = PreparedMeteoraDlmmAddRemoveWsolLiquidity {
        pair: *pair,
        position,
        base: signer.pubkey(),
        target_bin_id,
        target_bin_array: derive_bin_array_address(pair.pair, target_bin_id),
        wsol_amount: 0,
        target_implied_wsol_usdc_price: 0.0,
        reference_wsol_usdc_price: 0.0,
        target_wsol_discount_bps: 0,
        initialize_position: true,
        token_x_program: spl_token_interface::id(),
        token_y_program: spl_token_interface::id(),
        user_token_x: signer.pubkey(),
        user_token_y: signer.pubkey(),
    };
    let instruction = build_initialize_position_pda_instruction(&setup_prepared, signer.pubkey())?;
    let blockhash = rpc_client.get_latest_blockhash().await?;
    let transaction = build_transaction(signer, blockhash, &[instruction]);
    let signature = match rpc_client.send_and_confirm_transaction(&transaction).await {
        Ok(signature) => signature,
        Err(err) => {
            if rpc_client.get_account(&position).await.is_ok() {
                info!(
                    pair = %pair.pair,
                    position = %position,
                    target_bin_id,
                    err = %err,
                    "meteora dlmm position pda was initialized by a concurrent setup task",
                );
                return Ok(false);
            }
            return Err(err.into());
        }
    };
    info!(
        pair = %pair.pair,
        position = %position,
        target_bin_id,
        signature = %signature,
        "initialized meteora dlmm position pda during setup phase",
    );
    Ok(true)
}

async fn ensure_position_exists(
    rpc_client: &RpcClient,
    pair: &DlmmPairInfo,
    position: Address,
    target_bin_id: i32,
) -> Result<(), BoxError> {
    if rpc_client.get_account(&position).await.is_ok() {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "meteora dlmm position pda is missing during arming; setup should have run earlier: pair={}, position={}, target_bin_id={}",
        pair.pair, position, target_bin_id
    ))
    .into())
}

async fn ensure_bin_array_exists(
    rpc_client: &RpcClient,
    pair: &DlmmPairInfo,
    target_bin_array: Address,
    target_bin_array_index: i64,
) -> Result<(), BoxError> {
    if rpc_client.get_account(&target_bin_array).await.is_ok() {
        info!(
            pair = %pair.pair,
            target_bin_array = %target_bin_array,
            target_bin_array_index,
            "meteora dlmm bin array already exists during setup phase",
        );
        return Ok(());
    }

    Err(io::Error::other(format!(
        "target bin array does not exist; refusing to initialize it to avoid bin-array rent cost: pair={}, target_bin_array={}, target_bin_array_index={}",
        pair.pair, target_bin_array, target_bin_array_index
    ))
    .into())
}

fn decode_bin_array(
    address: Address,
    account: &Account,
) -> Result<dlmm::accounts::BinArray, BoxError> {
    if account.owner != METEORA_DLMM_PROGRAM_ID {
        return Err(io::Error::other(format!(
            "bin array {} is owned by {}, expected {}",
            address, account.owner, METEORA_DLMM_PROGRAM_ID
        ))
        .into());
    }
    if account.data.get(..8) != Some(dlmm::accounts::BinArray::DISCRIMINATOR) {
        return Err(io::Error::other(format!("bin array {} has wrong discriminator", address)).into());
    }
    bytemuck::try_pod_read_unaligned::<dlmm::accounts::BinArray>(
        account
            .data
            .get(8..8 + size_of::<dlmm::accounts::BinArray>())
            .ok_or_else(|| io::Error::other(format!("bin array {address} data too short")))?,
    )
    .map_err(|err| io::Error::other(format!("failed to decode bin array {address}: {err}")).into())
}

fn derive_position_address(pair: Address, base: Address, lower_bin_id: i32, width: i32) -> Address {
    let pair_pubkey = anchor_pubkey(pair);
    let base_pubkey = anchor_pubkey(base);
    let program_pubkey = anchor_pubkey(METEORA_DLMM_PROGRAM_ID);
    let lower_bin_id_bytes = lower_bin_id.to_le_bytes();
    let width_bytes = width.to_le_bytes();
    let (position, _) = AnchorPubkey::find_program_address(
        &[
            b"position",
            pair_pubkey.as_ref(),
            base_pubkey.as_ref(),
            &lower_bin_id_bytes,
            &width_bytes,
        ],
        &program_pubkey,
    );
    pubkey_to_address(position)
}

fn derive_bin_array_address(pair: Address, bin_id: i32) -> Address {
    let pair_pubkey = anchor_pubkey(pair);
    let program_pubkey = anchor_pubkey(METEORA_DLMM_PROGRAM_ID);
    let index_bytes = bin_array_index_for_bin_id(bin_id).to_le_bytes();
    let (bin_array, _) = AnchorPubkey::find_program_address(
        &[b"bin_array", pair_pubkey.as_ref(), &index_bytes],
        &program_pubkey,
    );
    pubkey_to_address(bin_array)
}

fn event_authority() -> Address {
    let program_pubkey = anchor_pubkey(METEORA_DLMM_PROGRAM_ID);
    let (event_authority, _) =
        AnchorPubkey::find_program_address(&[b"__event_authority"], &program_pubkey);
    pubkey_to_address(event_authority)
}

fn bin_array_index_for_bin_id(bin_id: i32) -> i64 {
    i64::from(bin_id.div_euclid(DLMM_BINS_PER_ARRAY))
}

fn bin_index_in_array(bin_id: i32) -> usize {
    bin_id.rem_euclid(DLMM_BINS_PER_ARRAY) as usize
}

fn anchor_pubkey(address: Address) -> AnchorPubkey {
    AnchorPubkey::new_from_array(address_to_array(address))
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

fn default_token_usdc_price(mint: Address) -> Result<f64, BoxError> {
    match mint {
        WSOL_MINT => Ok(DEFAULT_WSOL_USDC_PRICE),
        RAY_MINT => Ok(DEFAULT_RAY_USDC_PRICE),
        _ => Err(io::Error::other(format!(
            "missing hardcoded default USDC price for mint {mint}"
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        litesvm::LiteSVM,
        solana_rpc_client::nonblocking::rpc_client::RpcClient,
        solana_system_interface::instruction::transfer as system_transfer,
        std::{env, path::PathBuf},
    };

    const RAY_WSOL_PAIR: Address = address!("6jjGocGMvwtzXkUT1aA8S7XuyaGcaR17ZKiL1eqBJ9on");
    const AIRDROP_LAMPORTS: u64 = 20_000_000_000;
    const TEST_WSOL_AMOUNT: u64 = 10_000_000;

    #[test]
    fn litesvm_add_then_remove_wsol_liquidity_on_ray_pair() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            dotenvy::from_path(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env")).ok();

            let rpc_url = env::var("RPC_URL").expect("RPC_URL must be set");
            let rpc_client = RpcClient::new(rpc_url);
            let signer = Keypair::new();
            let args = ResolvedMeteoraDlmmAddRemoveWsolLiquidityArgs {
                pair: RAY_WSOL_PAIR,
                wsol_amount: TEST_WSOL_AMOUNT,
                min_wsol_discount_bps: 0,
            };
            let prepared = prepare_transactions_inner(&rpc_client, &signer, &args, false, false)
                .await
                .unwrap();

            let mut svm = LiteSVM::new();
            svm.add_program_from_file(METEORA_DLMM_PROGRAM_ID, program_binary_path("dlmm.so"))
                .unwrap();
            svm.airdrop(&signer.pubkey(), AIRDROP_LAMPORTS).unwrap();

            hydrate_account(&mut svm, &rpc_client, prepared.pair.pair).await;
            hydrate_account(&mut svm, &rpc_client, prepared.pair.token_x_mint).await;
            hydrate_account(&mut svm, &rpc_client, prepared.pair.token_y_mint).await;
            hydrate_account(&mut svm, &rpc_client, prepared.pair.reserve_x).await;
            hydrate_account(&mut svm, &rpc_client, prepared.pair.reserve_y).await;
            create_and_fund_wsol_account(&mut svm, &signer, TEST_WSOL_AMOUNT);
            initialize_position_in_litesvm(&mut svm, &signer, &prepared);
            hydrate_account(&mut svm, &rpc_client, prepared.target_bin_array).await;

            let transactions =
                build_transactions(&prepared, &signer, svm.latest_blockhash(), 42, false).unwrap();
            assert_eq!(transactions.len(), 2);

            svm.send_transaction(transactions[0].clone()).unwrap();
            assert!(
                svm.get_account(&prepared.position).is_some(),
                "expected add-liquidity tx to initialize or reuse the position",
            );

            svm.expire_blockhash();
            let transactions =
                build_transactions(&prepared, &signer, svm.latest_blockhash(), 42, false).unwrap();
            svm.send_transaction(transactions[1].clone()).unwrap();
        });
    }

    fn create_and_fund_wsol_account(svm: &mut LiteSVM, signer: &Keypair, amount: u64) -> Address {
        let user_wsol = get_associated_token_address_with_program_id(
            &signer.pubkey(),
            &WSOL_MINT,
            &spl_token_interface::id(),
        );
        let instructions = vec![
            create_associated_token_account_idempotent(
                &signer.pubkey(),
                &signer.pubkey(),
                &WSOL_MINT,
                &spl_token_interface::id(),
            ),
            system_transfer(&signer.pubkey(), &user_wsol, amount),
            spl_token_interface::instruction::sync_native(&spl_token_interface::id(), &user_wsol)
                .unwrap(),
        ];
        svm.send_transaction(Transaction::new_signed_with_payer(
            &instructions,
            Some(&signer.pubkey()),
            &[signer],
            svm.latest_blockhash(),
        ))
        .unwrap();
        user_wsol
    }

    fn initialize_position_in_litesvm(
        svm: &mut LiteSVM,
        signer: &Keypair,
        prepared: &PreparedMeteoraDlmmAddRemoveWsolLiquidity,
    ) {
        let instruction = build_initialize_position_pda_instruction(prepared, signer.pubkey()).unwrap();
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&signer.pubkey()),
            &[signer],
            svm.latest_blockhash(),
        ))
        .unwrap();
    }

    async fn hydrate_account(svm: &mut LiteSVM, rpc_client: &RpcClient, address: Address) {
        let account = rpc_client.get_account(&address).await.unwrap();
        svm.set_account(address, account).unwrap();
    }

    fn program_binary_path(binary_name: &str) -> PathBuf {
        if let Ok(path) = env::var(format!(
            "{}_SO",
            binary_name.trim_end_matches(".so").to_ascii_uppercase()
        )) {
            return PathBuf::from(path);
        }

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let candidate_paths = [
            manifest_dir.join("tests").join("fixtures").join(binary_name),
            manifest_dir.join("target").join("deploy").join(binary_name),
            manifest_dir
                .parent()
                .map(|parent| parent.join("target").join("deploy").join(binary_name))
                .unwrap_or_else(|| manifest_dir.join("target").join("deploy").join(binary_name)),
        ];

        candidate_paths
            .iter()
            .find(|path| path.exists())
            .cloned()
            .unwrap_or_else(|| missing_program_binary(binary_name, &candidate_paths))
    }

    fn missing_program_binary(binary_name: &str, candidate_paths: &[PathBuf]) -> ! {
        let expected_paths = candidate_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");

        panic!("program binary {binary_name} not found. looked in: {expected_paths}");
    }
}

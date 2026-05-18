use {
    crate::{
        error::BoxError,
        manifest::{
            MarketFixedRaw, MANIFEST_PROGRAM_ID, MANIFEST_WRAPPER_PROGRAM_ID,
            MARKET_FIXED_DISCRIMINANT, SYSTEM_PROGRAM_ID,
        },
        slot_assert::build_assert_slot_instruction,
        transactions::{ResolvedManifestPlaceCancelArgs, ResolvedManifestPlaceCancelOrder},
    },
    bytemuck::{Pod, Zeroable},
    solana_address::Address,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{
        config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionAccountsConfig},
        filter::{Memcmp, RpcFilterType},
        response::UiAccountEncoding,
    },
    solana_signer::Signer,
    solana_system_interface::instruction::create_account,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
    spl_associated_token_account_interface::{
        address::get_associated_token_address_with_program_id,
        instruction::create_associated_token_account_idempotent,
    },
    std::{cmp::Ordering, collections::HashSet, io, mem::size_of},
    tracing::info,
};

const WRAPPER_STATE_DISCRIMINANT: u64 = 1;
const WRAPPER_FIXED_SIZE: usize = 64;
const WRAPPER_BLOCK_HEADER_SIZE: usize = 16;
const WRAPPER_MARKET_INFO_SIZE: usize = 80;
const MARKET_BLOCK_HEADER_SIZE: usize = 16;
const RESTING_ORDER_SIZE: usize = 64;
const ORDER_TYPE_GLOBAL: u8 = 3;
const NIL: u32 = u32::MAX;

#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct WrapperFixedRaw {
    discriminant: u64,
    trader: Address,
    num_bytes_allocated: u32,
    free_list_head_index: u32,
    market_infos_root_index: u32,
    padding: [u32; 3],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct WrapperNodeHeaderRaw {
    left: u32,
    right: u32,
    parent: u32,
    color: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct WrapperMarketInfoRaw {
    market: Address,
    orders_root_index: u32,
    trader_index: u32,
    base_balance: u64,
    quote_balance: u64,
    quote_volume: u64,
    last_updated_slot: u32,
    padding: [u32; 3],
}

#[derive(Clone, Copy, Debug)]
struct MarketInfo {
    market: Address,
    base_mint: Address,
    quote_mint: Address,
    base_mint_decimals: u8,
    quote_mint_decimals: u8,
    base_vault: Address,
    quote_vault: Address,
    base_token_program: Address,
    quote_token_program: Address,
    best_bid: Option<ManifestOrderSummary>,
    best_ask: Option<ManifestOrderSummary>,
    global_bid_count: usize,
    global_ask_count: usize,
    highest_global_bid: Option<ManifestOrderSummary>,
    lowest_global_ask: Option<ManifestOrderSummary>,
}

#[derive(Clone, Copy, Debug)]
struct ManifestOrderSummary {
    price_inner: u128,
    base_atoms: u64,
    order_type: u8,
}

pub(crate) struct PreparedManifestPlaceCancel {
    market_info: MarketInfo,
    wrapper_state: Address,
    user_token_account: Address,
    order: ResolvedManifestPlaceCancelOrder,
    deposit_amount: u64,
    order_base_amount: u64,
    price_mantissa: u32,
    price_exponent: i8,
    deposit_mint: Address,
    deposit_vault: Address,
    deposit_token_program: Address,
}

pub(super) async fn prepare_transactions(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ResolvedManifestPlaceCancelArgs,
) -> Result<PreparedManifestPlaceCancel, BoxError> {
    let market_info = load_market_info(rpc_client, args.market).await?;
    validate_expected_market(&market_info, args.base_mint, args.quote_mint)?;
    let (price_mantissa, price_exponent) = price_to_mantissa_and_exponent(
        args.ui_price_quote_per_base,
        market_info.base_mint_decimals,
        market_info.quote_mint_decimals,
    )?;
    let ask_price_inner = price_inner_from_mantissa_and_exponent(price_mantissa, price_exponent);
    validate_order_does_not_cross(&market_info, args.order, ask_price_inner)?;
    let wrapper_state = find_wrapper_state(rpc_client, signer.pubkey(), &[args.market])
        .await?
        .ok_or_else(|| io::Error::other("manifest wrapper state missing after startup setup"))?;
    let (deposit_amount, order_base_amount, deposit_mint, deposit_vault, deposit_token_program) =
        order_amounts_and_deposit_accounts(&market_info, args.order, ask_price_inner)?;
    let user_token_account = get_associated_token_address_with_program_id(
        &signer.pubkey(),
        &deposit_mint,
        &deposit_token_program,
    );

    info!(
        market = %market_info.market,
        wrapper_state = %wrapper_state,
        base_mint = %market_info.base_mint,
        quote_mint = %market_info.quote_mint,
        order = ?args.order,
        deposit_amount,
        order_base_amount,
        ui_price_quote_per_base = args.ui_price_quote_per_base,
        price_mantissa,
        price_exponent,
        best_bid_price_inner = ?market_info.best_bid.map(|order| order.price_inner),
        best_ask_price_inner = ?market_info.best_ask.map(|order| order.price_inner),
        global_bid_count = market_info.global_bid_count,
        global_ask_count = market_info.global_ask_count,
        highest_global_bid_price_inner = ?market_info.highest_global_bid.map(|order| order.price_inner),
        lowest_global_ask_price_inner = ?market_info.lowest_global_ask.map(|order| order.price_inner),
        "prepared manifest wrapper place/cancel transactions",
    );

    Ok(PreparedManifestPlaceCancel {
        market_info,
        wrapper_state,
        user_token_account,
        order: args.order,
        deposit_amount,
        order_base_amount,
        price_mantissa,
        price_exponent,
        deposit_mint,
        deposit_vault,
        deposit_token_program,
    })
}

pub(super) fn build_transactions(
    prepared: &PreparedManifestPlaceCancel,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Result<Vec<VersionedTransaction>, BoxError> {
    Ok(vec![
        build_place_transaction(
            prepared,
            signer,
            blockhash,
            target_slot,
            include_slot_assert,
        ),
        build_cancel_withdraw_transaction(
            prepared,
            signer,
            blockhash,
            target_slot,
            include_slot_assert,
        ),
    ])
}

pub async fn run_startup_setup(
    rpc_client: &RpcClient,
    signer: &Keypair,
    args: &ResolvedManifestPlaceCancelArgs,
) -> Result<(), BoxError> {
    let market_info = load_market_info(rpc_client, args.market).await?;
    validate_expected_market(&market_info, args.base_mint, args.quote_mint)?;
    let mut setup_instructions = Vec::new();
    let mut setup_signers = Vec::new();
    let mut created_wrapper = false;
    let wrapper_state = match find_wrapper_state(rpc_client, signer.pubkey(), &[args.market]).await? {
        Some(wrapper_state) => wrapper_state,
        None => {
            created_wrapper = true;
            let wrapper_keypair = Keypair::new();
            let wrapper_state = wrapper_keypair.pubkey();
            let lamports = rpc_client
                .get_minimum_balance_for_rent_exemption(WRAPPER_FIXED_SIZE)
                .await?;
            setup_instructions.push(create_account(
                &signer.pubkey(),
                &wrapper_state,
                lamports,
                WRAPPER_FIXED_SIZE as u64,
                &MANIFEST_WRAPPER_PROGRAM_ID,
            ));
            setup_instructions.push(build_create_wrapper_instruction(
                signer.pubkey(),
                wrapper_state,
            ));
            setup_signers.push(wrapper_keypair);
            wrapper_state
        }
    };

    let (deposit_mint, deposit_token_program) = match args.order {
        ResolvedManifestPlaceCancelOrder::Ask { .. } => {
            (market_info.base_mint, market_info.base_token_program)
        }
        ResolvedManifestPlaceCancelOrder::Bid { .. } => {
            (market_info.quote_mint, market_info.quote_token_program)
        }
    };
    let user_token_account = get_associated_token_address_with_program_id(
        &signer.pubkey(),
        &deposit_mint,
        &deposit_token_program,
    );
    if rpc_client.get_account(&user_token_account).await.is_err()
    {
        setup_instructions.push(create_associated_token_account_idempotent(
            &signer.pubkey(),
            &signer.pubkey(),
            &deposit_mint,
            &deposit_token_program,
        ));
    }

    let has_market = if created_wrapper {
        false
    } else {
        let wrapper_account = rpc_client.get_account(&wrapper_state).await?;
        wrapper_has_market(&wrapper_account.data, market_info.market)?
    };
    if !has_market {
        setup_instructions.push(build_claim_seat_instruction(
            signer.pubkey(),
            market_info.market,
            wrapper_state,
        ));
    }

    if setup_instructions.is_empty() {
        info!(
            market = %market_info.market,
            wrapper_state = %wrapper_state,
            "manifest wrapper startup setup found no missing accounts",
        );
        return Ok(());
    }

    let blockhash = rpc_client.get_latest_blockhash().await?;
    let signer_refs = setup_signers.iter().collect::<Vec<_>>();
    let mut all_signers = Vec::with_capacity(1 + signer_refs.len());
    all_signers.push(signer);
    all_signers.extend(signer_refs);
    let transaction = VersionedTransaction::from(Transaction::new_signed_with_payer(
        &setup_instructions,
        Some(&signer.pubkey()),
        &all_signers,
        blockhash,
    ));
    let signature = rpc_client.send_and_confirm_transaction(&transaction).await?;
    info!(
        market = %market_info.market,
        wrapper_state = %wrapper_state,
        signature = %signature,
        setup_instruction_count = setup_instructions.len(),
        "completed manifest wrapper startup setup",
    );

    Ok(())
}

pub fn simulation_account_configs(
    _prepared: &PreparedManifestPlaceCancel,
    transaction_count: usize,
) -> Result<
    (
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
        Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    ),
    BoxError,
> {
    Ok((vec![None; transaction_count], vec![None; transaction_count]))
}

pub fn log_post_simulation(
    prepared: &PreparedManifestPlaceCancel,
    _simulation_result: &solana_rpc_client_api::bundles::RpcSimulateBundleResult,
) -> Result<(), BoxError> {
    info!(
        market = %prepared.market_info.market,
        wrapper_state = %prepared.wrapper_state,
        order = ?prepared.order,
        deposit_amount = prepared.deposit_amount,
        order_base_amount = prepared.order_base_amount,
        "simulated manifest wrapper place/cancel bundle",
    );
    Ok(())
}

fn build_place_transaction(
    prepared: &PreparedManifestPlaceCancel,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> VersionedTransaction {
    let mut instructions = Vec::with_capacity(4);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.push(build_wrapper_deposit_instruction(
        signer.pubkey(),
        prepared,
    ));
    instructions.push(build_wrapper_place_order_instruction(
        signer.pubkey(),
        prepared,
        target_slot,
    ));
    instructions.push(build_uniquifier_memo_instruction(
        signer.pubkey(),
        &slot_uniquifier_memo(target_slot, 1),
    ));
    build_transaction(signer, blockhash, &instructions)
}

fn build_cancel_withdraw_transaction(
    prepared: &PreparedManifestPlaceCancel,
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> VersionedTransaction {
    let mut instructions = Vec::with_capacity(4);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.push(build_wrapper_cancel_all_instruction(
        signer.pubkey(),
        prepared,
    ));
    instructions.push(build_wrapper_withdraw_instruction(
        signer.pubkey(),
        prepared,
    ));
    instructions.push(build_uniquifier_memo_instruction(
        signer.pubkey(),
        &slot_uniquifier_memo(target_slot, 2),
    ));
    build_transaction(signer, blockhash, &instructions)
}

fn build_create_wrapper_instruction(owner: Address, wrapper_state: Address) -> Instruction {
    Instruction {
        program_id: MANIFEST_WRAPPER_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(owner, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new(wrapper_state, true),
        ],
        data: vec![0],
    }
}

fn build_claim_seat_instruction(owner: Address, market: Address, wrapper_state: Address) -> Instruction {
    Instruction {
        program_id: MANIFEST_WRAPPER_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(MANIFEST_PROGRAM_ID, false),
            AccountMeta::new(owner, true),
            AccountMeta::new(market, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new(wrapper_state, false),
        ],
        data: vec![1],
    }
}

fn build_wrapper_deposit_instruction(owner: Address, prepared: &PreparedManifestPlaceCancel) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(2);
    data.extend_from_slice(&prepared.deposit_amount.to_le_bytes());

    Instruction {
        program_id: MANIFEST_WRAPPER_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(MANIFEST_PROGRAM_ID, false),
            AccountMeta::new(owner, true),
            AccountMeta::new(prepared.market_info.market, false),
            AccountMeta::new(prepared.user_token_account, false),
            AccountMeta::new(prepared.deposit_vault, false),
            AccountMeta::new_readonly(prepared.deposit_token_program, false),
            AccountMeta::new(prepared.wrapper_state, false),
            AccountMeta::new_readonly(prepared.deposit_mint, false),
        ],
        data,
    }
}

fn build_wrapper_withdraw_instruction(owner: Address, prepared: &PreparedManifestPlaceCancel) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3);
    data.extend_from_slice(&prepared.deposit_amount.to_le_bytes());

    Instruction {
        program_id: MANIFEST_WRAPPER_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(MANIFEST_PROGRAM_ID, false),
            AccountMeta::new(owner, true),
            AccountMeta::new(prepared.market_info.market, false),
            AccountMeta::new(prepared.user_token_account, false),
            AccountMeta::new(prepared.deposit_vault, false),
            AccountMeta::new_readonly(prepared.deposit_token_program, false),
            AccountMeta::new(prepared.wrapper_state, false),
            AccountMeta::new_readonly(prepared.deposit_mint, false),
        ],
        data,
    }
}

fn build_wrapper_place_order_instruction(
    owner: Address,
    prepared: &PreparedManifestPlaceCancel,
    target_slot: Slot,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 4 + 1 + 4 + 27);
    data.push(4);
    data.extend_from_slice(&0_u32.to_le_bytes());
    data.push(0);
    data.extend_from_slice(&1_u32.to_le_bytes());
    data.extend_from_slice(&target_slot.to_le_bytes());
    data.extend_from_slice(&prepared.order_base_amount.to_le_bytes());
    data.extend_from_slice(&prepared.price_mantissa.to_le_bytes());
    data.push(prepared.price_exponent as u8);
    data.push(u8::from(matches!(
        prepared.order,
        ResolvedManifestPlaceCancelOrder::Bid { .. }
    )));
    data.extend_from_slice(&0_u32.to_le_bytes());
    data.push(2);

    Instruction {
        program_id: MANIFEST_WRAPPER_PROGRAM_ID,
        accounts: build_wrapper_batch_accounts(owner, prepared),
        data,
    }
}

fn build_wrapper_cancel_all_instruction(owner: Address, prepared: &PreparedManifestPlaceCancel) -> Instruction {
    let mut data = Vec::with_capacity(10);
    data.push(4);
    data.extend_from_slice(&0_u32.to_le_bytes());
    data.push(1);
    data.extend_from_slice(&0_u32.to_le_bytes());

    Instruction {
        program_id: MANIFEST_WRAPPER_PROGRAM_ID,
        accounts: build_wrapper_batch_accounts(owner, prepared),
        data,
    }
}

fn build_wrapper_batch_accounts(owner: Address, prepared: &PreparedManifestPlaceCancel) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(prepared.wrapper_state, false),
        AccountMeta::new_readonly(MANIFEST_PROGRAM_ID, false),
        AccountMeta::new(owner, true),
        AccountMeta::new(prepared.market_info.market, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
    ]
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

fn validate_expected_market(
    market_info: &MarketInfo,
    expected_base_mint: Address,
    expected_quote_mint: Address,
) -> Result<(), BoxError> {
    if market_info.base_mint != expected_base_mint || market_info.quote_mint != expected_quote_mint {
        return Err(io::Error::other(format!(
            "manifest-place-cancel expects a market with base={} and quote={}, got base={} quote={}",
            expected_base_mint, expected_quote_mint, market_info.base_mint, market_info.quote_mint,
        ))
        .into());
    }

    Ok(())
}

fn validate_order_does_not_cross(
    market_info: &MarketInfo,
    order: ResolvedManifestPlaceCancelOrder,
    order_price_inner: u128,
) -> Result<(), BoxError> {
    match order {
        ResolvedManifestPlaceCancelOrder::Ask { .. } => {
            let Some(best_bid) = market_info.best_bid else {
                return Ok(());
            };

            if order_price_inner < best_bid.price_inner {
                let crossing_global_bid = market_info
                    .highest_global_bid
                    .filter(|global_bid| order_price_inner < global_bid.price_inner);
                return Err(io::Error::other(format!(
                    "manifest-place-cancel ask would cross current best bid for market {}: ask_price_inner={} best_bid_price_inner={} best_bid_base_atoms={} best_bid_order_type={} global_bid_count={} crossing_global_bid_price_inner={}",
                    market_info.market,
                    order_price_inner,
                    best_bid.price_inner,
                    best_bid.base_atoms,
                    best_bid.order_type,
                    market_info.global_bid_count,
                    crossing_global_bid
                        .map(|order| order.price_inner.to_string())
                        .unwrap_or_else(|| "none".to_string()),
                ))
                .into());
            }
        }
        ResolvedManifestPlaceCancelOrder::Bid { .. } => {
            let Some(best_ask) = market_info.best_ask else {
                return Ok(());
            };

            if order_price_inner > best_ask.price_inner {
                let crossing_global_ask = market_info
                    .lowest_global_ask
                    .filter(|global_ask| order_price_inner > global_ask.price_inner);
                return Err(io::Error::other(format!(
                    "manifest-place-cancel bid would cross current best ask for market {}: bid_price_inner={} best_ask_price_inner={} best_ask_base_atoms={} best_ask_order_type={} global_ask_count={} crossing_global_ask_price_inner={}",
                    market_info.market,
                    order_price_inner,
                    best_ask.price_inner,
                    best_ask.base_atoms,
                    best_ask.order_type,
                    market_info.global_ask_count,
                    crossing_global_ask
                        .map(|order| order.price_inner.to_string())
                        .unwrap_or_else(|| "none".to_string()),
                ))
                .into());
            }
        }
    }

    Ok(())
}

fn order_amounts_and_deposit_accounts(
    market_info: &MarketInfo,
    order: ResolvedManifestPlaceCancelOrder,
    price_inner: u128,
) -> Result<(u64, u64, Address, Address, Address), BoxError> {
    match order {
        ResolvedManifestPlaceCancelOrder::Ask { base_amount } => {
            Ok((
                base_amount,
                base_amount,
                market_info.base_mint,
                market_info.base_vault,
                market_info.base_token_program,
            ))
        }
        ResolvedManifestPlaceCancelOrder::Bid { quote_amount } => {
            let order_base_amount = u128::from(quote_amount)
                .checked_mul(1_000_000_000_000_000_000)
                .ok_or_else(|| io::Error::other("manifest bid amount conversion overflow"))?
                / price_inner;
            let order_base_amount = u64::try_from(order_base_amount).map_err(|_| {
                io::Error::other("manifest bid order base amount does not fit in u64")
            })?;
            if order_base_amount == 0 {
                return Err(io::Error::other(
                    "manifest bid quote amount is too small for the configured price",
                )
                .into());
            }
            Ok((
                quote_amount,
                order_base_amount,
                market_info.quote_mint,
                market_info.quote_vault,
                market_info.quote_token_program,
            ))
        }
    }
}

fn price_to_mantissa_and_exponent(
    quote_tokens_per_base_token: f64,
    base_mint_decimals: u8,
    quote_mint_decimals: u8,
) -> Result<(u32, i8), BoxError> {
    if !quote_tokens_per_base_token.is_finite() || quote_tokens_per_base_token <= 0.0 {
        return Err(io::Error::other("manifest ask price must be finite and positive").into());
    }

    let atom_price = quote_tokens_per_base_token
        * 10_f64.powi(i32::from(quote_mint_decimals) - i32::from(base_mint_decimals));
    if !atom_price.is_finite() || atom_price <= 0.0 {
        return Err(io::Error::other(format!(
            "failed to convert manifest ask price {} to quote atoms per base atom",
            quote_tokens_per_base_token,
        ))
        .into());
    }

    for exponent in -18_i8..=8 {
        let scale = 10_f64.powi(i32::from(exponent));
        let mantissa = (atom_price / scale).round();
        if mantissa >= 1.0 && mantissa <= f64::from(u32::MAX) {
            return Ok((mantissa as u32, exponent));
        }
    }

    Err(io::Error::other(format!(
        "manifest ask price {} cannot be represented as QuoteAtomsPerBaseAtom",
        quote_tokens_per_base_token,
    ))
    .into())
}

fn price_inner_from_mantissa_and_exponent(mantissa: u32, exponent: i8) -> u128 {
    let scale_exponent = u32::try_from(i16::from(exponent) + 18)
        .expect("manifest price exponent is in the supported range");
    u128::from(mantissa) * 10_u128.pow(scale_exponent)
}

async fn load_market_info(rpc_client: &RpcClient, market: Address) -> Result<MarketInfo, BoxError> {
    let account = rpc_client.get_account(&market).await?;
    if account.owner != MANIFEST_PROGRAM_ID {
        return Err(io::Error::other(format!(
            "manifest market {} is owned by {}, expected {}",
            market, account.owner, MANIFEST_PROGRAM_ID
        ))
        .into());
    }
    if account.data.len() < size_of::<MarketFixedRaw>() {
        return Err(io::Error::other(format!(
            "manifest market {} data too short: {}",
            market,
            account.data.len()
        ))
        .into());
    }

    let raw = bytemuck::pod_read_unaligned::<MarketFixedRaw>(
        &account.data[..size_of::<MarketFixedRaw>()],
    );
    let discriminant = raw.discriminant;
    if discriminant != MARKET_FIXED_DISCRIMINANT {
        return Err(io::Error::other(format!(
            "manifest market {} has invalid discriminant {}",
            market, discriminant
        ))
        .into());
    }

    let base_mint_account = rpc_client.get_account(&raw.base_mint).await?;
    let quote_mint_account = rpc_client.get_account(&raw.quote_mint).await?;
    let best_bid = read_market_order(&account.data, raw.bids_best_index)?;
    let best_ask = read_market_order(&account.data, raw.asks_best_index)?;
    let bid_orders = collect_market_orders(&account.data, raw.bids_root_index)?;
    let ask_orders = collect_market_orders(&account.data, raw.asks_root_index)?;
    let global_bid_count = bid_orders
        .iter()
        .filter(|order| order.order_type == ORDER_TYPE_GLOBAL)
        .count();
    let global_ask_count = ask_orders
        .iter()
        .filter(|order| order.order_type == ORDER_TYPE_GLOBAL)
        .count();
    let highest_global_bid = bid_orders
        .iter()
        .filter(|order| order.order_type == ORDER_TYPE_GLOBAL)
        .max_by_key(|order| order.price_inner)
        .copied();
    let lowest_global_ask = ask_orders
        .iter()
        .filter(|order| order.order_type == ORDER_TYPE_GLOBAL)
        .min_by_key(|order| order.price_inner)
        .copied();
    Ok(MarketInfo {
        market,
        base_mint: raw.base_mint,
        quote_mint: raw.quote_mint,
        base_mint_decimals: raw.base_mint_decimals,
        quote_mint_decimals: raw.quote_mint_decimals,
        base_vault: raw.base_vault,
        quote_vault: raw.quote_vault,
        base_token_program: base_mint_account.owner,
        quote_token_program: quote_mint_account.owner,
        best_bid,
        best_ask,
        global_bid_count,
        global_ask_count,
        highest_global_bid,
        lowest_global_ask,
    })
}

fn collect_market_orders(
    market_account_data: &[u8],
    root_index: u32,
) -> Result<Vec<ManifestOrderSummary>, BoxError> {
    let mut orders = Vec::new();
    let mut stack = Vec::new();
    let mut seen = HashSet::new();
    if root_index != NIL {
        stack.push(root_index);
    }

    while let Some(index) = stack.pop() {
        if index == NIL || !seen.insert(index) {
            continue;
        }

        let node_offset = size_of::<MarketFixedRaw>() + index as usize;
        let header = market_account_data
            .get(node_offset..node_offset + MARKET_BLOCK_HEADER_SIZE)
            .ok_or_else(|| io::Error::other("manifest market order node index out of bounds"))?;
        let left = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let right = u32::from_le_bytes(header[4..8].try_into().unwrap());
        stack.push(left);
        stack.push(right);

        if let Some(order) = read_market_order(market_account_data, index)? {
            orders.push(order);
        }
    }

    Ok(orders)
}

fn read_market_order(
    market_account_data: &[u8],
    index: u32,
) -> Result<Option<ManifestOrderSummary>, BoxError> {
    if index == NIL {
        return Ok(None);
    }

    let order_offset = size_of::<MarketFixedRaw>() + index as usize + MARKET_BLOCK_HEADER_SIZE;
    let order = market_account_data
        .get(order_offset..order_offset + RESTING_ORDER_SIZE)
        .ok_or_else(|| io::Error::other("manifest market resting order index out of bounds"))?;
    let price_inner = u128::from_le_bytes(order[0..16].try_into().unwrap());
    let base_atoms = u64::from_le_bytes(order[16..24].try_into().unwrap());
    let order_type = order[41];

    Ok(Some(ManifestOrderSummary {
        price_inner,
        base_atoms,
        order_type,
    }))
}

async fn find_wrapper_state(
    rpc_client: &RpcClient,
    trader: Address,
    markets: &[Address],
) -> Result<Option<Address>, BoxError> {
    #[allow(deprecated)]
    let accounts = rpc_client
        .get_program_accounts_with_config(
            &MANIFEST_WRAPPER_PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                        0,
                        WRAPPER_STATE_DISCRIMINANT.to_le_bytes().to_vec(),
                    )),
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(8, trader.as_ref().to_vec())),
                ]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    ..RpcAccountInfoConfig::default()
                },
                ..RpcProgramAccountsConfig::default()
            },
        )
        .await?;

    let mut fallback = None;
    for (wrapper_state, account) in accounts {
        if fallback.is_none() {
            fallback = Some(wrapper_state);
        }
        for market in markets {
            if wrapper_has_market(&account.data, *market)? {
                return Ok(Some(wrapper_state));
            }
        }
    }

    Ok(fallback)
}

fn wrapper_has_market(data: &[u8], market: Address) -> Result<bool, BoxError> {
    let fixed = parse_wrapper_fixed(data)?;
    Ok(find_market_info(data, fixed.market_infos_root_index, market)?.is_some())
}

fn parse_wrapper_fixed(data: &[u8]) -> Result<WrapperFixedRaw, BoxError> {
    if data.len() < WRAPPER_FIXED_SIZE {
        return Err(io::Error::other(format!(
            "manifest wrapper data too short: {}",
            data.len()
        ))
        .into());
    }
    let fixed = bytemuck::pod_read_unaligned::<WrapperFixedRaw>(&data[..WRAPPER_FIXED_SIZE]);
    let discriminant = fixed.discriminant;
    if discriminant != WRAPPER_STATE_DISCRIMINANT {
        return Err(io::Error::other(format!(
            "manifest wrapper has invalid discriminant {}",
            discriminant
        ))
        .into());
    }
    Ok(fixed)
}

fn find_market_info(
    data: &[u8],
    root_index: u32,
    market: Address,
) -> Result<Option<WrapperMarketInfoRaw>, BoxError> {
    let dynamic = &data[WRAPPER_FIXED_SIZE..];
    let mut index = root_index;
    while index != NIL {
        let node = read_node_header(dynamic, index)?;
        let market_info = read_market_info(dynamic, index)?;
        match market.as_ref().cmp(market_info.market.as_ref()) {
            Ordering::Equal => return Ok(Some(market_info)),
            Ordering::Less => index = node.left,
            Ordering::Greater => index = node.right,
        }
    }
    Ok(None)
}

fn read_node_header(dynamic: &[u8], index: u32) -> Result<WrapperNodeHeaderRaw, BoxError> {
    let offset = index as usize;
    let bytes = dynamic
        .get(offset..offset + WRAPPER_BLOCK_HEADER_SIZE)
        .ok_or_else(|| io::Error::other("wrapper node header index out of bounds"))?;
    Ok(bytemuck::pod_read_unaligned::<WrapperNodeHeaderRaw>(bytes))
}

fn read_market_info(dynamic: &[u8], index: u32) -> Result<WrapperMarketInfoRaw, BoxError> {
    let offset = index as usize + WRAPPER_BLOCK_HEADER_SIZE;
    let bytes = dynamic
        .get(offset..offset + WRAPPER_MARKET_INFO_SIZE)
        .ok_or_else(|| io::Error::other("wrapper market info index out of bounds"))?;
    Ok(bytemuck::pod_read_unaligned::<WrapperMarketInfoRaw>(bytes))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::manifest::build_create_market_instruction,
        litesvm::LiteSVM,
        solana_program_pack::Pack,
        spl_token_interface::{
            instruction::{initialize_mint, mint_to},
            state::{Account as TokenAccount, Mint},
        },
        std::path::PathBuf,
    };

    const AIRDROP_LAMPORTS: u64 = 10_000_000_000;

    #[test]
    fn wrapper_place_then_cancel_withdraw_succeeds() {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(MANIFEST_PROGRAM_ID, program_binary_path("manifest.so"))
            .unwrap();
        svm.add_program_from_file(
            MANIFEST_WRAPPER_PROGRAM_ID,
            program_binary_path("wrapper.so"),
        )
        .unwrap();

        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), AIRDROP_LAMPORTS).unwrap();

        let base_mint = create_mint(&mut svm, &payer, 6);
        let quote_mint = create_mint(&mut svm, &payer, 6);
        let user_base_token_account = create_ata(&mut svm, &payer, base_mint);
        mint_tokens(
            &mut svm,
            &payer,
            base_mint,
            user_base_token_account,
            1_000_000,
        );

        let market = create_market(&mut svm, &payer, base_mint, quote_mint);
        let wrapper_state = create_wrapper_and_claim_seat(&mut svm, &payer, market);
        let market_account = svm.get_account(&market).unwrap();
        let market_raw = bytemuck::pod_read_unaligned::<MarketFixedRaw>(
            &market_account.data[..size_of::<MarketFixedRaw>()],
        );
        let prepared = PreparedManifestPlaceCancel {
            market_info: MarketInfo {
                market,
                base_mint,
                quote_mint,
                base_mint_decimals: 6,
                quote_mint_decimals: 6,
                base_vault: market_raw.base_vault,
                quote_vault: market_raw.quote_vault,
                base_token_program: spl_token_interface::id(),
                quote_token_program: spl_token_interface::id(),
                best_bid: None,
                best_ask: None,
                global_bid_count: 0,
                global_ask_count: 0,
                highest_global_bid: None,
                lowest_global_ask: None,
            },
            wrapper_state,
            user_token_account: user_base_token_account,
            order: ResolvedManifestPlaceCancelOrder::Ask {
                base_amount: 100_000,
            },
            deposit_amount: 100_000,
            order_base_amount: 100_000,
            price_mantissa: 1_000_000,
            price_exponent: 6,
            deposit_mint: base_mint,
            deposit_vault: market_raw.base_vault,
            deposit_token_program: spl_token_interface::id(),
        };

        let starting_amount = token_amount(&svm, user_base_token_account);
        let transactions = build_transactions(
            &prepared,
            &payer,
            svm.latest_blockhash(),
            42,
            false,
        )
        .unwrap();
        assert_eq!(transactions.len(), 2);

        svm.send_transaction(transactions[0].clone())
            .unwrap();
        assert!(
            wrapper_has_market_with_orders(&svm, wrapper_state, market).unwrap(),
            "expected wrapper to track an open order after place transaction",
        );

        svm.expire_blockhash();
        let cancel_transactions = build_transactions(
            &prepared,
            &payer,
            svm.latest_blockhash(),
            42,
            false,
        )
        .unwrap();
        svm.send_transaction(cancel_transactions[1].clone())
            .unwrap();

        assert!(
            !wrapper_has_market_with_orders(&svm, wrapper_state, market).unwrap(),
            "expected wrapper cancel-all to clear the open order",
        );
        assert_eq!(token_amount(&svm, user_base_token_account), starting_amount);
    }

    fn create_mint(svm: &mut LiteSVM, payer: &Keypair, decimals: u8) -> Address {
        let mint = Keypair::new();
        let lamports = svm.minimum_balance_for_rent_exemption(Mint::LEN);
        let instructions = vec![
            create_account(
                &payer.pubkey(),
                &mint.pubkey(),
                lamports,
                Mint::LEN as u64,
                &spl_token_interface::id(),
            ),
            initialize_mint(
                &spl_token_interface::id(),
                &mint.pubkey(),
                &payer.pubkey(),
                None,
                decimals,
            )
            .unwrap(),
        ];
        svm.send_transaction(Transaction::new_signed_with_payer(
            &instructions,
            Some(&payer.pubkey()),
            &[payer, &mint],
            svm.latest_blockhash(),
        ))
        .unwrap();
        mint.pubkey()
    }

    fn create_ata(svm: &mut LiteSVM, payer: &Keypair, mint: Address) -> Address {
        let ata = get_associated_token_address_with_program_id(
            &payer.pubkey(),
            &mint,
            &spl_token_interface::id(),
        );
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[create_associated_token_account_idempotent(
                &payer.pubkey(),
                &payer.pubkey(),
                &mint,
                &spl_token_interface::id(),
            )],
            Some(&payer.pubkey()),
            &[payer],
            svm.latest_blockhash(),
        ))
        .unwrap();
        ata
    }

    fn mint_tokens(
        svm: &mut LiteSVM,
        payer: &Keypair,
        mint: Address,
        token_account: Address,
        amount: u64,
    ) {
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[mint_to(
                &spl_token_interface::id(),
                &mint,
                &token_account,
                &payer.pubkey(),
                &[],
                amount,
            )
            .unwrap()],
            Some(&payer.pubkey()),
            &[payer],
            svm.latest_blockhash(),
        ))
        .unwrap();
    }

    fn create_market(
        svm: &mut LiteSVM,
        payer: &Keypair,
        base_mint: Address,
        quote_mint: Address,
    ) -> Address {
        let market = Keypair::new();
        let lamports = svm.minimum_balance_for_rent_exemption(size_of::<MarketFixedRaw>());
        let instructions = vec![
            create_account(
                &payer.pubkey(),
                &market.pubkey(),
                lamports,
                size_of::<MarketFixedRaw>() as u64,
                &MANIFEST_PROGRAM_ID,
            ),
            build_create_market_instruction(payer.pubkey(), market.pubkey(), base_mint, quote_mint),
        ];
        svm.send_transaction(Transaction::new_signed_with_payer(
            &instructions,
            Some(&payer.pubkey()),
            &[payer, &market],
            svm.latest_blockhash(),
        ))
        .unwrap();
        market.pubkey()
    }

    fn create_wrapper_and_claim_seat(
        svm: &mut LiteSVM,
        payer: &Keypair,
        market: Address,
    ) -> Address {
        let wrapper_state = Keypair::new();
        let lamports = svm.minimum_balance_for_rent_exemption(WRAPPER_FIXED_SIZE);
        let instructions = vec![
            create_account(
                &payer.pubkey(),
                &wrapper_state.pubkey(),
                lamports,
                WRAPPER_FIXED_SIZE as u64,
                &MANIFEST_WRAPPER_PROGRAM_ID,
            ),
            build_create_wrapper_instruction(payer.pubkey(), wrapper_state.pubkey()),
            build_claim_seat_instruction(payer.pubkey(), market, wrapper_state.pubkey()),
        ];
        svm.send_transaction(Transaction::new_signed_with_payer(
            &instructions,
            Some(&payer.pubkey()),
            &[payer, &wrapper_state],
            svm.latest_blockhash(),
        ))
        .unwrap();
        wrapper_state.pubkey()
    }

    fn token_amount(svm: &LiteSVM, token_account: Address) -> u64 {
        let account = svm.get_account(&token_account).unwrap();
        TokenAccount::unpack(&account.data).unwrap().amount
    }

    fn wrapper_has_market_with_orders(
        svm: &LiteSVM,
        wrapper_state: Address,
        market: Address,
    ) -> Result<bool, BoxError> {
        let account = svm.get_account(&wrapper_state).unwrap();
        let fixed = parse_wrapper_fixed(&account.data)?;
        let market_info = find_market_info(&account.data, fixed.market_infos_root_index, market)?
            .ok_or_else(|| io::Error::other("wrapper missing market info"))?;
        Ok(market_info.orders_root_index != NIL)
    }

    fn program_binary_path(binary_name: &str) -> PathBuf {
        if let Ok(path) = std::env::var(format!(
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
            PathBuf::from("/private/tmp/bonasa-manifest")
                .join("target")
                .join("deploy")
                .join(binary_name),
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

        panic!(
            "Manifest program binary {binary_name} not found. Set MANIFEST_SO/WRAPPER_SO or build Bonasa Manifest SBF artifacts. looked in: {}",
            expected_paths,
        );
    }
}

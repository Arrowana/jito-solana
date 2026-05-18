use {
    bytemuck::{Pod, Zeroable},
    solana_address::{address, Address},
    solana_instruction::{AccountMeta, Instruction},
};

pub const MANIFEST_PROGRAM_ID: Address = address!("MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms");
pub const MANIFEST_WRAPPER_PROGRAM_ID: Address =
    address!("wMNFSTkir3HgyZTsB7uqu3i7FA73grFCptPXgrZjksL");
pub const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");
pub const MARKET_FIXED_DISCRIMINANT: u64 = 4_859_840_929_024_028_656;

#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct MarketFixedRaw {
    pub discriminant: u64,
    pub version: u8,
    pub base_mint_decimals: u8,
    pub quote_mint_decimals: u8,
    pub base_vault_bump: u8,
    pub quote_vault_bump: u8,
    pub padding1: [u8; 3],
    pub base_mint: Address,
    pub quote_mint: Address,
    pub base_vault: Address,
    pub quote_vault: Address,
    pub order_sequence_number: u64,
    pub num_bytes_allocated: u32,
    pub bids_root_index: u32,
    pub bids_best_index: u32,
    pub asks_root_index: u32,
    pub asks_best_index: u32,
    pub claimed_seats_root_index: u32,
    pub free_list_head_index: u32,
    pub padding2: [u32; 1],
    pub quote_volume: u64,
    pub padding3: [u64; 8],
}

pub fn build_create_market_instruction(
    payer: Address,
    market: Address,
    base_mint: Address,
    quote_mint: Address,
) -> Instruction {
    let (base_vault, _) = Address::find_program_address(
        &[b"vault", market.as_ref(), base_mint.as_ref()],
        &MANIFEST_PROGRAM_ID,
    );
    let (quote_vault, _) = Address::find_program_address(
        &[b"vault", market.as_ref(), quote_mint.as_ref()],
        &MANIFEST_PROGRAM_ID,
    );

    Instruction {
        program_id: MANIFEST_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(market, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(base_mint, false),
            AccountMeta::new_readonly(quote_mint, false),
            AccountMeta::new(base_vault, false),
            AccountMeta::new(quote_vault, false),
            AccountMeta::new_readonly(spl_token_interface::id(), false),
            AccountMeta::new_readonly(spl_token_2022_interface::id(), false),
        ],
        data: vec![0],
    }
}

use solana_address::{address, Address};

pub const RAYDIUM_CP_SWAP_PROGRAM_ID: Address =
    address!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const RAYDIUM_CREATE_POOL_FEE_RECEIVER: Address =
    address!("DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8");
pub const RAYDIUM_AUTH_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";
pub const RAYDIUM_POOL_SEED: &[u8] = b"pool";
pub const RAYDIUM_POOL_VAULT_SEED: &[u8] = b"pool_vault";
pub const RAYDIUM_POOL_LP_MINT_SEED: &[u8] = b"pool_lp_mint";
pub const RAYDIUM_OBSERVATION_SEED: &[u8] = b"observation";
pub const AMM_CONFIG_DISCRIMINATOR: [u8; 8] = [218, 244, 33, 104, 203, 203, 43, 111];
pub const POOL_STATE_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];
pub const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] =
    [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
pub const SWAP_BASE_OUTPUT_DISCRIMINATOR: [u8; 8] =
    [0x37, 0xd9, 0x62, 0x56, 0xa3, 0x4a, 0xb4, 0xad];
pub const INITIALIZE_DISCRIMINATOR: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
pub const DEPOSIT_DISCRIMINATOR: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];

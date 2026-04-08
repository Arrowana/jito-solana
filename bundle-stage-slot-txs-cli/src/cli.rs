use {
    clap::{Args, Parser, Subcommand, ValueEnum},
    solana_address::Address,
    std::{io, path::PathBuf},
};

#[derive(Clone, Debug, Parser)]
#[command(name = "bundle-stage-slot-txs-cli")]
pub struct Config {
    #[arg(long)]
    pub rpc_url: String,

    #[arg(long)]
    pub identity: Option<Address>,

    #[arg(long)]
    pub keypair: Option<String>,

    #[arg(long, env = "JUP_API_KEY")]
    pub jup_api_key: Option<String>,

    #[arg(long, default_value_t = 10)]
    pub gap_duration_millis: u64,

    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4))]
    pub consecutive_slots: u8,

    #[command(subcommand)]
    pub transaction_mode: Option<TransactionMode>,
}

impl Config {
    pub fn selected_transaction_mode(&self) -> TransactionMode {
        self.transaction_mode
            .clone()
            .unwrap_or(TransactionMode::Memo)
    }

    pub fn validate(&self) -> Result<(), io::Error> {
        match self.selected_transaction_mode() {
            TransactionMode::Memo => {
                self.require_identity()?;
                self.require_keypair()?;
                Ok(())
            }
            TransactionMode::RaydiumCpSwap(args) => {
                self.require_identity()?;
                self.require_keypair()?;
                let expected_pool_count = usize::from(self.consecutive_slots);
                if args.pools.len() != expected_pool_count {
                    return Err(io::Error::other(format!(
                        "raydium-cp-swap requires exactly {} --pool values for consecutive_slots={}, got {}",
                        expected_pool_count,
                        self.consecutive_slots,
                        args.pools.len(),
                    )));
                }

                Ok(())
            }
            TransactionMode::ScanRaydiumCpSwap(args) => {
                if args.simulate_top > 0 {
                    self.require_keypair().map(|_| ())?;
                }
                Ok(())
            }
            TransactionMode::ProvisionRaydiumCpSwapPool(args) => {
                self.require_keypair()?;
                args.validate()
            }
        }
    }

    pub fn require_identity(&self) -> Result<Address, io::Error> {
        self.identity.ok_or_else(|| {
            io::Error::other("--identity is required for memo and raydium-cp-swap modes")
        })
    }

    pub fn require_keypair(&self) -> Result<&str, io::Error> {
        self.keypair.as_deref().ok_or_else(|| {
            io::Error::other("--keypair is required for memo and raydium-cp-swap modes")
        })
    }

    pub fn jup_api_key(&self) -> Option<String> {
        self.jup_api_key
            .clone()
            .or_else(|| std::env::var("JUPITER_API_KEY").ok())
    }
}

#[derive(Clone, Debug, Subcommand)]
pub enum TransactionMode {
    Memo,
    RaydiumCpSwap(RaydiumCpSwapArgs),
    ScanRaydiumCpSwap(ScanRaydiumCpSwapArgs),
    ProvisionRaydiumCpSwapPool(ProvisionRaydiumCpSwapPoolArgs),
}

impl TransactionMode {
    pub fn label(&self) -> &'static str {
        match self {
            TransactionMode::Memo => "memo",
            TransactionMode::RaydiumCpSwap(_) => "raydium-cp-swap",
            TransactionMode::ScanRaydiumCpSwap(_) => "scan-raydium-cp-swap",
            TransactionMode::ProvisionRaydiumCpSwapPool(_) => "provision-raydium-cp-swap-pool",
        }
    }
}

#[derive(Clone, Debug, ValueEnum)]
pub enum TokenAllowlistSource {
    JupiterVerified,
    JupiterVerifiedCsv,
    DefillamaTop200,
    Union,
}

impl TokenAllowlistSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::JupiterVerified => "jupiter-verified",
            Self::JupiterVerifiedCsv => "jupiter-verified-csv",
            Self::DefillamaTop200 => "defillama-top200",
            Self::Union => "union",
        }
    }
}

#[derive(Args, Clone, Debug)]
pub struct RaydiumCpSwapArgs {
    #[arg(long = "pool", required = true, num_args = 1..=4)]
    pub pools: Vec<Address>,

    #[arg(long)]
    pub input_mint: Address,

    #[arg(long)]
    pub input_amount: u64,
}

#[derive(Args, Clone, Debug)]
pub struct ScanRaydiumCpSwapArgs {
    #[arg(long)]
    pub input_mint: Address,

    #[arg(long)]
    pub input_amount: u64,

    #[arg(long = "trade-fee-rate", num_args = 1.., default_values_t = [2_500_u64, 3_000_u64])]
    pub trade_fee_rates: Vec<u64>,

    #[arg(long, value_enum, default_value_t = TokenAllowlistSource::JupiterVerified)]
    pub token_allowlist_source: TokenAllowlistSource,

    #[arg(long, env = "JUPITER_API_KEY")]
    pub jupiter_api_key: Option<String>,

    #[arg(long, default_value_t = 0.0)]
    pub min_estimated_tvl_usdc: f64,

    #[arg(long, default_value_t = 100_000.0)]
    pub min_token_volume_24h_usd: f64,

    #[arg(long, default_value_t = 20)]
    pub top: usize,

    #[arg(long, default_value_t = 0)]
    pub simulate_top: usize,

    #[arg(long, default_value = "raydium-cp-swap-shortlist.csv")]
    pub output_csv: PathBuf,
}

#[derive(Args, Clone, Debug)]
pub struct ProvisionRaydiumCpSwapPoolArgs {
    #[arg(long)]
    pub token_a_mint: Address,

    #[arg(long)]
    pub token_b_mint: Address,

    #[arg(long)]
    pub amm_config: Option<Address>,

    #[arg(long)]
    pub token_a_usd_price: Option<f64>,

    #[arg(long)]
    pub token_b_usd_price: Option<f64>,

    #[arg(long, default_value_t = 100.0)]
    pub total_tvl_usdc: f64,

    #[arg(long, default_value_t = 10.0)]
    pub chunk_tvl_usdc: f64,

    #[arg(long, default_value_t = 5.0)]
    pub max_existing_tvl_usdc: f64,

    #[arg(long, default_value_t = 500)]
    pub max_price_deviation_bps: u64,

    #[arg(long, default_value_t = 500)]
    pub max_jupiter_price_impact_bps: u64,

    #[arg(long, default_value_t = 100)]
    pub jupiter_slippage_bps: u64,
}

impl ProvisionRaydiumCpSwapPoolArgs {
    fn validate(&self) -> Result<(), io::Error> {
        if self.token_a_mint == self.token_b_mint {
            return Err(io::Error::other("token_a_mint and token_b_mint must differ"));
        }
        if !(self.total_tvl_usdc.is_finite() && self.total_tvl_usdc > 0.0) {
            return Err(io::Error::other("--total-tvl-usdc must be a positive finite number"));
        }
        if !(self.chunk_tvl_usdc.is_finite() && self.chunk_tvl_usdc > 0.0) {
            return Err(io::Error::other("--chunk-tvl-usdc must be a positive finite number"));
        }
        if !(self.max_existing_tvl_usdc.is_finite() && self.max_existing_tvl_usdc >= 0.0) {
            return Err(io::Error::other(
                "--max-existing-tvl-usdc must be a non-negative finite number",
            ));
        }
        if self.jupiter_slippage_bps == 0 {
            return Err(io::Error::other("--jupiter-slippage-bps must be greater than 0"));
        }
        for (label, maybe_price) in [
            ("token-a-usd-price", self.token_a_usd_price),
            ("token-b-usd-price", self.token_b_usd_price),
        ] {
            if let Some(price) = maybe_price {
                if !(price.is_finite() && price > 0.0) {
                    return Err(io::Error::other(format!(
                        "--{} must be a positive finite number when provided",
                        label
                    )));
                }
            }
        }
        Ok(())
    }
}

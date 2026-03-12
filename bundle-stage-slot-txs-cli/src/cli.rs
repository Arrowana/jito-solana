use {
    clap::{Args, Parser, Subcommand},
    solana_address::Address,
    std::io,
};

#[derive(Clone, Debug, Parser)]
#[command(name = "bundle-stage-slot-txs-cli")]
pub struct Config {
    #[arg(long)]
    pub rpc_url: String,

    #[arg(long)]
    pub identity: Address,

    #[arg(long)]
    pub keypair: String,

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
            TransactionMode::Memo => Ok(()),
            TransactionMode::RaydiumCpSwap(args) => {
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
        }
    }
}

#[derive(Clone, Debug, Subcommand)]
pub enum TransactionMode {
    Memo,
    RaydiumCpSwap(RaydiumCpSwapArgs),
}

impl TransactionMode {
    pub fn label(&self) -> &'static str {
        match self {
            TransactionMode::Memo => "memo",
            TransactionMode::RaydiumCpSwap(_) => "raydium-cp-swap",
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

use {
    clap::{Args, Parser, Subcommand},
    solana_address::Address,
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

    #[command(subcommand)]
    pub transaction_mode: Option<TransactionMode>,
}

impl Config {
    pub fn selected_transaction_mode(&self) -> TransactionMode {
        self.transaction_mode
            .clone()
            .unwrap_or(TransactionMode::Memo)
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
    #[arg(long)]
    pub pool: Address,

    #[arg(long)]
    pub input_mint: Address,

    #[arg(long)]
    pub input_amount: u64,
}

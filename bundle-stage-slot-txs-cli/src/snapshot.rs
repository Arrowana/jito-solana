use {
    crate::error::BoxError,
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        fs::{self, File},
        io::{self, Write},
        path::Path,
    },
};

pub const BAIT_AND_DISAPPEAR_TXS_PATH: &str = "/tmp/bundle_stage_slot_txs.bin";
const BAIT_AND_DISAPPEAR_TXS_TEMP_PATH: &str = "/tmp/bundle_stage_slot_txs.bin.tmp";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaitAndDisappearFile {
    pub gap_duration_millis: u64,
    pub slot_transactions: Vec<BaitAndDisappearSlotTransactions>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaitAndDisappearSlotTransactions {
    pub slot: Slot,
    pub transactions: Vec<VersionedTransaction>,
}

pub fn clear_snapshot_file(gap_duration_millis: u64) -> Result<(), BoxError> {
    write_snapshot_file(&BaitAndDisappearFile {
        gap_duration_millis,
        slot_transactions: Vec::new(),
    })
}

pub fn write_target_slots_snapshot(
    gap_duration_millis: u64,
    slot_transactions: Vec<BaitAndDisappearSlotTransactions>,
) -> Result<(), BoxError> {
    write_snapshot_file(&BaitAndDisappearFile {
        gap_duration_millis,
        slot_transactions,
    })
}

fn write_snapshot_file(snapshot: &BaitAndDisappearFile) -> Result<(), BoxError> {
    let bytes = bincode::serialize(snapshot)?;
    if let Some(parent) = Path::new(BAIT_AND_DISAPPEAR_TXS_TEMP_PATH).parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = File::create(BAIT_AND_DISAPPEAR_TXS_TEMP_PATH)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(
        BAIT_AND_DISAPPEAR_TXS_TEMP_PATH,
        BAIT_AND_DISAPPEAR_TXS_PATH,
    )
    .map_err(|err| -> BoxError {
        Box::new(io::Error::new(
            err.kind(),
            format!(
                "failed to replace snapshot {} with {}: {err}",
                BAIT_AND_DISAPPEAR_TXS_PATH, BAIT_AND_DISAPPEAR_TXS_TEMP_PATH,
            ),
        ))
    })?;

    Ok(())
}

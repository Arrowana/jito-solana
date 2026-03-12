use {
    arc_swap::ArcSwap,
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        collections::{hash_map::Entry, HashMap},
        fs,
        io::{self, ErrorKind},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, SystemTime},
    },
};

pub const BAIT_AND_DISAPPEAR_TXS_PATH: &str = "/tmp/bundle_stage_slot_txs.bin";
const BAIT_AND_DISAPPEAR_TXS_POLL_INTERVAL: Duration = Duration::from_secs(1);

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

#[derive(Clone, Debug)]
pub struct BaitAndDisappearSnapshot {
    gap_duration: Duration,
    slot_transactions: HashMap<Slot, Vec<VersionedTransaction>>,
}

impl BaitAndDisappearSnapshot {
    pub fn gap_duration(&self) -> Duration {
        self.gap_duration
    }

    pub fn transactions_for_slot(&self, slot: Slot) -> Option<&Vec<VersionedTransaction>> {
        self.slot_transactions.get(&slot)
    }
}

pub type SharedBaitAndDisappearSnapshot = Arc<ArcSwap<Option<BaitAndDisappearSnapshot>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotFileMetadata {
    len: u64,
    modified: Option<SystemTime>,
}

pub fn new_shared_bait_and_disappear_snapshot() -> SharedBaitAndDisappearSnapshot {
    Arc::new(ArcSwap::from_pointee(None))
}

pub fn spawn_bait_and_disappear_snapshot_reader(
    exit: Arc<AtomicBool>,
    snapshot: SharedBaitAndDisappearSnapshot,
) -> JoinHandle<()> {
    Builder::new()
        .name("solBundleSlotTx".to_string())
        .spawn(move || {
            let mut last_metadata = None;
            while !exit.load(Ordering::Relaxed) {
                match read_snapshot_file_metadata() {
                    Ok(metadata) => {
                        if last_metadata.as_ref() != Some(&metadata) {
                            last_metadata = Some(metadata);
                            match load_bait_and_disappear_snapshot() {
                                Ok(Some(next_snapshot)) => {
                                    let slot_count = next_snapshot.slot_transactions.len();
                                    let gap_duration_millis =
                                        next_snapshot.gap_duration().as_millis() as u64;
                                    snapshot.store(Arc::new(Some(next_snapshot)));
                                    info!(
                                        "bait and disappear snapshot loaded: path={} slots={} gap_duration_millis={}",
                                        BAIT_AND_DISAPPEAR_TXS_PATH,
                                        slot_count,
                                        gap_duration_millis,
                                    );
                                }
                                Ok(None) => {
                                    snapshot.store(Arc::new(None));
                                    info!(
                                        "bait and disappear snapshot cleared: path={}",
                                        BAIT_AND_DISAPPEAR_TXS_PATH,
                                    );
                                }
                                Err(err) => {
                                    snapshot.store(Arc::new(None));
                                    warn!(
                                        "failed to load bait and disappear snapshot: path={} err={err}",
                                        BAIT_AND_DISAPPEAR_TXS_PATH,
                                    );
                                }
                            }
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::NotFound => {
                        if last_metadata.take().is_some() {
                            snapshot.store(Arc::new(None));
                            info!(
                                "bait and disappear snapshot removed: path={}",
                                BAIT_AND_DISAPPEAR_TXS_PATH,
                            );
                        }
                    }
                    Err(err) => {
                        if last_metadata.take().is_some() {
                            snapshot.store(Arc::new(None));
                        }
                        warn!(
                            "failed to stat bait and disappear snapshot: path={} err={err}",
                            BAIT_AND_DISAPPEAR_TXS_PATH,
                        );
                    }
                }

                thread::sleep(BAIT_AND_DISAPPEAR_TXS_POLL_INTERVAL);
            }
        })
        .unwrap()
}

fn read_snapshot_file_metadata() -> io::Result<SnapshotFileMetadata> {
    let metadata = fs::metadata(Path::new(BAIT_AND_DISAPPEAR_TXS_PATH))?;
    Ok(SnapshotFileMetadata {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn load_bait_and_disappear_snapshot() -> io::Result<Option<BaitAndDisappearSnapshot>> {
    let bytes = fs::read(Path::new(BAIT_AND_DISAPPEAR_TXS_PATH))?;
    let snapshot_file = bincode::deserialize::<BaitAndDisappearFile>(&bytes).map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("failed to deserialize snapshot: {err}"),
        )
    })?;

    if snapshot_file.slot_transactions.is_empty() {
        return Ok(None);
    }

    let mut slot_transactions = HashMap::with_capacity(snapshot_file.slot_transactions.len());
    for entry in snapshot_file.slot_transactions {
        match slot_transactions.entry(entry.slot) {
            Entry::Occupied(_) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("duplicate slot entry {}", entry.slot),
                ));
            }
            Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(entry.transactions);
            }
        }
    }

    Ok(Some(BaitAndDisappearSnapshot {
        gap_duration: Duration::from_millis(snapshot_file.gap_duration_millis),
        slot_transactions,
    }))
}

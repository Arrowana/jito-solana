use {
    crate::error::BoxError,
    solana_address::Address,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcLeaderScheduleConfig,
    std::time::{Duration, Instant},
};

pub const ARMING_WINDOW_SLOTS: Slot = 20;
pub const POST_TARGET_GRACE_SLOTS: Slot = 10;
pub const COUNTDOWN_LOG_INTERVAL_SLOTS: Slot = 100;
const LEADER_SCHEDULE_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum PublishedState {
    Cleared,
    Armed { start_slot: Slot, last_slot: Slot },
}

#[derive(Default)]
pub struct LeaderScheduleCache {
    cached_schedule: Option<CachedLeaderSchedule>,
}

struct CachedLeaderSchedule {
    refreshed_at: Instant,
    slots: Vec<Slot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSlots {
    pub slots: Vec<Slot>,
}

impl TargetSlots {
    pub fn start_slot(&self) -> Slot {
        self.slots[0]
    }

    pub fn last_slot(&self) -> Slot {
        *self.slots.last().unwrap_or(&self.start_slot())
    }
}

pub async fn current_slot(rpc_client: &RpcClient) -> Result<Slot, BoxError> {
    Ok(rpc_client
        .get_slot_with_commitment(CommitmentConfig::processed())
        .await?)
}

impl LeaderScheduleCache {
    pub async fn target_slots(
        &mut self,
        rpc_client: &RpcClient,
        identity: &Address,
        current_slot: Slot,
        consecutive_slots: usize,
    ) -> Result<Option<TargetSlots>, BoxError> {
        if self.should_refresh(current_slot) {
            self.cached_schedule = Some(CachedLeaderSchedule {
                refreshed_at: Instant::now(),
                slots: fetch_leader_slots(rpc_client, identity).await?,
            });
        }

        Ok(self
            .cached_schedule
            .as_ref()
            .and_then(|cached_schedule| {
                next_rotation_slots_in_schedule(
                    &cached_schedule.slots,
                    current_slot,
                    consecutive_slots,
                )
            }))
    }

    fn should_refresh(&self, current_slot: Slot) -> bool {
        match self.cached_schedule.as_ref() {
            Some(cached_schedule) => {
                cached_schedule.refreshed_at.elapsed() >= LEADER_SCHEDULE_REFRESH_INTERVAL
                    || cached_schedule
                        .slots
                        .last()
                        .map(|last_slot| current_slot > *last_slot)
                        .unwrap_or(true)
            }
            None => true,
        }
    }
}

async fn fetch_leader_slots(
    rpc_client: &RpcClient,
    identity: &Address,
) -> Result<Vec<Slot>, BoxError> {
    let epoch_info = rpc_client
        .get_epoch_info_with_commitment(CommitmentConfig::processed())
        .await?;
    let epoch_schedule = rpc_client.get_epoch_schedule().await?;
    let identity_string = identity.to_string();
    let mut all_slots = Vec::new();

    for epoch in [epoch_info.epoch, epoch_info.epoch.saturating_add(1)] {
        let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);
        let Some(leader_schedule) = rpc_client.get_leader_schedule_with_config(
            Some(first_slot_in_epoch),
            RpcLeaderScheduleConfig {
                identity: Some(identity_string.clone()),
                commitment: Some(CommitmentConfig::processed()),
                ..RpcLeaderScheduleConfig::default()
            },
        )
        .await?
        else {
            continue;
        };

        let Some(leader_slots) = leader_schedule.get(&identity_string) else {
            continue;
        };
        all_slots.extend(
            leader_slots
                .iter()
                .map(|slot_index| first_slot_in_epoch.saturating_add(*slot_index as u64)),
        );
    }

    all_slots.sort_unstable();
    Ok(all_slots)
}

pub fn published_state_for_target(target_slots: &TargetSlots, current_slot: Slot) -> PublishedState {
    let remaining_slots = remaining_slots_to_target(target_slots.start_slot(), current_slot);
    if remaining_slots > 0 && remaining_slots <= ARMING_WINDOW_SLOTS {
        PublishedState::Armed {
            start_slot: target_slots.start_slot(),
            last_slot: target_slots.last_slot(),
        }
    } else {
        PublishedState::Cleared
    }
}

pub fn published_state_is_within_grace_window(
    published_state: &PublishedState,
    current_slot: Slot,
) -> bool {
    match published_state {
        PublishedState::Armed { last_slot, .. } => {
            current_slot <= last_slot.saturating_add(POST_TARGET_GRACE_SLOTS)
        }
        PublishedState::Cleared => false,
    }
}

pub fn remaining_slots_to_target(target_slot: Slot, current_slot: Slot) -> Slot {
    target_slot.saturating_sub(current_slot)
}

pub fn estimated_time_to_target(target_slot: Slot, current_slot: Slot) -> Duration {
    Duration::from_millis(
        remaining_slots_to_target(target_slot, current_slot).saturating_mul(DEFAULT_MS_PER_SLOT),
    )
}

pub fn countdown_log_bucket(target_slot: Slot, current_slot: Slot) -> Option<Slot> {
    let remaining_slots = remaining_slots_to_target(target_slot, current_slot);
    if remaining_slots == 0 {
        None
    } else {
        Some(remaining_slots / COUNTDOWN_LOG_INTERVAL_SLOTS)
    }
}

pub fn format_eta(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h{minutes}m{seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn next_rotation_slots_in_schedule(
    leader_slots: &[Slot],
    current_slot: Slot,
    consecutive_slots: usize,
) -> Option<TargetSlots> {
    let consecutive_slots = consecutive_slots.max(1);
    let mut run_start_index = 0;

    while run_start_index < leader_slots.len() {
        let run_start_slot = leader_slots[run_start_index];
        let mut run_end_index = run_start_index + 1;

        while run_end_index < leader_slots.len()
            && leader_slots[run_end_index - 1].saturating_add(1) == leader_slots[run_end_index]
        {
            run_end_index += 1;
        }

        if run_start_slot > current_slot {
            let slot_count = consecutive_slots.min(run_end_index.saturating_sub(run_start_index));
            return Some(TargetSlots {
                slots: leader_slots[run_start_index..run_start_index + slot_count].to_vec(),
            });
        }

        run_start_index = run_end_index;
    }

    None
}

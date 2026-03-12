use {
    solana_address::Address,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcLeaderScheduleConfig,
    std::{
        error::Error,
        time::{Duration, Instant},
    },
};

pub const ARMING_WINDOW_SLOTS: Slot = 20;
pub const POST_TARGET_GRACE_SLOTS: Slot = 10;
pub const COUNTDOWN_LOG_INTERVAL_SLOTS: Slot = 100;
const LEADER_SCHEDULE_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum PublishedState {
    Cleared,
    Armed(Slot),
}

#[derive(Default)]
pub struct LeaderScheduleCache {
    cached_schedule: Option<CachedLeaderSchedule>,
}

struct CachedLeaderSchedule {
    refreshed_at: Instant,
    slots: Vec<Slot>,
}

pub fn current_slot(rpc_client: &RpcClient) -> Result<Slot, Box<dyn Error>> {
    Ok(rpc_client.get_slot_with_commitment(CommitmentConfig::processed())?)
}

impl LeaderScheduleCache {
    pub fn target_slot(
        &mut self,
        rpc_client: &RpcClient,
        identity: &Address,
        current_slot: Slot,
    ) -> Result<Option<Slot>, Box<dyn Error>> {
        if self.should_refresh(current_slot) {
            self.cached_schedule = Some(CachedLeaderSchedule {
                refreshed_at: Instant::now(),
                slots: fetch_leader_slots(rpc_client, identity)?,
            });
        }

        Ok(self
            .cached_schedule
            .as_ref()
            .and_then(|cached_schedule| {
                next_rotation_start_in_slots(&cached_schedule.slots, current_slot)
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

fn fetch_leader_slots(
    rpc_client: &RpcClient,
    identity: &Address,
) -> Result<Vec<Slot>, Box<dyn Error>> {
    let epoch_info = rpc_client.get_epoch_info_with_commitment(CommitmentConfig::processed())?;
    let epoch_schedule = rpc_client.get_epoch_schedule()?;
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
        )?
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

pub fn published_state_for_target(target_slot: Slot, current_slot: Slot) -> PublishedState {
    let remaining_slots = remaining_slots_to_target(target_slot, current_slot);
    if remaining_slots > 0 && remaining_slots <= ARMING_WINDOW_SLOTS {
        PublishedState::Armed(target_slot)
    } else {
        PublishedState::Cleared
    }
}

pub fn published_state_is_within_grace_window(
    published_state: &PublishedState,
    current_slot: Slot,
) -> bool {
    match published_state {
        PublishedState::Armed(slot) => {
            current_slot <= slot.saturating_add(POST_TARGET_GRACE_SLOTS)
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

fn next_rotation_start_in_slots(leader_slots: &[Slot], current_slot: Slot) -> Option<Slot> {
    let mut previous_slot: Option<Slot> = None;
    let mut current_run_start: Option<Slot> = None;

    for slot in leader_slots {
        if previous_slot
            .map(|previous_slot| previous_slot.saturating_add(1) != *slot)
            .unwrap_or(true)
        {
            current_run_start = Some(*slot);
        }

        let run_start = current_run_start?;
        if run_start > current_slot {
            return Some(run_start);
        }

        previous_slot = Some(*slot);
    }

    None
}

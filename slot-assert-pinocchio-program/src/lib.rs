#![no_std]

use pinocchio::{
    account::AccountView,
    address::Address,
    error::ProgramError,
    no_allocator,
    nostd_panic_handler,
    program_entrypoint,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

const ASSERT_SLOT_DISCRIMINATOR: u8 = 0;
const SLOT_MISMATCH_ERROR: u32 = 1;

program_entrypoint!(process_instruction);
no_allocator!();
nostd_panic_handler!();

pub fn process_instruction(
    _program_id: &Address,
    _accounts: &[AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    match instruction_data.split_first() {
        Some((&ASSERT_SLOT_DISCRIMINATOR, data)) => assert_slot(data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn assert_slot(data: &[u8]) -> ProgramResult {
    let expected_slot = parse_expected_slot(data)?;
    let clock = Clock::get()?;

    if clock.slot != expected_slot {
        return Err(ProgramError::Custom(SLOT_MISMATCH_ERROR));
    }

    Ok(())
}

fn parse_expected_slot(data: &[u8]) -> Result<u64, ProgramError> {
    let slot_bytes: [u8; 8] = data
        .try_into()
        .map_err(|_| ProgramError::InvalidInstructionData)?;
    Ok(u64::from_le_bytes(slot_bytes))
}

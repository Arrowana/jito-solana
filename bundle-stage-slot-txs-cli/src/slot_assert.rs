use {solana_address::Address, solana_clock::Slot, solana_instruction::Instruction};

pub const SLOT_ASSERT_PROGRAM_ID: Address =
    Address::from_str_const("S1otXSd4rsN4fReyRwj9CUCTqBGqnPPZREKe6SMMDvW");

const ASSERT_SLOT_DISCRIMINATOR: u8 = 0;

pub fn build_assert_slot_instruction(target_slot: Slot) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(ASSERT_SLOT_DISCRIMINATOR);
    data.extend_from_slice(&target_slot.to_le_bytes());

    Instruction {
        program_id: SLOT_ASSERT_PROGRAM_ID,
        accounts: Vec::new(),
        data,
    }
}

use {
    solana_address::Address,
    solana_clock::Slot,
    solana_instruction::Instruction,
};

pub const SLOT_ASSERT_PROGRAM_ID: Address = Address::new_from_array([
    70, 206, 213, 194, 44, 193, 100, 16, 154, 231, 110, 41, 87, 144, 193, 251, 184, 85, 19,
    236, 190, 144, 25, 8, 183, 122, 208, 86, 91, 181, 243, 80,
]);

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

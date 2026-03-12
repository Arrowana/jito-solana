use {
    crate::slot_assert::build_assert_slot_instruction,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
};

pub fn build_transactions(
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> Vec<VersionedTransaction> {
    vec![
        build_memo_transaction(
            signer,
            "Bob",
            &uniquifier_memo(target_slot),
            blockhash,
            target_slot,
            include_slot_assert,
        ),
        build_memo_transaction(
            signer,
            "Alice",
            &uniquifier_memo(target_slot),
            blockhash,
            target_slot,
            include_slot_assert,
        ),
    ]
}

fn build_memo_transaction(
    signer: &Keypair,
    memo: &str,
    uniquifier: &str,
    blockhash: Hash,
    target_slot: Slot,
    include_slot_assert: bool,
) -> VersionedTransaction {
    let mut instructions = Vec::<Instruction>::with_capacity(3);
    if include_slot_assert {
        instructions.push(build_assert_slot_instruction(target_slot));
    }
    instructions.push(spl_memo_interface::instruction::build_memo(
        &spl_memo_interface::v3::id(),
        memo.as_bytes(),
        &[&signer.pubkey()],
    ));
    instructions.push(spl_memo_interface::instruction::build_memo(
        &spl_memo_interface::v3::id(),
        uniquifier.as_bytes(),
        &[&signer.pubkey()],
    ));
    VersionedTransaction::from(Transaction::new_signed_with_payer(
        &instructions,
        Some(&signer.pubkey()),
        &[signer],
        blockhash,
    ))
}

fn uniquifier_memo(target_slot: Slot) -> String {
    format!("slot:{target_slot}")
}

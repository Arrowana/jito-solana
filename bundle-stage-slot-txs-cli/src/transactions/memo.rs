use {
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
};

pub fn build_transactions(
    signer: &Keypair,
    blockhash: Hash,
    target_slot: Slot,
) -> Vec<VersionedTransaction> {
    vec![
        build_memo_transaction(signer, "Bob", &uniquifier_memo(target_slot, 1), blockhash),
        build_memo_transaction(signer, "Alice", &uniquifier_memo(target_slot, 2), blockhash),
    ]
}

fn build_memo_transaction(
    signer: &Keypair,
    memo: &str,
    uniquifier: &str,
    blockhash: Hash,
) -> VersionedTransaction {
    let instructions = [
        spl_memo_interface::instruction::build_memo(
            &spl_memo_interface::v3::id(),
            memo.as_bytes(),
            &[&signer.pubkey()],
        ),
        spl_memo_interface::instruction::build_memo(
            &spl_memo_interface::v3::id(),
            uniquifier.as_bytes(),
            &[&signer.pubkey()],
        ),
    ];
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

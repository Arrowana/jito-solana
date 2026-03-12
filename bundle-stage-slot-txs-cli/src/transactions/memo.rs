use {
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
};

pub fn build_transactions(signer: &Keypair, blockhash: Hash) -> Vec<VersionedTransaction> {
    vec![
        build_memo_transaction(signer, "Bob", blockhash),
        build_memo_transaction(signer, "Alice", blockhash),
    ]
}

fn build_memo_transaction(signer: &Keypair, memo: &str, blockhash: Hash) -> VersionedTransaction {
    let memo_instruction = spl_memo_interface::instruction::build_memo(
        &spl_memo_interface::v3::id(),
        memo.as_bytes(),
        &[&signer.pubkey()],
    );
    VersionedTransaction::from(Transaction::new_signed_with_payer(
        &[memo_instruction],
        Some(&signer.pubkey()),
        &[signer],
        blockhash,
    ))
}

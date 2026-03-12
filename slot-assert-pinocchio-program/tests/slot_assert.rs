use {
    litesvm::LiteSVM,
    solana_address::Address,
    solana_clock::Slot,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::Transaction,
    std::path::{Path, PathBuf},
};

const ASSERT_SLOT_DISCRIMINATOR: u8 = 0;
const PROGRAM_BINARY_NAME: &str = "slot_assert_pinocchio_program.so";
const AIRDROP_LAMPORTS: u64 = 1_000_000_000;
const SLOT_ASSERT_PROGRAM_ID: Address = Address::new_from_array([
    70, 206, 213, 194, 44, 193, 100, 16, 154, 231, 110, 41, 87, 144, 193, 251, 184, 85, 19,
    236, 190, 144, 25, 8, 183, 122, 208, 86, 91, 181, 243, 80,
]);

#[test]
fn assert_slot_succeeds_on_matching_slot() {
    let expected_slot = 42;
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(SLOT_ASSERT_PROGRAM_ID, program_binary_path())
        .unwrap();

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), AIRDROP_LAMPORTS).unwrap();
    svm.warp_to_slot(expected_slot);

    let result = svm.send_transaction(build_assert_slot_transaction(&payer, &svm, expected_slot));
    assert!(result.is_ok(), "expected matching-slot transaction to succeed: {result:?}");
}

#[test]
fn assert_slot_fails_on_mismatched_slot() {
    let expected_slot = 42;
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(SLOT_ASSERT_PROGRAM_ID, program_binary_path())
        .unwrap();

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), AIRDROP_LAMPORTS).unwrap();
    svm.warp_to_slot(expected_slot + 1);

    let result = svm.send_transaction(build_assert_slot_transaction(&payer, &svm, expected_slot));
    assert!(result.is_err(), "expected mismatched-slot transaction to fail");
}

fn build_assert_slot_transaction(
    payer: &Keypair,
    svm: &LiteSVM,
    expected_slot: Slot,
) -> Transaction {
    Transaction::new_signed_with_payer(
        &[Instruction {
            program_id: SLOT_ASSERT_PROGRAM_ID,
            accounts: vec![],
            data: build_assert_slot_instruction_data(expected_slot),
        }],
        Some(&payer.pubkey()),
        &[payer],
        svm.latest_blockhash(),
    )
}

fn build_assert_slot_instruction_data(expected_slot: Slot) -> Vec<u8> {
    let mut data = Vec::with_capacity(9);
    data.push(ASSERT_SLOT_DISCRIMINATOR);
    data.extend_from_slice(&expected_slot.to_le_bytes());
    data
}

fn program_binary_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate_paths = [
        manifest_dir.join("target").join("deploy").join(PROGRAM_BINARY_NAME),
        manifest_dir
            .parent()
            .map(|parent| parent.join("target").join("deploy").join(PROGRAM_BINARY_NAME))
            .unwrap_or_else(|| manifest_dir.join("target").join("deploy").join(PROGRAM_BINARY_NAME)),
    ];

    candidate_paths
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| missing_program_binary(&candidate_paths))
}

fn missing_program_binary(candidate_paths: &[PathBuf]) -> ! {
    let expected_paths = candidate_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");

    panic!(
        "slot-assert program binary not found. Build it first with `cargo build-sbf --manifest-path {}`. looked in: {}",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml").display(),
        expected_paths,
    );
}

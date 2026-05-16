use {
    crate::{
        error::BoxError,
        manifest::{
            build_create_market_instruction, MarketFixedRaw, MANIFEST_PROGRAM_ID, USDC_MINT,
            WSOL_MINT,
        },
    },
    solana_address::Address,
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_signer::Signer,
    solana_system_interface::instruction::create_account,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
    std::mem::size_of,
    tracing::info,
};

pub async fn create_manifest_sol_usdc_market(
    rpc_client: &RpcClient,
    payer: &Keypair,
) -> Result<Address, BoxError> {
    let market = Keypair::new();
    let market_address = market.pubkey();
    let lamports = rpc_client
        .get_minimum_balance_for_rent_exemption(size_of::<MarketFixedRaw>())
        .await?;
    let blockhash = rpc_client.get_latest_blockhash().await?;
    let instructions = vec![
        create_account(
            &payer.pubkey(),
            &market_address,
            lamports,
            size_of::<MarketFixedRaw>() as u64,
            &MANIFEST_PROGRAM_ID,
        ),
        build_create_market_instruction(payer.pubkey(), market_address, WSOL_MINT, USDC_MINT),
    ];
    let transaction = VersionedTransaction::from(Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer, &market],
        blockhash,
    ));
    let signature = rpc_client.send_and_confirm_transaction(&transaction).await?;

    info!(
        market = %market_address,
        signature = %signature,
        base_mint = %WSOL_MINT,
        quote_mint = %USDC_MINT,
        "created manifest SOL/USDC market",
    );
    println!("{market_address}");
    Ok(market_address)
}

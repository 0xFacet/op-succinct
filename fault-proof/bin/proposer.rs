use std::{env, sync::Arc};

use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_transport_http::reqwest::Url;
use anyhow::Result;
use clap::Parser;
use fault_proof::{
    contract::{DisputeGameFactory, OPSuccinctFaultDisputeGame},
    FactoryTrait, L2ProviderTrait,
    prometheus::ProposerGauge, proposer::OPSuccinctProposer,
    utils::setup_logging,
};
use op_alloy_network::EthereumWallet;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    hosts::default::SingleChainOPSuccinctHost,
    metrics::{init_metrics, MetricsGauge},
};
use rustls::crypto::CryptoProvider;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = ".env.proposer")]
    env_file: String,
    
    #[arg(long, default_value = "false")]
    debug_games: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize rustls crypto provider before anything else
    let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    setup_logging();

    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    let wallet = EthereumWallet::from(
        env::var("PRIVATE_KEY")
            .expect("PRIVATE_KEY must be set")
            .parse::<PrivateKeySigner>()
            .unwrap(),
    );

    let l1_provider_with_wallet = ProviderBuilder::new()
        .wallet(wallet.clone())
        .on_http(env::var("L1_RPC").unwrap().parse::<Url>().unwrap());

    let factory = DisputeGameFactory::new(
        env::var("FACTORY_ADDRESS")
            .expect("FACTORY_ADDRESS must be set")
            .parse::<Address>()
            .unwrap(),
        l1_provider_with_wallet.clone(),
    );

    // Debug games if requested
    if args.debug_games {
        println!("Debugging games...");
        let l2_provider = ProviderBuilder::default()
            .on_http(env::var("L2_RPC").unwrap().parse::<Url>().unwrap());
        
        // Get the latest game index
        let latest_game_index = factory.fetch_latest_game_index().await?.unwrap_or(U256::ZERO);
        println!("Latest game index: {}", latest_game_index);
        
        // Loop through recent games and print detailed status
        for i in 0..5.min(latest_game_index.to::<u64>() + 1) {
            let game_index = latest_game_index - U256::from(i);
            let game_address = factory.fetch_game_address_by_index(game_index).await?;
            let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider_with_wallet.clone());
            
            let claim_data = game.claimData().call().await?.claimData_;
            let status = claim_data.status;
            let block_number = game.l2BlockNumber().call().await?.l2BlockNumber_;
            let game_claim = game.rootClaim().call().await?.rootClaim_;
            
            // Try to compute output root (may fail if L2 node can't provide state for that block)
            let output_root_result = l2_provider.compute_output_root_at_block(block_number).await;
            
            println!("\nGame {}: Address={}", game_index, game_address);
            println!("  Status: {:?}", status);
            println!("  Block number: {}", block_number);
            println!("  Game claim root: {}", game_claim);
            
            if let Ok(output_root) = output_root_result {
                println!("  Computed root: {}", output_root);
                println!("  Claims match: {}", output_root == game_claim);
            } else {
                println!("  Error computing output root: {}", output_root_result.unwrap_err());
            }
            
            // Check deadline status
            let current_timestamp = l2_provider
                .get_l2_block_by_number(alloy_eips::BlockNumberOrTag::Latest)
                .await?
                .header
                .timestamp;
                
            let deadline = U256::from(claim_data.deadline).to::<u64>();
            println!("  Deadline: {} (timestamp)", deadline);
            println!("  Current timestamp: {}", current_timestamp);
            println!("  Deadline passed: {}", deadline < current_timestamp);
        }
        
        println!("\nDebug complete. Exiting without running proposer.");
        return Ok(());
    }

    // Use PROVER_ADDRESS from env if available, otherwise use wallet's default signer address from the private key.
    let prover_address = env::var("PROVER_ADDRESS")
        .ok()
        .and_then(|addr| addr.parse::<Address>().ok())
        .unwrap_or_else(|| wallet.default_signer().address());

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let proposer = OPSuccinctProposer::new(
        prover_address,
        l1_provider_with_wallet,
        factory,
        Arc::new(SingleChainOPSuccinctHost {
            fetcher: Arc::new(fetcher),
        }),
    )
    .await
    .unwrap();

    // Initialize proposer gauges.
    ProposerGauge::register_all();

    // Initialize metrics exporter.
    init_metrics(&proposer.config.metrics_port);

    // Initialize the metrics gauges.
    ProposerGauge::init_all();

    proposer.run().await.expect("Runs in an infinite loop");

    Ok(())
}

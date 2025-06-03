use std::{env, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use jito_json_rpc_client::jsonrpc_client::rpc_client::RpcClient as JitoRpcClient;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::{Transaction, VersionedTransaction},
};
use std::str::FromStr;
use tokio::time::{sleep, Instant};

use crate::{
    common::utils::log_message,
    services::jito::{
        get_tip_account, get_tip_value, init_tip_accounts, wait_for_bundle_confirmation,
    },
};

// Configuration constants
const DEFAULT_UNIT_PRICE: u64 = 1_000; // Increased default for better priority
const DEFAULT_UNIT_LIMIT: u32 = 300_000;
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;
const CONFIRMATION_TIMEOUT_SECS: u64 = 60;

/// Configuration for transaction processing
#[derive(Debug, Clone)]
pub struct TxConfig {
    pub unit_price: u64,
    pub unit_limit: u32,
    pub max_retries: u32,
    pub use_jito: bool,
}

impl Default for TxConfig {
    fn default() -> Self {
        Self {
            unit_price: get_unit_price(),
            unit_limit: get_unit_limit(),
            max_retries: MAX_RETRIES,
            use_jito: true,
        }
    }
}

/// Get prioritization fee unit price from environment or default
fn get_unit_price() -> u64 {
    env::var("UNIT_PRICE")
        .ok()
        .and_then(|v| u64::from_str(&v).ok())
        .unwrap_or(DEFAULT_UNIT_PRICE)
}

/// Get compute unit limit from environment or default
fn get_unit_limit() -> u32 {
    env::var("UNIT_LIMIT")
        .ok()
        .and_then(|v| u32::from_str(&v).ok())
        .unwrap_or(DEFAULT_UNIT_LIMIT)
}

/// Calculate total prioritization fee
fn calculate_priority_fee(unit_price: u64, unit_limit: u32) -> u64 {
    unit_price.saturating_mul(unit_limit as u64)
}

/// Add compute budget instructions for transaction prioritization
fn add_compute_budget_instructions(
    instructions: &mut Vec<Instruction>,
    config: &TxConfig,
) -> Result<()> {
    // Set compute unit limit
    let compute_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(config.unit_limit);
    instructions.insert(0, compute_limit_ix);

    // Set compute unit price for prioritization
    if config.unit_price > 0 {
        let compute_price_ix = ComputeBudgetInstruction::set_compute_unit_price(config.unit_price);
        instructions.insert(1, compute_price_ix);
    }

    Ok(())
}

/// Confirm transaction using Jito bundle service
pub async fn jito_confirm(
    keypair: &Keypair,
    versioned_tx: VersionedTransaction,
    recent_block_hash: &Hash,
    jito_client: Arc<JitoRpcClient>,
) -> Result<String> {
    log_message("Starting Jito bundle confirmation");

    // Initialize tip accounts and get tip details concurrently
    let (tip_account, tip_value) = tokio::try_join!(
        async {
            init_tip_accounts().await?;
            get_tip_account().context("Failed to get tip account")
        },
        async { Ok(get_tip_value()) }
    )?;

    // Pre-allocate bundle vector with known capacity
    let mut bundle_txs = Vec::with_capacity(2);
    bundle_txs.push(versioned_tx);

    // Create and add tip transaction
    let tip_instruction = solana_sdk::system_instruction::transfer(
        &keypair.pubkey(),
        &tip_account,
        tip_value,
    );

    let tip_tx = Transaction::new_signed_with_payer(
        &[tip_instruction],
        Some(&keypair.pubkey()),
        &[keypair],
        *recent_block_hash,
    );
    
    bundle_txs.push(VersionedTransaction::from(tip_tx));

    // Send bundle and wait for confirmation concurrently
    let bundle_id = jito_client
        .send_bundle(&bundle_txs)
        .await
        .context("Failed to send bundle to Jito")?;

    log_message(&format!("Bundle sent with ID: {}", bundle_id));

    // Use a more efficient confirmation with early return
    tokio::time::timeout(
        Duration::from_secs(CONFIRMATION_TIMEOUT_SECS),
        wait_for_bundle_confirmation(&bundle_id, jito_client),
    )
    .await
    .context("Bundle confirmation timeout")?
    .context("Bundle confirmation failed")?;
    
    log_message("Bundle confirmed successfully");
    Ok(bundle_id)
}

/// Create, sign, and send transaction with retry logic
pub async fn new_signed_and_send(
    client: &RpcClient,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    jito_client: Option<Arc<JitoRpcClient>>,
    config: Option<TxConfig>,
    timestamp: Instant,
) -> Result<Vec<String>> {
    let config = config.unwrap_or_default();
    let mut results = Vec::new();
    
    log_message(&format!(
        "Processing transaction with {} instructions (Priority fee: {} lamports)",
        instructions.len(),
        calculate_priority_fee(config.unit_price, config.unit_limit)
    ));

    // Add compute budget instructions for prioritization
    add_compute_budget_instructions(&mut instructions, &config)?;

    // Get recent blockhash
    let recent_blockhash = client
        .get_latest_blockhash()
        .context("Failed to get recent blockhash")?;

    // Create and sign transaction
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &[keypair],
        recent_blockhash,
    );

    let versioned_tx = VersionedTransaction::from(transaction);

    // Try Jito first if available and enabled
    if config.use_jito && jito_client.is_some() {
        match jito_confirm(
            keypair,
            versioned_tx.clone(),
            &recent_blockhash,
            jito_client.unwrap(),
        )
        .await
        {
            Ok(bundle_id) => {
                results.push(bundle_id);
                log_message("Transaction sent successfully via Jito");
                return Ok(results);
            }
            Err(e) => {
                log_message(&format!("Jito submission failed: {}, falling back to RPC", e));
            }
        }
    }

    // Fallback to regular RPC with retry logic
    let mut last_error = None;
    for attempt in 1..=config.max_retries {
        match send_transaction_with_confirmation(client, &versioned_tx).await {
            Ok(signature) => {
                results.push(signature.to_string());
                log_message(&format!(
                    "Transaction sent successfully via RPC on attempt {} (took: {:?})",
                    attempt,
                    timestamp.elapsed()
                ));
                return Ok(results);
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < config.max_retries {
                    log_message(&format!(
                        "Transaction attempt {} failed, retrying in {}ms",
                        attempt, RETRY_DELAY_MS
                    ));
                    sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All transaction attempts failed")))
}

/// Send transaction and wait for confirmation
async fn send_transaction_with_confirmation(
    client: &RpcClient,
    versioned_tx: &VersionedTransaction,
) -> Result<Signature> {
    // Send transaction
    let signature = client
        .send_transaction(versioned_tx)
        .context("Failed to send transaction")?;

    // Wait for confirmation
    let confirmation = client
        .confirm_transaction_with_spinner(
            &signature,
            &recent_blockhash,
            CommitmentConfig::confirmed(),
        )
        .context("Failed to confirm transaction")?;

    if confirmation {
        Ok(signature)
    } else {
        Err(anyhow::anyhow!("Transaction confirmation failed"))
    }
}

/// Batch process multiple transactions
pub async fn batch_send_transactions(
    client: &RpcClient,
    keypair: &Keypair,
    instruction_batches: Vec<Vec<Instruction>>,
    jito_client: Option<Arc<JitoRpcClient>>,
    config: Option<TxConfig>,
) -> Result<Vec<String>> {
    let mut all_results = Vec::new();
    let timestamp = Instant::now();

    for (i, instructions) in instruction_batches.into_iter().enumerate() {
        log_message(&format!("Processing batch {} of transactions", i + 1));
        
        match new_signed_and_send(
            client,
            keypair,
            instructions,
            jito_client.clone(),
            config.clone(),
            timestamp,
        )
        .await
        {
            Ok(mut results) => all_results.append(&mut results),
            Err(e) => {
                log_message(&format!("Batch {} failed: {}", i + 1, e));
                return Err(e);
            }
        }

        // Small delay between batches to avoid rate limiting
        sleep(Duration::from_millis(100)).await;
    }

    Ok(all_results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_priority_fee() {
        assert_eq!(calculate_priority_fee(1000, 300_000), 300_000_000);
        assert_eq!(calculate_priority_fee(0, 300_000), 0);
    }

    #[test]
    fn test_tx_config_default() {
        let config = TxConfig::default();
        assert!(config.unit_price > 0);
        assert!(config.unit_limit > 0);
        assert!(config.max_retries > 0);
        assert!(config.use_jito);
    }
}

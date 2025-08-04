use std::{str::FromStr, sync::Arc};

use crate::{
    core::{
        token::{self, get_account_info},
        tx,
    },
    engine::swap::{SwapDirection, SwapInType},
};
use anyhow::{anyhow, Context, Result};
use borsh::from_slice;
use borsh_derive::{BorshDeserialize, BorshSerialize};
use jito_json_rpc_client::jsonrpc_client::rpc_client::RpcClient as JitoRpcClient;
use raydium_amm::math::U128;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program,
    commitment_config::CommitmentConfig,
    account::Account,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account_idempotent,
};
use tokio::time::Instant;
pub const TEN_THOUSAND: u64 = 10000;
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const RENT_PROGRAM: &str = "SysvarRent111111111111111111111111111111111";
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const PUMP_GLOBAL: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
pub const PUMP_FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
pub const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
// pub const PUMP_FUN_MINT_AUTHORITY: &str = "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM";
pub const PUMP_ACCOUNT: &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
pub const PUMP_BUY_METHOD: u64 = 16927863322537952870;
pub const PUMP_SELL_METHOD: u64 = 12502976635542562355;
// Additional constants
pub const MIN_SOL_BALANCE: u64 = 5000000; // 0.005 SOL minimum
pub const MAX_SLIPPAGE_BPS: u64 = 5000; // 50% max slippage
pub const DEFAULT_SLIPPAGE_BPS: u64 = 100; // 1% default slippage

pub struct Pump {
    pub rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pub keypair: Arc<Keypair>,
    pub rpc_client: Option<Arc<solana_client::rpc_client::RpcClient>>,
}

impl Pump {
    /// Creates a new Pump instance with the provided RPC clients and keypair
    pub fn new(
        rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
        rpc_client: Arc<solana_client::rpc_client::RpcClient>,
        keypair: Arc<Keypair>,
    ) -> Self {
        Self {
            rpc_nonblocking_client,
            keypair,
            rpc_client: Some(rpc_client),
        }
    }

    /// Creates a new Pump instance with only non-blocking client
    pub fn new_nonblocking(
        rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
        keypair: Arc<Keypair>,
    ) -> Self {
        Self {
            rpc_nonblocking_client,
            keypair,
            rpc_client: None,
        }
    }

    /// Executes a token swap on PumpFun with the specified parameters
    pub async fn swap(
        &self,
        mint: &str,
        amount_in: u64,
        swap_direction: SwapDirection,
        slippage_bps: u64,
        jito_client: Arc<JitoRpcClient>,
        timestamp: Instant,
    ) -> Result<Vec<String>> {
        // Input validation
        self.validate_swap_params(mint, amount_in, slippage_bps)?;
        
        // Get the appropriate RPC client
        let client = self.get_rpc_client()?;
        
        // Build swap instructions based on direction and parameters
        let instructions = self.build_swap_instructions(
            mint,
            amount_in,
            swap_direction,
            slippage_bps,
        ).await?;
        
        // Execute the transaction
        tx::new_signed_and_send(
            &client,
            &self.keypair,
            instructions,
            jito_client,
            timestamp,
        )
        .await
        .context("Failed to execute swap transaction")
    }

    /// Validates swap parameters to ensure they are within acceptable ranges
    fn validate_swap_params(
        &self,
        mint: &str,
        amount_in: u64,
        slippage_bps: u64,
    ) -> Result<()> {
        // Validate mint address format
        Pubkey::from_str(mint)
            .context("Invalid mint address format")?;
        
        // Validate amount is not zero
        if amount_in == 0 {
            return Err(anyhow!("Swap amount cannot be zero"));
        }
        
        // Validate slippage is reasonable (max 50% = 5000 bps)
        if slippage_bps > 5000 {
            return Err(anyhow!("Slippage tolerance too high: {}bps (max: 5000bps)", slippage_bps));
        }
        
        Ok(())
    }

    /// Gets the blocking RPC client, returning an error if not available
    fn get_rpc_client(&self) -> Result<&Arc<solana_client::rpc_client::RpcClient>> {
        self.rpc_client
            .as_ref()
            .ok_or_else(|| anyhow!("Blocking RPC client not available"))
    }

    /// Builds the necessary instructions for the swap transaction
    async fn build_swap_instructions(
        &self,
        mint: &str,
        amount_in: u64,
        swap_direction: SwapDirection,
        slippage_bps: u64,
    ) -> Result<Vec<Instruction>> {
        let mint_pubkey = Pubkey::from_str(mint)?;
        
        // Get bonding curve information
        let pump_program = Pubkey::from_str(PUMP_PROGRAM)?;
        let (bonding_curve, associated_bonding_curve, bonding_curve_account) = 
            get_bonding_curve_account(
                self.rpc_client.as_ref().unwrap().clone(),
                &mint_pubkey,
                &pump_program,
            ).await?;

        // Calculate amounts based on swap direction and slippage
        let (min_amount_out, max_amount_in) = self.calculate_swap_amounts(
            amount_in,
            slippage_bps,
            &swap_direction,
            &bonding_curve_account,
        )?;

        // Build instructions based on swap direction
        match swap_direction {
            SwapDirection::Buy => {
                self.build_buy_instructions(
                    &mint_pubkey,
                    amount_in,
                    min_amount_out,
                    &bonding_curve,
                    &associated_bonding_curve,
                ).await
            }
            SwapDirection::Sell => {
                self.build_sell_instructions(
                    &mint_pubkey,
                    amount_in,
                    min_amount_out,
                    &bonding_curve,
                    &associated_bonding_curve,
                ).await
            }
        }
    }

    /// Calculates the appropriate amounts for the swap based on slippage tolerance
    fn calculate_swap_amounts(
        &self,
        amount_in: u64,
        slippage_bps: u64,
        swap_direction: &SwapDirection,
        bonding_curve_account: &BondingCurveAccount,
    ) -> Result<(u64, u64)> {
        match swap_direction {
            SwapDirection::Buy => {
                // For buys: calculate minimum tokens to receive
                let min_tokens_out = min_amount_with_slippage(amount_in, slippage_bps)?;
                let max_sol_in = max_amount_with_slippage(amount_in, slippage_bps)?;
                Ok((min_tokens_out, max_sol_in))
            }
            SwapDirection::Sell => {
                // For sells: calculate minimum SOL to receive
                let min_sol_out = min_amount_with_slippage(amount_in, slippage_bps)?;
                Ok((min_sol_out, amount_in))
            }
        }
    }

    /// Builds instructions for buying tokens
    async fn build_buy_instructions(
        &self,
        mint: &Pubkey,
        sol_amount: u64,
        min_tokens_out: u64,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
    ) -> Result<Vec<Instruction>> {
        // Implementation for buy instructions
        // This would include creating associated token accounts if needed,
        // and building the actual pump.fun buy instruction
        todo!("Implement buy instruction building")
    }

    /// Builds instructions for selling tokens
    async fn build_sell_instructions(
        &self,
        mint: &Pubkey,
        token_amount: u64,
        min_sol_out: u64,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
    ) -> Result<Vec<Instruction>> {
        // Implementation for sell instructions
        // This would include building the actual pump.fun sell instruction
        todo!("Implement sell instruction building")
    }

    /// Gets current token price from bonding curve
    pub async fn get_token_price(&self, mint: &str) -> Result<f64>

    /// Checks if a token has graduated to Raydium
    pub async fn is_token_graduated(&self, mint: &str) -> Result<bool>

    /// Gets comprehensive token information
    pub async fn get_token_info(&self, mint: &str) -> Result<TokenInfo>

    /// Estimates transaction fees for a swap
    pub async fn estimate_swap_fees(&self, mint: &str, swap_direction: SwapDirection) -> Result<SwapFees>

    /// Gets the user's SOL balance
    pub async fn get_sol_balance(&self) -> Result<u64>

    /// Gets the user's token balance for a specific mint
    pub async fn get_token_balance(&self, mint: &str) -> Result<u64>

    /// Checks if a token has graduated to Raydium
    pub async fn is_token_graduated(&self, mint: &str) -> Result<bool>

    /// Gets comprehensive token information
    pub async fn get_token_info(&self, mint: &str) -> Result<TokenInfo>

    /// Estimates transaction fees for a swap
    pub async fn estimate_swap_fees(&self, mint: &str, swap_direction: SwapDirection) -> Result<SwapFees>

    /// Checks if wallet has sufficient balance for the swap
    async fn check_wallet_balance(&self, swap_direction: &SwapDirection, amount: u64) -> Result<()>
}

fn min_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> Result<u64, &'static str> {
    // Validate slippage is not greater than 100% (10,000 basis points)
    if slippage_bps >= TEN_THOUSAND {
        return Err("Slippage cannot be 100% or greater");
    }
    
    // Calculate the percentage to keep (more efficient single calculation)
    let keep_percentage = TEN_THOUSAND - slippage_bps;
    
    // Perform the calculation with proper error handling
    input_amount
        .checked_mul(keep_percentage)
        .and_then(|result| result.checked_div(TEN_THOUSAND))
        .ok_or("Arithmetic overflow in slippage calculation")
}
fn max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> Result<u64, &'static str> {
    // Validate slippage to prevent unreasonable values (e.g., > 10000 bps = 100%)
    if slippage_bps > TEN_THOUSAND {
        return Err("Slippage exceeds 100%, which may indicate an error");
    }
    
    // Calculate the multiplier percentage (100% + slippage)
    let multiplier_percentage = TEN_THOUSAND
        .checked_add(slippage_bps)
        .ok_or("Overflow when adding slippage to base percentage")?;
    
    // Perform the calculation with proper error handling
    input_amount
        .checked_mul(multiplier_percentage)
        .and_then(|result| result.checked_div(TEN_THOUSAND))
        .ok_or("Arithmetic overflow in slippage calculation")
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaydiumInfo {
    pub base: f64,
    pub quote: f64,
    pub price: f64,
}
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PumpInfo {
    pub mint: String,
    pub bonding_curve: String,
    pub associated_bonding_curve: String,
    pub raydium_pool: Option<String>,
    pub raydium_info: Option<RaydiumInfo>,
    pub complete: bool,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub total_supply: u64,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
pub struct BondingCurveAccount {
    pub discriminator: u64,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
}

pub async fn get_bonding_curve_account(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<(Pubkey, Pubkey, BondingCurveAccount)> {
    Ok((
        bonding_curve,
        associated_bonding_curve,
        bonding_curve_account,
    ))
}

pub fn get_pda(mint: &Pubkey, program_id: &Pubkey) -> Result<Pubkey> {
    let seeds = [b"bonding-curve".as_ref(), mint.as_ref()];
    let (bonding_curve, _bump) = Pubkey::find_program_address(&seeds, program_id);
    Ok(bonding_curve)
}

pub async fn get_pump_info(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: &str,
) -> Result<PumpInfo> {
    Ok(pump_info)
}

// These would need to be added to support the new functions:
pub struct TokenInfo {
    pub mint: String,
    pub price: f64,
    pub user_balance: u64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub total_supply: u64,
    pub is_graduated: bool,
    pub market_cap: f64,
}

pub struct SwapFees {
    pub base_transaction_fee: u64,
    pub platform_fee_bps: u64,
    pub token_account_creation_fee: u64,
    pub total_estimated_fee: u64,
}

use tracing::{info, warn, error};
use std::time::Instant;

use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::time::Instant;
use jito_json_rpc_client::jsonrpc_client::rpc_client::RpcClient as JitoRpcClient;

/// Executes a pump swap with improved error handling and validation
pub async fn pump_swap(
    state: AppState,
    amount_in: u64,
    swap_direction: &str,
    slippage: u64,
    mint: &str,
    jito_client: Arc<JitoRpcClient>,
    timestamp: Instant,
) -> Result<Vec<String>> {
    // Parse and validate swap direction
    let swap_direction = parse_swap_direction(swap_direction)?;
    
    // Validate inputs early
    validate_pump_swap_inputs(amount_in, slippage, mint)?;
    
    // Create Pump instance (reuse if possible in production)
    let pump = Pump::new(
        state.rpc_nonblocking_client,
        state.rpc_client,
        state.wallet,
    );
    
    // Log timing information
    println!("Pump swap initiated after: {:.2?}", timestamp.elapsed());
    
    // Execute swap with proper error propagation
    pump.swap(
        mint,
        amount_in,
        swap_direction,
        slippage,
        jito_client,
        timestamp,
    )
    .await
    .map_err(|e| anyhow!("Pump swap failed: {}", e))
}

/// Enhanced version with additional features
pub async fn pump_swap_enhanced(
    state: AppState,
    amount_in: u64,
    swap_direction: &str,
    slippage: Option<u64>,
    mint: &str,
    jito_client: Arc<JitoRpcClient>,
    timestamp: Instant,
) -> Result<PumpSwapResult> {
    // Parse swap direction
    let swap_direction = parse_swap_direction(swap_direction)?;
    
    // Use default slippage if not provided
    let slippage = slippage.unwrap_or(DEFAULT_SLIPPAGE_BPS);
    
    // Validate inputs
    validate_pump_swap_inputs(amount_in, slippage, mint)?;
    
    // Create Pump instance
    let pump = Pump::new(
        state.rpc_nonblocking_client,
        state.rpc_client,
        state.wallet,
    );
    
    // Pre-swap validation
    pump.check_wallet_balance(&swap_direction, amount_in).await?;
    
    // Get price before swap for comparison
    let price_before = pump.get_token_price(mint).await.ok();
    
    // Estimate fees
    let estimated_fees = pump.estimate_swap_fees(mint, swap_direction).await?;
    
    println!("Executing swap - Elapsed: {:.2?}, Estimated fees: {} lamports", 
             timestamp.elapsed(), estimated_fees.total_estimated_fee);
    
    // Execute the swap
    let transaction_signatures = pump.swap(
        mint,
        amount_in,
        swap_direction,
        slippage,
        jito_client,
        timestamp,
    ).await?;
    
    // Get price after swap (optional, for analytics)
    let price_after = pump.get_token_price(mint).await.ok();
    
    Ok(PumpSwapResult {
        transaction_signatures,
        estimated_fees,
        price_before,
        price_after,
        execution_time: timestamp.elapsed(),
    })
}

/// Simplified swap function with sensible defaults
pub async fn pump_swap_simple(
    state: AppState,
    amount_in: u64,
    swap_direction: &str,
    mint: &str,
    jito_client: Arc<JitoRpcClient>,
) -> Result<Vec<String>> {
    let swap_direction = parse_swap_direction(swap_direction)?;
    
    let pump = Pump::new(
        state.rpc_nonblocking_client,
        state.rpc_client,
        state.wallet,
    );
    
    // Use appropriate method based on direction
    match swap_direction {
        SwapDirection::Buy => pump.buy_token(mint, amount_in, jito_client).await,
        SwapDirection::Sell => pump.sell_token(mint, amount_in, jito_client).await,
    }
}

/// Parses string swap direction into enum
fn parse_swap_direction(direction: &str) -> Result<SwapDirection> {
    match direction.to_lowercase().as_str() {
        "buy" | "b" => Ok(SwapDirection::Buy),
        "sell" | "s" => Ok(SwapDirection::Sell),
        _ => Err(anyhow!("Invalid swap direction: '{}'. Use 'buy' or 'sell'", direction)),
    }
}

/// Validates pump swap input parameters
fn validate_pump_swap_inputs(amount_in: u64, slippage: u64, mint: &str) -> Result<()> {
    // Validate amount
    if amount_in == 0 {
        return Err(anyhow!("Amount cannot be zero"));
    }
    
    // Validate slippage
    if slippage > MAX_SLIPPAGE_BPS {
        return Err(anyhow!("Slippage too high: {}bps (max: {}bps)", 
                          slippage, MAX_SLIPPAGE_BPS));
    }
    
    // Validate mint address
    Pubkey::from_str(mint)
        .map_err(|_| anyhow!("Invalid mint address: {}", mint))?;
    
    Ok(())
}

/// Result structure for enhanced swap function
#[derive(Debug)]
pub struct PumpSwapResult {
    pub transaction_signatures: Vec<String>,
    pub estimated_fees: SwapFees,
    pub price_before: Option<f64>,
    pub price_after: Option<f64>,
    pub execution_time: std::time::Duration,
}

/// Batch swap function for multiple tokens
pub async fn pump_swap_batch(
    state: AppState,
    swaps: Vec<SwapRequest>,
    jito_client: Arc<JitoRpcClient>,
) -> Result<Vec<Result<Vec<String>>>> {
    let pump = Pump::new(
        state.rpc_nonblocking_client,
        state.rpc_client,
        state.wallet,
    );
    
    let mut results = Vec::new();
    
    for swap_request in swaps {
        let swap_direction = parse_swap_direction(&swap_request.direction)?;
        
        let result = pump.swap(
            &swap_request.mint,
            swap_request.amount,
            swap_direction,
            swap_request.slippage.unwrap_or(DEFAULT_SLIPPAGE_BPS),
            jito_client.clone(),
            Instant::now(),
        ).await;
        
        results.push(result);
    }
    
    Ok(results)
}

/// Request structure for batch swaps
#[derive(Debug, Clone)]
pub struct SwapRequest {
    pub mint: String,
    pub amount: u64,
    pub direction: String,
    pub slippage: Option<u64>,
}

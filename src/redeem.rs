//! CTF Redeem: redeem winner tokens for settled markets to pUSD (V2) or USDC.e (legacy).
//!
//! Supports **Gnosis Safe** (execTransaction) and **Magic/Email EIP-1167** (Polymarket Relayer).
//! V2 默认经 CollateralAdapter 赎回为 pUSD；设 `REDEEM_OUTPUT_TOKEN=USDC.e` 可走 legacy 路径。
//! pUSD 首次赎回需对 CTF 执行 `setApprovalForAll(adapter, true)`，本模块会自动处理。

use std::env;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use anyhow::Result;
use polymarket_client_sdk::{contract_config, POLYGON};
use std::str::FromStr as _;
use tracing::{info, warn};

use alloy::sol;
sol! {
    #[sol(rpc)]
    interface IERC1155 {
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

use crate::adapter_auth::{ensure_adapter_approved, encode_set_approval_for_all, wait_relayer_tx};
use crate::deposit_wallet_relay::{
    relayer_execute_deposit_wallet_calldata, use_deposit_wallet_relayer,
};
use crate::proxy_relay::{
    derive_proxy_wallet, relayer_execute_proxy_calldata, IGnosisSafe, CTF_COLLATERAL_ADAPTER,
    CTF_POLYGON, NEG_RISK_ADAPTER, NEG_RISK_COLLATERAL_ADAPTER, PROXY_FACTORY,
    PROXY_REDEEM_LEGACY_GAS, PROXY_REDEEM_PUSD_GAS, RELAYER_URL_DEFAULT, RPC_URL_DEFAULT,
    USDC_POLYGON, PUSD_POLYGON,
};

const PARENT_COLLECTION_ID: B256 = B256::ZERO;

fn encode_redeem_calldata_4arg(collateral_token: Address, condition_id: B256) -> Vec<u8> {
    let sel = &keccak256(b"redeemPositions(address,bytes32,bytes32,uint256[])")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(collateral_token.as_slice());
    out.extend_from_slice(PARENT_COLLECTION_ID.as_slice());
    out.extend_from_slice(condition_id.as_slice());
    out.extend_from_slice(&U256::from(128u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
    out
}

fn encode_redeem_calldata_neg_risk_legacy(condition_id: B256, amounts: [U256; 2]) -> Vec<u8> {
    let sel = &keccak256(b"redeemPositions(bytes32,uint256[])")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(condition_id.as_slice());
    out.extend_from_slice(&U256::from(128u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
    out.extend_from_slice(&amounts[0].to_be_bytes::<32>());
    out.extend_from_slice(&amounts[1].to_be_bytes::<32>());
    out
}

fn redeem_to_pusd() -> bool {
    match env::var("REDEEM_OUTPUT_TOKEN") {
        Ok(v) => {
            let s = v.trim().to_lowercase();
            s != "usdc.e" && s != "usdc"
        }
        Err(_) => true,
    }
}

fn pusd_adapter(neg_risk: bool) -> Address {
    if neg_risk {
        NEG_RISK_COLLATERAL_ADAPTER
    } else {
        CTF_COLLATERAL_ADAPTER
    }
}

fn resolve_redeem_call(
    neg_risk: bool,
    condition_id: B256,
    outcome_index: Option<i32>,
    size_raw: Option<U256>,
) -> Result<(Address, Vec<u8>)> {
    if redeem_to_pusd() {
        let target = pusd_adapter(neg_risk);
        let calldata = encode_redeem_calldata_4arg(PUSD_POLYGON, condition_id);
        return Ok((target, calldata));
    }

    if neg_risk {
        let idx = outcome_index.ok_or_else(|| {
            anyhow::anyhow!("NegRisk legacy USDC.e 赎回需要 outcome_index（或设 REDEEM_OUTPUT_TOKEN=pUSD）")
        })?;
        let size = size_raw.ok_or_else(|| {
            anyhow::anyhow!("NegRisk legacy USDC.e 赎回需要持仓 size（或设 REDEEM_OUTPUT_TOKEN=pUSD）")
        })?;
        let mut amounts = [U256::ZERO, U256::ZERO];
        if idx == 0 || idx == 1 {
            amounts[idx as usize] = size;
        } else {
            anyhow::bail!("无效的 outcome_index: {}", idx);
        }
        let calldata = encode_redeem_calldata_neg_risk_legacy(condition_id, amounts);
        return Ok((NEG_RISK_ADAPTER, calldata));
    }

    let config = contract_config(POLYGON, false)
        .ok_or_else(|| anyhow::anyhow!("Unsupported chain_id: {}", POLYGON))?;
    let calldata = encode_redeem_calldata_4arg(USDC_POLYGON, condition_id);
    Ok((config.conditional_tokens, calldata))
}

async fn outcome_balance<P: Provider>(provider: &P, proxy: Address, asset: U256) -> Result<U256> {
    let ctf = IERC1155::new(CTF_POLYGON, provider);
    ctf
        .balanceOf(proxy, asset)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("balanceOf failed: {}", e))
}

async fn verify_redeemed<P: Provider>(
    provider: &P,
    proxy: Address,
    assets: &[U256],
    before: &[U256],
) -> Result<()> {
    for (asset, prev) in assets.iter().zip(before.iter()) {
        let after = outcome_balance(provider, proxy, *asset).await?;
        if after >= *prev {
            anyhow::bail!(
                "赎回后 outcome token {} 余额未减少 (before={} after={})，Relayer 可能 gas 不足或链上 redeem 失败",
                asset,
                prev,
                after
            );
        }
    }
    Ok(())
}

fn redeem_gas_limit() -> u64 {
    if redeem_to_pusd() {
        PROXY_REDEEM_PUSD_GAS
    } else {
        PROXY_REDEEM_LEGACY_GAS
    }
}

fn use_relayer_by_config(code_len: usize) -> bool {
    if use_deposit_wallet_relayer() {
        return true;
    }
    let s = match env::var("SIGNATURE_TYPE") {
        Ok(v) => v.trim().to_lowercase(),
        Err(_) => return code_len < 150,
    };
    if s == "proxy" {
        return true;
    }
    if s == "gnosissafe" || s == "safe" {
        return false;
    }
    code_len < 150
}

async fn relayer_redeem_calldata(
    redeem_data: &[u8],
    redeem_to: Address,
    wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    gas_limit: Option<u64>,
) -> Result<String> {
    if use_deposit_wallet_relayer() {
        relayer_execute_deposit_wallet_calldata(
            redeem_data,
            redeem_to,
            wallet,
            signer,
            builder_key,
            builder_secret,
            builder_passphrase,
            relayer_url,
            "Redeem positions",
        )
        .await
    } else {
        relayer_execute_proxy_calldata(
            redeem_data,
            redeem_to,
            wallet,
            signer,
            builder_key,
            builder_secret,
            builder_passphrase,
            relayer_url,
            "Redeem positions",
            gas_limit,
        )
        .await
    }
}

async fn safe_exec_call<P: Provider>(
    safe: &IGnosisSafe::IGnosisSafeInstance<P>,
    signer: &impl alloy::signers::Signer,
    to: Address,
    calldata: Vec<u8>,
) -> Result<B256> {
    let nonce: U256 = safe
        .nonce()
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read Safe nonce: {}", e))?;
    let tx_hash_data = safe
        .encodeTransactionData(
            to,
            U256::ZERO,
            calldata.clone().into(),
            0u8,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            Address::ZERO,
            Address::ZERO,
            nonce,
        )
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Safe.encodeTransactionData failed: {}", e))?
        .0;
    let tx_hash = keccak256(tx_hash_data.as_ref());
    let sig = signer
        .sign_hash(&tx_hash)
        .await
        .map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;
    let mut sig_bytes = sig.as_bytes().to_vec();
    if sig_bytes.len() == 65 && (sig_bytes[64] == 0 || sig_bytes[64] == 1) {
        sig_bytes[64] += 27;
    }
    let pending = safe
        .execTransaction(
            to,
            U256::ZERO,
            calldata.into(),
            0u8,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            Address::ZERO,
            Address::ZERO,
            sig_bytes.into(),
        )
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Safe.execTransaction failed: {}", e))?;
    let tx_hash_out = *pending.tx_hash();
    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("Failed waiting for receipt: {}", e))?;
    if !receipt.status() {
        anyhow::bail!("Safe tx reverted: {:#x}", tx_hash_out);
    }
    Ok(tx_hash_out)
}

async fn build_redeem_calls<P: Provider>(
    provider: &P,
    proxy: Address,
    neg_risk: bool,
    redeem_target: Address,
    redeem_calldata: Vec<u8>,
) -> Result<Vec<(Address, Vec<u8>)>> {
    let _ = (provider, proxy, neg_risk);
    Ok(vec![(redeem_target, redeem_calldata)])
}

/// Redeem winner tokens for `condition_id` on `proxy`.
/// `verify_assets`: 赎回前后校验这些 outcome token 余额是否减少（可为空跳过）。
pub async fn redeem_one(
    condition_id: B256,
    neg_risk: bool,
    proxy: Address,
    private_key: &str,
    rpc_url: Option<&str>,
    outcome_index: Option<i32>,
    size_raw: Option<U256>,
    verify_assets: &[U256],
) -> Result<String> {
    let rpc = rpc_url.unwrap_or(RPC_URL_DEFAULT);
    let chain = POLYGON;
    let signer = LocalSigner::from_str(private_key)?.with_chain_id(Some(chain));
    let wallet = signer.address();

    let (redeem_target, redeem_calldata) =
        resolve_redeem_call(neg_risk, condition_id, outcome_index, size_raw)?;

    let output = if redeem_to_pusd() { "pUSD" } else { "USDC.e" };
    info!(
        "Redeem {:?} | neg_risk={} | target={:?} | output={}",
        condition_id, neg_risk, redeem_target, output
    );

    let provider = ProviderBuilder::new().wallet(signer.clone()).connect(rpc).await?;
    let mut balances_before = Vec::new();
    for asset in verify_assets {
        balances_before.push(outcome_balance(&provider, proxy, *asset).await?);
    }
    let gas_limit = Some(redeem_gas_limit());
    info!("Relayer redeem gasLimit={}", gas_limit.unwrap_or(PROXY_REDEEM_PUSD_GAS));
    let calls = build_redeem_calls(&provider, proxy, neg_risk, redeem_target, redeem_calldata).await?;
    let code = provider.get_code_at(proxy).await.unwrap_or_default();
    let use_relayer = use_relayer_by_config(code.len());

    if use_relayer {
        if !use_deposit_wallet_relayer() {
            let derived = derive_proxy_wallet(wallet, PROXY_FACTORY);
            let try_anyway = env::var("MERGE_TRY_ANYWAY")
                .map(|s| s.trim() == "1" || s.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if derived != proxy && !try_anyway {
                anyhow::bail!(
                    "POLYMARKET_PROXY_ADDRESS ({:?}) does not match ProxyFactory derive ({:?}). Set MERGE_TRY_ANYWAY=1 to force.",
                    proxy, derived
                );
            }
            if derived != proxy {
                warn!("MERGE_TRY_ANYWAY=1: derive != proxy, still sending Relayer request.");
            }
        }
        let builder_key = env::var("POLY_BUILDER_API_KEY").ok();
        let builder_secret = env::var("POLY_BUILDER_SECRET").ok();
        let builder_passphrase = env::var("POLY_BUILDER_PASSPHRASE").ok();
        let relayer_url = env::var("RELAYER_URL").unwrap_or_else(|_| RELAYER_URL_DEFAULT.to_string());
        match (builder_key.as_deref(), builder_secret.as_deref(), builder_passphrase.as_deref()) {
            (Some(k), Some(s), Some(p)) => {
                if redeem_to_pusd() {
                    ensure_adapter_approved(
                        &provider,
                        proxy,
                        pusd_adapter(neg_risk),
                        &signer,
                        k,
                        s,
                        p,
                        &relayer_url,
                    )
                    .await?;
                }
                let (redeem_to, redeem_data) = calls
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing redeem call"))?;
                let out = relayer_redeem_calldata(
                    &redeem_data,
                    redeem_to,
                    proxy,
                    &signer,
                    k,
                    s,
                    p,
                    &relayer_url,
                    gas_limit,
                )
                .await?;
                wait_relayer_tx(&provider, &out).await?;
                if !verify_assets.is_empty() {
                    verify_redeemed(&provider, proxy, verify_assets, &balances_before).await?;
                }
                info!("✅ Relayer redeem confirmed: {}", out);
                return Ok(out);
            }
            _ => anyhow::bail!(
                "Magic/Email requires POLY_BUILDER_API_KEY, POLY_BUILDER_SECRET, POLY_BUILDER_PASSPHRASE.",
            ),
        }
    }

    let safe = IGnosisSafe::new(proxy, provider.clone());
    if redeem_to_pusd() {
        let adapter = pusd_adapter(neg_risk);
        if !crate::adapter_auth::is_adapter_approved(&provider, CTF_POLYGON, proxy, adapter).await? {
            let approve_calldata = encode_set_approval_for_all(adapter, true);
            let tx = safe_exec_call(&safe, &signer, CTF_POLYGON, approve_calldata).await?;
            info!("✅ Safe setApprovalForAll tx: {:#x}", tx);
        }
    }
    let mut last_tx = B256::ZERO;
    for (to, calldata) in calls {
        last_tx = safe_exec_call(&safe, &signer, to, calldata).await?;
        info!("✅ Safe redeem tx: {:#x}", last_tx);
    }
    if !verify_assets.is_empty() {
        verify_redeemed(&provider, proxy, verify_assets, &balances_before).await?;
    }
    Ok(format!("{:#x}", last_tx))
}

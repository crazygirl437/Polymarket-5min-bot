//! Wrap USDC.e → pUSD via Collateral Onramp after legacy-collateral merge.

use std::env;

use alloy::primitives::{keccak256, Address, U256};
use alloy::providers::Provider;
use anyhow::Result;
use tracing::info;

use crate::deposit_wallet_relay::relayer_execute_deposit_wallet_calls;
use crate::wallet_kind::WalletKind;
use crate::proxy_relay::{
    relayer_execute_proxy_calls, IGnosisSafe, COLLATERAL_ONRAMP, PUSD_POLYGON, USDC_POLYGON,
};

use alloy::sol;
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// True when merge output should be wrapped to pUSD (default on when `MERGE_OUTPUT_TOKEN=pUSD`).
pub fn merge_wrap_to_pusd() -> bool {
    match env::var("MERGE_WRAP_TO_PUSD") {
        Ok(v) => {
            let s = v.trim().to_lowercase();
            s != "0" && s != "false" && s != "no" && s != "off"
        }
        Err(_) => true,
    }
}

pub fn encode_erc20_approve(spender: Address, amount: U256) -> Vec<u8> {
    let sel = &keccak256(b"approve(address,uint256)")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(spender.as_slice());
    out.extend_from_slice(&amount.to_be_bytes::<32>());
    out
}

pub fn encode_onramp_wrap(recipient: Address, amount: U256) -> Vec<u8> {
    let sel = &keccak256(b"wrap(address,address,uint256)")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(USDC_POLYGON.as_slice());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(recipient.as_slice());
    out.extend_from_slice(&amount.to_be_bytes::<32>());
    out
}

async fn erc20_balance<P: Provider>(provider: &P, owner: Address, token: Address) -> Result<U256> {
    IERC20::new(token, provider)
        .balanceOf(owner)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("ERC20 balanceOf failed: {}", e))
}

async fn erc20_allowance<P: Provider>(
    provider: &P,
    owner: Address,
    token: Address,
    spender: Address,
) -> Result<U256> {
    IERC20::new(token, provider)
        .allowance(owner, spender)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("ERC20 allowance failed: {}", e))
}

async fn safe_exec_call<P: Provider>(
    safe: &IGnosisSafe::IGnosisSafeInstance<P>,
    signer: &impl alloy::signers::Signer,
    to: Address,
    calldata: Vec<u8>,
) -> Result<alloy::primitives::B256> {
    use alloy::primitives::keccak256;
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

async fn relayer_wrap_calls(
    wallet_kind: WalletKind,
    wallet: Address,
    calls: Vec<(Address, Vec<u8>)>,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
) -> Result<String> {
    match wallet_kind {
        WalletKind::DepositWallet => {
            relayer_execute_deposit_wallet_calls(
                &calls,
                wallet,
                signer,
                builder_key,
                builder_secret,
                builder_passphrase,
                relayer_url,
                "Wrap USDC.e to pUSD",
            )
            .await
        }
        WalletKind::MagicProxy => {
            relayer_execute_proxy_calls(
                &calls,
                wallet,
                signer,
                builder_key,
                builder_secret,
                builder_passphrase,
                relayer_url,
                "Wrap USDC.e to pUSD",
                None,
            )
            .await
        }
        WalletKind::GnosisSafe => {
            anyhow::bail!("relayer_wrap_calls: unexpected GnosisSafe");
        }
    }
}

/// After a USDC.e-collateral merge, wrap `amount` USDC.e to pUSD in the deposit wallet.
pub async fn wrap_usdce_to_pusd<P: Provider>(
    provider: &P,
    prov_read: &impl Provider,
    wallet: Address,
    amount: U256,
    wallet_kind: WalletKind,
    safe: Option<&IGnosisSafe::IGnosisSafeInstance<P>>,
    signer: &impl alloy::signers::Signer,
    builder_key: Option<&str>,
    builder_secret: Option<&str>,
    builder_passphrase: Option<&str>,
    relayer_url: &str,
) -> Result<()> {
    if amount == U256::ZERO {
        return Ok(());
    }

    let usdc_before = erc20_balance(prov_read, wallet, USDC_POLYGON).await?;
    if usdc_before < amount {
        anyhow::bail!(
            "wrap 需要 {} USDC.e，钱包余额仅 {}",
            amount,
            usdc_before
        );
    }
    let pusd_before = erc20_balance(prov_read, wallet, PUSD_POLYGON).await?;
    info!(
        "🔄 Wrap USDC.e → pUSD | amount={} ({}) | wallet={:?}",
        amount,
        amount / U256::from(1_000_000),
        wallet
    );

    match wallet_kind {
        WalletKind::GnosisSafe => {
            let safe = safe.ok_or_else(|| anyhow::anyhow!("Safe instance required for wrap"))?;
            let allowance = erc20_allowance(prov_read, wallet, USDC_POLYGON, COLLATERAL_ONRAMP).await?;
            if allowance < amount {
                let approve = encode_erc20_approve(COLLATERAL_ONRAMP, amount);
                let tx = safe_exec_call(safe, signer, USDC_POLYGON, approve).await?;
                info!("✅ Safe USDC.e approve for Onramp: {:#x}", tx);
            }
            let wrap_calldata = encode_onramp_wrap(wallet, amount);
            let tx = safe_exec_call(safe, signer, COLLATERAL_ONRAMP, wrap_calldata).await?;
            info!("✅ Safe wrap tx: {:#x}", tx);
        }
        WalletKind::DepositWallet | WalletKind::MagicProxy => {
            let (k, s, p) = match (builder_key, builder_secret, builder_passphrase) {
                (Some(k), Some(s), Some(p)) => (k, s, p),
                _ => anyhow::bail!("Wrap via relayer requires POLY_BUILDER_* credentials"),
            };
            let allowance = erc20_allowance(prov_read, wallet, USDC_POLYGON, COLLATERAL_ONRAMP).await?;
            let mut calls: Vec<(Address, Vec<u8>)> = Vec::new();
            if allowance < amount {
                calls.push((
                    USDC_POLYGON,
                    encode_erc20_approve(COLLATERAL_ONRAMP, amount),
                ));
            }
            calls.push((COLLATERAL_ONRAMP, encode_onramp_wrap(wallet, amount)));
            let tx = relayer_wrap_calls(
                wallet_kind,
                wallet,
                calls,
                signer,
                k,
                s,
                p,
                relayer_url,
            )
            .await?;
            crate::adapter_auth::wait_relayer_tx(provider, &tx).await?;
            info!("✅ Relayer wrap confirmed: {}", tx);
        }
    }

    let pusd_after = erc20_balance(prov_read, wallet, PUSD_POLYGON).await?;
    if pusd_after < pusd_before + amount {
        anyhow::bail!(
            "wrap 后 pUSD 余额未增加 (before={} after={} expected +{})",
            pusd_before,
            pusd_after,
            amount
        );
    }
    info!(
        "✅ Wrapped to pUSD | +{} (wallet pUSD balance={})",
        amount,
        pusd_after
    );
    Ok(())
}

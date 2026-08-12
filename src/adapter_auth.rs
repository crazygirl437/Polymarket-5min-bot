//! Shared CTF adapter approval helpers for pUSD on-chain ops (merge, redeem).

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::Provider;
use anyhow::Result;
use tracing::info;

use crate::deposit_wallet_relay::{
    relayer_execute_deposit_wallet_calldata, use_deposit_wallet_relayer,
};
use crate::proxy_relay::{relayer_execute_proxy_calldata, CTF_POLYGON};

use alloy::sol;
sol! {
    #[sol(rpc)]
    interface IERC1155Approval {
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }
}

pub fn encode_set_approval_for_all(operator: Address, approved: bool) -> Vec<u8> {
    let sel = &keccak256(b"setApprovalForAll(address,bool)")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(operator.as_slice());
    out.extend_from_slice(&U256::from(approved as u8).to_be_bytes::<32>());
    out
}

pub async fn is_adapter_approved<P: Provider>(
    provider: &P,
    ctf: Address,
    owner: Address,
    adapter: Address,
) -> Result<bool> {
    let ctf_contract = IERC1155Approval::new(ctf, provider);
    ctf_contract
        .isApprovedForAll(owner, adapter)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("isApprovedForAll failed: {}", e))
}

pub async fn wait_relayer_tx<P: Provider>(provider: &P, tx_hash: &str) -> Result<()> {
    let hash: B256 = tx_hash
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid tx hash {}: {}", tx_hash, e))?;
    for _ in 0..60 {
        if let Some(receipt) = provider.get_transaction_receipt(hash).await? {
            if !receipt.status() {
                anyhow::bail!("Relayer tx reverted on-chain: {}", tx_hash);
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    anyhow::bail!("Timed out waiting for relayer tx: {}", tx_hash);
}

async fn submit_adapter_approval<P: Provider>(
    _provider: &P,
    wallet: Address,
    adapter: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
) -> Result<String> {
    let approve_calldata = encode_set_approval_for_all(adapter, true);
    if use_deposit_wallet_relayer() {
        relayer_execute_deposit_wallet_calldata(
            &approve_calldata,
            CTF_POLYGON,
            wallet,
            signer,
            builder_key,
            builder_secret,
            builder_passphrase,
            relayer_url,
            "Approve CTF adapter",
        )
        .await
    } else {
        relayer_execute_proxy_calldata(
            &approve_calldata,
            CTF_POLYGON,
            wallet,
            signer,
            builder_key,
            builder_secret,
            builder_passphrase,
            relayer_url,
            "Approve CTF adapter",
            None,
        )
        .await
    }
}

pub async fn ensure_adapter_approved<P: Provider>(
    provider: &P,
    wallet: Address,
    adapter: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
) -> Result<()> {
    if is_adapter_approved(provider, CTF_POLYGON, wallet, adapter).await? {
        return Ok(());
    }
    info!("pUSD adapter {:?} 未授权，提交 setApprovalForAll …", adapter);
    let tx = submit_adapter_approval(
        provider,
        wallet,
        adapter,
        signer,
        builder_key,
        builder_secret,
        builder_passphrase,
        relayer_url,
    )
    .await?;
    wait_relayer_tx(provider, &tx).await?;
    if !is_adapter_approved(provider, CTF_POLYGON, wallet, adapter).await? {
        anyhow::bail!(
            "setApprovalForAll 已提交 ({}) 但链上仍未授权，请稍后重试",
            tx
        );
    }
    info!("✅ CTF adapter 已授权");
    Ok(())
}

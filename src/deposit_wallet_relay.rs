//! Polymarket V2 deposit wallet relayer (`WALLET` batch) for on-chain ops.

use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::{eip712_domain, SolStruct};
use anyhow::Result;
use tracing::info;

use crate::proxy_relay::{relayer_submit_authed, to_hex_0x, RELAYER_URL_DEFAULT};

/// Polygon mainnet deposit wallet factory (Polymarket docs).
pub const DEPOSIT_WALLET_FACTORY: Address = {
    use polymarket_client_sdk::types::address;
    address!("0x00000000000Fb5C9ADea0298D729A0CB3823Cc07")
};

const RELAYER_GET_NONCE: &str = "/nonce";
const WALLET_BATCH_DEADLINE_SECS: u64 = 600;

alloy::sol! {
    struct Call {
        address target;
        uint256 value;
        bytes data;
    }
    struct Batch {
        address wallet;
        uint256 nonce;
        uint256 deadline;
        Call[] calls;
    }
}

/// True when `SIGNATURE_TYPE` indicates V2 deposit wallet (Poly1271).
pub fn use_deposit_wallet_relayer() -> bool {
    match env::var("SIGNATURE_TYPE") {
        Ok(v) => {
            let s = v.trim().to_lowercase();
            s == "poly1271" || s == "deposit" || s == "deposit_wallet" || s == "3"
        }
        Err(_) => true,
    }
}

pub async fn get_wallet_nonce(relayer_url: &str, owner: Address) -> Result<U256> {
    let client = reqwest::Client::new();
    let base = relayer_url.trim_end_matches('/');
    let url = format!(
        "{}{}?address={:#x}&type=WALLET",
        base, RELAYER_GET_NONCE, owner
    );
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("GET /nonce (WALLET) failed status={} body={}", status, text);
    }
    let j: serde_json::Value = serde_json::from_str(&text)?;
    let nonce = j
        .get("nonce")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v.as_u64())
        })
        .unwrap_or(0);
    Ok(U256::from(nonce))
}

async fn sign_deposit_wallet_batch(
    signer: &impl alloy::signers::Signer,
    chain_id: u64,
    deposit_wallet: Address,
    nonce: U256,
    deadline: U256,
    calls: Vec<(Address, U256, Vec<u8>)>,
) -> Result<String> {
    let domain = eip712_domain! {
        name: "DepositWallet",
        version: "1",
        chain_id: chain_id,
        verifying_contract: deposit_wallet,
    };
    let batch_calls: Vec<Call> = calls
        .into_iter()
        .map(|(target, value, data)| Call {
            target,
            value,
            data: Bytes::from(data),
        })
        .collect();
    let batch = Batch {
        wallet: deposit_wallet,
        nonce,
        deadline,
        calls: batch_calls,
    };
    let hash = batch.eip712_signing_hash(&domain);
    let sig = signer
        .sign_hash(&hash)
        .await
        .map_err(|e| anyhow::anyhow!("deposit wallet batch sign failed: {}", e))?;
    let mut sig_bytes = sig.as_bytes().to_vec();
    if sig_bytes.len() == 65 && (sig_bytes[64] == 0 || sig_bytes[64] == 1) {
        sig_bytes[64] += 27;
    }
    Ok(to_hex_0x(&sig_bytes))
}

/// Execute one or more calls on a deposit wallet via relayer `WALLET` batch.
pub async fn relayer_execute_deposit_wallet_calls(
    calls: &[(Address, Vec<u8>)],
    deposit_wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    metadata: &str,
) -> Result<String> {
    if calls.is_empty() {
        anyhow::bail!("relayer_execute_deposit_wallet_calls: empty calls");
    }
    let relayer_url = if relayer_url.is_empty() {
        RELAYER_URL_DEFAULT
    } else {
        relayer_url
    };
    let owner = signer.address();
    let chain_id = signer.chain_id().unwrap_or(137);
    let nonce = get_wallet_nonce(relayer_url, owner).await?;
    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        + WALLET_BATCH_DEADLINE_SECS;

    let typed_calls: Vec<(Address, U256, Vec<u8>)> = calls
        .iter()
        .map(|(target, data)| (*target, U256::ZERO, data.clone()))
        .collect();
    let signature = sign_deposit_wallet_batch(
        signer,
        chain_id,
        deposit_wallet,
        nonce,
        U256::from(deadline),
        typed_calls,
    )
    .await?;

    let calls_json: Vec<serde_json::Value> = calls
        .iter()
        .map(|(target, data)| {
            serde_json::json!({
                "target": format!("{:#x}", target),
                "value": "0",
                "data": to_hex_0x(data),
            })
        })
        .collect();

    let body = serde_json::json!({
        "type": "WALLET",
        "from": format!("{:#x}", owner),
        "to": format!("{:#x}", DEPOSIT_WALLET_FACTORY),
        "nonce": nonce.to_string(),
        "signature": signature,
        "metadata": metadata,
        "depositWalletParams": {
            "depositWallet": format!("{:#x}", deposit_wallet),
            "deadline": deadline.to_string(),
            "calls": calls_json,
        }
    });

    info!(
        "Relayer WALLET batch | wallet={:?} | calls={} | nonce={}",
        deposit_wallet,
        calls.len(),
        nonce
    );

    relayer_submit_authed(
        body,
        builder_key,
        builder_secret,
        builder_passphrase,
        relayer_url,
    )
    .await
}

pub async fn relayer_execute_deposit_wallet_calldata(
    calldata: &[u8],
    target: Address,
    deposit_wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    metadata: &str,
) -> Result<String> {
    relayer_execute_deposit_wallet_calls(
        &[(target, calldata.to_vec())],
        deposit_wallet,
        signer,
        builder_key,
        builder_secret,
        builder_passphrase,
        relayer_url,
        metadata,
    )
    .await
}

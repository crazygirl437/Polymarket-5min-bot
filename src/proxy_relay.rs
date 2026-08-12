//! Proxy wallet + Relayer/Safe shared infrastructure for merge, redeem, withdraw.
//!
//! Relayer requests (/relay-payload, /submit), proxy call encoding, Gnosis Safe interface and signing.

use std::env;

use alloy::primitives::{keccak256, Address, B256, Bytes, U256};
use alloy::sol_types::SolCall;
use anyhow::Result;
use tracing::info;

use polymarket_client_sdk::types::address;

use alloy::sol;
sol! {
    #[sol(rpc)]
    interface IGnosisSafe {
        function nonce() external view returns (uint256);
        function encodeTransactionData(
            address to,
            uint256 value,
            bytes memory data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            uint256 _nonce
        ) external view returns (bytes memory);
        function execTransaction(
            address to,
            uint256 value,
            bytes memory data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            bytes memory signatures
        ) external payable returns (bool success);
    }
}

sol! {
    struct ProxyCallTuple {
        uint8 typeCode;
        address to;
        uint256 value;
        bytes data;
    }
    function proxy(ProxyCallTuple[] calls) external payable returns (bytes[] returnValues);
}

pub const RPC_URL_DEFAULT: &str = "https://polygon-bor-rpc.publicnode.com";
pub const RELAYER_URL_DEFAULT: &str = "https://relayer-v2.polymarket.com";
/// USDC.e（bridged），V1 及旧持仓抵押品
pub const USDC_POLYGON: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
/// pUSD（Polymarket USD），V2 抵押品
pub const PUSD_POLYGON: Address = address!("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB");
/// CTF（Conditional Tokens Framework）
pub const CTF_POLYGON: Address = address!("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045");
/// NegRisk 适配器（V1 赎回 USDC.e）
pub const NEG_RISK_ADAPTER: Address = address!("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296");
/// V2 标准市场抵押品适配器（赎回/merge 为 pUSD）
pub const CTF_COLLATERAL_ADAPTER: Address = address!("0xAdA100Db00Ca00073811820692005400218FcE1f");
/// V2 Collateral Onramp：USDC.e → pUSD wrap
pub const COLLATERAL_ONRAMP: Address = address!("0x93070a847efEf7F70739046A929D47a521F5B8ee");
/// V2 NegRisk 抵押品适配器（赎回为 pUSD）
pub const NEG_RISK_COLLATERAL_ADAPTER: Address = address!("0xadA2005600Dec949baf300f4C6120000bDB6eAab");

const RELAYER_GET_RELAY_PAYLOAD: &str = "/relay-payload";
const RELAYER_SUBMIT: &str = "/submit";

pub const PROXY_FACTORY: Address = address!("0xaB45c5A4B0c941a2F231C04C3f49182e1A254052");
const RELAY_HUB: Address = address!("0xD216153c06E857cD7f72665E0aF1d7D82172F494");
const PROXY_INIT_CODE_HASH: [u8; 32] = [
    0xd2, 0x1d, 0xf8, 0xdc, 0x65, 0x88, 0x0a, 0x86, 0x06, 0xf0, 0x9f, 0xe0, 0xce, 0x3d, 0xf9, 0xb8,
    0x86, 0x92, 0x87, 0xab, 0x0b, 0x05, 0x8b, 0xe0, 0x5a, 0xa9, 0xe8, 0xaf, 0x63, 0x30, 0xa0, 0x0b,
];
pub const PROXY_DEFAULT_GAS: u64 = 160_000;
/// pUSD CollateralAdapter redeem 实测约需 370k+ gas
pub const PROXY_REDEEM_PUSD_GAS: u64 = 450_000;
/// legacy CTF redeem 实测约需 166k gas
pub const PROXY_REDEEM_LEGACY_GAS: u64 = 220_000;
/// pUSD CollateralAdapter merge 实测约需 370k+ gas
pub const PROXY_MERGE_PUSD_GAS: u64 = 450_000;

/// Shorten long 0x-prefixed hex to `0x` + first 8 + `..` + last 6 for logs.
pub fn short_hex(s: &str) -> String {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.len() > 14 {
        let lo = hex.len().saturating_sub(6);
        format!("0x{}..{}", &hex[..8.min(hex.len())], &hex[lo..])
    } else {
        format!("0x{}", hex)
    }
}

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
type HmacSha256 = Hmac<Sha256>;

pub(crate) fn derive_proxy_wallet(eoa: Address, proxy_factory: Address) -> Address {
    let salt = keccak256(eoa.as_slice());
    let mut buf = [0u8; 1 + 20 + 32 + 32];
    buf[0] = 0xff;
    buf[1..21].copy_from_slice(proxy_factory.as_slice());
    buf[21..53].copy_from_slice(salt.as_slice());
    buf[53..85].copy_from_slice(&PROXY_INIT_CODE_HASH);
    let h = keccak256(buf);
    Address::from_slice(&h.as_slice()[12..32])
}

pub(crate) fn to_hex_0x(b: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for &x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 0xf) as usize] as char);
    }
    s
}

fn build_hmac_signature(secret: &[u8], timestamp: u64, method: &str, path: &str, body: &str) -> String {
    let msg = format!("{}{}{}{}", timestamp, method, path, body);
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key");
    mac.update(msg.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    sig.replace('+', "-").replace('/', "_")
}

pub(crate) async fn get_relay_payload(client: &reqwest::Client, base: &str, eoa: Address) -> Result<(Address, String)> {
    let url = format!("{}{}", base.trim_end_matches('/'), RELAYER_GET_RELAY_PAYLOAD);
    let resp = client
        .get(&url)
        .query(&[("address", format!("{:#x}", eoa)), ("type", "PROXY".to_string())])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("GET /relay-payload failed status={} body={}", status, text);
    }
    let j: serde_json::Value = serde_json::from_str(&text)?;
    let addr = j.get("address").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("relay-payload missing address"))?;
    let nonce = j
        .get("nonce")
        .map(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
                .unwrap_or_else(|| "0".into())
        })
        .unwrap_or_else(|| "0".into());
    let relay = addr.trim().parse::<Address>().map_err(|e| anyhow::anyhow!("Failed to parse relay address: {}", e))?;
    Ok((relay, nonce.to_string()))
}

pub(crate) fn encode_proxy_call(target: Address, data: &[u8]) -> Vec<u8> {
    encode_proxy_calls(&[(target, data)])
}

pub(crate) fn encode_proxy_calls(calls: &[(Address, &[u8])]) -> Vec<u8> {
    let tuples: Vec<ProxyCallTuple> = calls
        .iter()
        .map(|(to, data)| ProxyCallTuple {
            typeCode: 1u8,
            to: *to,
            value: U256::ZERO,
            data: Bytes::from(data.to_vec()),
        })
        .collect();
    proxyCall { calls: tuples }.abi_encode().to_vec()
}

pub(crate) fn create_struct_hash(
    from: Address,
    to: Address,
    data: &[u8],
    tx_fee: u64,
    gas_price: u64,
    gas_limit: u64,
    nonce: &str,
    relay_hub: Address,
    relay: Address,
) -> B256 {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"rlx:");
    buf.extend_from_slice(from.as_slice());
    buf.extend_from_slice(to.as_slice());
    buf.extend_from_slice(data);
    buf.extend_from_slice(&U256::from(tx_fee).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(gas_price).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(gas_limit).to_be_bytes::<32>());
    let n: u64 = nonce.parse().unwrap_or(0);
    buf.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    buf.extend_from_slice(relay_hub.as_slice());
    buf.extend_from_slice(relay.as_slice());
    keccak256(buf)
}

pub(crate) fn eip191_hash(struct_hash: B256) -> B256 {
    let mut msg = b"\x19Ethereum Signed Message:\n32".to_vec();
    msg.extend_from_slice(struct_hash.as_slice());
    keccak256(msg)
}

/// Execute one or more proxy calls via Relayer (gasless). Called by merge/redeem/withdraw.
pub(crate) async fn relayer_execute_proxy_calldata(
    calldata: &[u8],
    target_address: Address,
    proxy_wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    metadata: &str,
    gas_limit: Option<u64>,
) -> Result<String> {
    relayer_execute_proxy_calls(
        &[(target_address, calldata.to_vec())],
        proxy_wallet,
        signer,
        builder_key,
        builder_secret,
        builder_passphrase,
        relayer_url,
        metadata,
        gas_limit,
    )
    .await
}

pub(crate) async fn relayer_execute_proxy_calls(
    calls: &[(Address, Vec<u8>)],
    proxy_wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    metadata: &str,
    gas_limit: Option<u64>,
) -> Result<String> {
    if calls.is_empty() {
        anyhow::bail!("relayer_execute_proxy_calls: empty calls");
    }
    let client = reqwest::Client::new();
    let eoa = signer.address();
    let base = relayer_url.trim_end_matches('/');

    let (relay, nonce) = get_relay_payload(&client, base, eoa).await?;
    let call_refs: Vec<(Address, &[u8])> = calls.iter().map(|(a, d)| (*a, d.as_slice())).collect();
    let proxy_data = encode_proxy_calls(&call_refs);
    let base_gas = gas_limit.unwrap_or_else(|| {
        env::var("MERGE_PROXY_GAS_LIMIT")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(PROXY_DEFAULT_GAS)
    });
    let gas_limit = base_gas.saturating_mul(calls.len() as u64).max(base_gas);

    if env::var("MERGE_PROXY_TO").map(|s| s.trim().eq_ignore_ascii_case("PROXY_WALLET")).unwrap_or(false) {
        info!("ℹ️ MERGE_PROXY_TO=PROXY_WALLET ignored, using to=PROXY_FACTORY");
    }
    let to = PROXY_FACTORY;
    let struct_hash = create_struct_hash(eoa, to, &proxy_data, 0, 0, gas_limit, &nonce, RELAY_HUB, relay);
    let to_sign = eip191_hash(struct_hash);
    let sig = signer.sign_hash(&to_sign).await.map_err(|e| anyhow::anyhow!("EOA signing failed: {}", e))?;
    let mut sig_bytes = sig.as_bytes().to_vec();
    if sig_bytes.len() == 65 && (sig_bytes[64] == 0 || sig_bytes[64] == 1) {
        sig_bytes[64] += 27;
    }
    let signature_hex = to_hex_0x(&sig_bytes);

    let signature_params = serde_json::json!({
        "gasPrice": "0",
        "gasLimit": gas_limit.to_string(),
        "relayerFee": "0",
        "relayHub": format!("{:#x}", RELAY_HUB),
        "relay": format!("{:#x}", relay)
    });
    let body = serde_json::json!({
        "from": format!("{:#x}", eoa),
        "to": format!("{:#x}", to),
        "proxyWallet": format!("{:#x}", proxy_wallet),
        "data": to_hex_0x(&proxy_data),
        "nonce": nonce,
        "signature": signature_hex,
        "signatureParams": signature_params,
        "type": "PROXY",
        "metadata": metadata
    });
    let body_str = serde_json::to_string(&body)?;

    let path = RELAYER_SUBMIT;
    let method = "POST";
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let secret_b64 = builder_secret
        .trim()
        .replace('-', "+")
        .replace('_', "/");
    let secret_bytes = base64::engine::general_purpose::STANDARD
        .decode(&secret_b64)
        .map_err(|e| anyhow::anyhow!("POLY_BUILDER_SECRET base64 decode failed: {}", e))?;
    let sig_hmac = build_hmac_signature(&secret_bytes, timestamp, method, path, &body_str);

    let url = format!("{}{}", base, path);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("POLY_BUILDER_API_KEY", builder_key.trim())
        .header("POLY_BUILDER_TIMESTAMP", timestamp.to_string())
        .header("POLY_BUILDER_PASSPHRASE", builder_passphrase.trim())
        .header("POLY_BUILDER_SIGNATURE", sig_hmac)
        .body(body_str)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("Relayer request failed status={} body={}", status, text);
    }
    parse_relayer_tx_hash(&text)
}

pub(crate) fn parse_relayer_tx_hash(text: &str) -> Result<String> {
    let json: serde_json::Value = serde_json::from_str(text)?;
    let hash = json
        .get("transactionHash")
        .or_else(|| json.get("transaction_hash"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(hash.unwrap_or_else(|| text.to_string()))
}

/// Submit a pre-built relayer JSON body with builder HMAC auth.
pub(crate) async fn relayer_submit_authed(
    body: serde_json::Value,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
) -> Result<String> {
    let client = reqwest::Client::new();
    let base = relayer_url.trim_end_matches('/');
    let path = RELAYER_SUBMIT;
    let method = "POST";
    let body_str = serde_json::to_string(&body)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let secret_b64 = builder_secret
        .trim()
        .replace('-', "+")
        .replace('_', "/");
    let secret_bytes = base64::engine::general_purpose::STANDARD
        .decode(&secret_b64)
        .map_err(|e| anyhow::anyhow!("POLY_BUILDER_SECRET base64 decode failed: {}", e))?;
    let sig_hmac = build_hmac_signature(&secret_bytes, timestamp, method, path, &body_str);

    let url = format!("{}{}", base, path);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("POLY_BUILDER_API_KEY", builder_key.trim())
        .header("POLY_BUILDER_TIMESTAMP", timestamp.to_string())
        .header("POLY_BUILDER_PASSPHRASE", builder_passphrase.trim())
        .header("POLY_BUILDER_SIGNATURE", sig_hmac)
        .body(body_str)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("Relayer request failed status={} body={}", status, text);
    }
    parse_relayer_tx_hash(&text)
}

// IGnosisSafe from sol! above, used by merge/redeem/withdraw via crate::proxy_relay::IGnosisSafe

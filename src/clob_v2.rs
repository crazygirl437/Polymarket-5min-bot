//! Polymarket V2 CLOB client factory (shared by main bot and test binaries).

use anyhow::Result;
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use polymarket_client_sdk_v2::clob::types::SignatureType;
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk_v2::types::Address;
use polymarket_client_sdk_v2::POLYGON;
use std::str::FromStr;

pub const CLOB_API_URL_DEFAULT: &str = "https://clob.polymarket.com";

pub type AuthenticatedClobClient = Client<
    polymarket_client_sdk_v2::auth::state::Authenticated<
        polymarket_client_sdk_v2::auth::Normal,
    >,
>;

/// Parse V2 CLOB signature type from env string.
///
/// Most V2 accounts (email/Magic and browser wallet) use `Poly1271` (deposit wallet).
/// Legacy `Proxy` applies only when Settings funder equals ProxyFactory CREATE2 derive.
pub fn parse_signature_type(s: &str) -> SignatureType {
    match s.trim().to_lowercase().as_str() {
        "proxy" | "magic" | "email" => SignatureType::Proxy,
        "gnosissafe" | "safe" => SignatureType::GnosisSafe,
        "poly1271" | "deposit" | "deposit_wallet" | "3" => SignatureType::Poly1271,
        "eoa" | "0" => SignatureType::Eoa,
        _ => SignatureType::Poly1271,
    }
}

/// Build an authenticated V2 CLOB client (EIP-712 domain v2 / pUSD).
pub async fn create_authenticated_clob_client(
    private_key: &str,
    clob_api_url: &str,
    funder_address: Option<Address>,
    signature_type: SignatureType,
) -> Result<AuthenticatedClobClient> {
    if !matches!(signature_type, SignatureType::Eoa) && funder_address.is_none() {
        anyhow::bail!(
            "POLYMARKET_PROXY_ADDRESS (deposit wallet / proxy) is required for {:?} orders",
            signature_type
        );
    }

    let signer = LocalSigner::from_str(private_key)
        .map_err(|e| anyhow::anyhow!("Invalid private key: {}", e))?
        .with_chain_id(Some(POLYGON));

    let clob_config = ClobConfig::builder().use_server_time(true).build();
    let mut auth_builder = Client::new(clob_api_url, clob_config)?
        .authentication_builder(&signer);

    if let Some(funder) = funder_address {
        auth_builder = auth_builder
            .funder(funder)
            .signature_type(signature_type);
    }

    auth_builder
        .authenticate()
        .await
        .map_err(|e| anyhow::anyhow!("CLOB V2 auth failed: {}", e))
}

/// Parse proxy/deposit wallet address from v1 SDK Address string representation.
pub fn v1_address_to_v2(addr: polymarket_client_sdk::types::Address) -> Address {
    addr.to_string().parse().expect("valid address")
}

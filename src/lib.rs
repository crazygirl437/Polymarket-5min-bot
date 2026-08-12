//! polypulse library: shared modules for the main binary and test binaries.

pub mod clob_v2;
mod adapter_auth;
mod deposit_wallet_relay;
mod proxy_relay;
mod pusd_wrap;
mod wallet_kind;

pub use clob_v2::{
    create_authenticated_clob_client, parse_signature_type, v1_address_to_v2,
    AuthenticatedClobClient, CLOB_API_URL_DEFAULT,
};

pub use proxy_relay::{
    CTF_COLLATERAL_ADAPTER, CTF_POLYGON, NEG_RISK_ADAPTER, NEG_RISK_COLLATERAL_ADAPTER,
    PROXY_MERGE_PUSD_GAS, PUSD_POLYGON, RPC_URL_DEFAULT, USDC_POLYGON,
};

pub mod merge;
pub mod positions;
pub mod redeem;
pub mod ui;
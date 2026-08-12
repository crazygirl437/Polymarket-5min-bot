//! CTF Merge module: merge equal YES/NO tokens back to pUSD (V2) or USDC.e (legacy).
//!
//! Supports **Gnosis Safe** (execTransaction), **Magic/Email** (PROXY relayer),
//! and **V2 deposit wallet** (WALLET batch relayer when `SIGNATURE_TYPE=Poly1271`).
//! V2 默认经 CollateralAdapter merge 为 pUSD；USDC.e 抵押持仓 merge 后可自动 wrap 为 pUSD（`MERGE_WRAP_TO_PUSD`，默认开启）。
//! 设 `MERGE_OUTPUT_TOKEN=USDC.e` 可走 legacy 路径。
//! Merge amount is automatically `min(YES_balance, NO_balance)`.

use std::env;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use anyhow::Result;
use polymarket_client_sdk::ctf::types::{CollectionIdRequest, MergePositionsRequest, PositionIdRequest};
use polymarket_client_sdk::ctf::Client;
use polymarket_client_sdk::{contract_config, POLYGON};
use std::str::FromStr as _;
use tracing::{info, warn};

use crate::adapter_auth::{ensure_adapter_approved, encode_set_approval_for_all, wait_relayer_tx};
use crate::deposit_wallet_relay::relayer_execute_deposit_wallet_calldata;
use crate::pusd_wrap::{merge_wrap_to_pusd, wrap_usdce_to_pusd};
use crate::wallet_kind::{classify_wallet, WalletKind};
use crate::proxy_relay::{
    self, derive_proxy_wallet, relayer_execute_proxy_calldata, IGnosisSafe, CTF_COLLATERAL_ADAPTER,
    CTF_POLYGON, PROXY_FACTORY, PROXY_MERGE_PUSD_GAS, PUSD_POLYGON, RELAYER_URL_DEFAULT,
    RPC_URL_DEFAULT, USDC_POLYGON,
};

use alloy::sol;
sol! {
    #[sol(rpc)]
    interface IERC1155Balance {
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

fn merge_to_pusd() -> bool {
    match env::var("MERGE_OUTPUT_TOKEN") {
        Ok(v) => {
            let s = v.trim().to_lowercase();
            s != "usdc.e" && s != "usdc"
        }
        Err(_) => true,
    }
}

fn merge_collateral() -> Address {
    if merge_to_pusd() {
        PUSD_POLYGON
    } else {
        USDC_POLYGON
    }
}

fn merge_target_for_collateral(ctf: Address, collateral: Address) -> Address {
    if collateral == PUSD_POLYGON && merge_to_pusd() {
        CTF_COLLATERAL_ADAPTER
    } else {
        ctf
    }
}

fn merge_gas_for_collateral(collateral: Address) -> Option<u64> {
    if collateral == PUSD_POLYGON && merge_to_pusd() {
        Some(PROXY_MERGE_PUSD_GAS)
    } else {
        None
    }
}

struct ResolvedMerge {
    collateral: Address,
    yes_id: U256,
    no_id: U256,
    b_yes: U256,
    b_no: U256,
}

async fn binary_position_ids(
    client: &Client<impl Provider + Clone>,
    condition_id: B256,
    collateral: Address,
) -> Result<(U256, U256)> {
    let req_col_yes = CollectionIdRequest::builder()
        .parent_collection_id(B256::ZERO)
        .condition_id(condition_id)
        .index_set(U256::from(1))
        .build();
    let req_col_no = CollectionIdRequest::builder()
        .parent_collection_id(B256::ZERO)
        .condition_id(condition_id)
        .index_set(U256::from(2))
        .build();
    let col_yes = client.collection_id(&req_col_yes).await?;
    let col_no = client.collection_id(&req_col_no).await?;
    let pos_yes = client
        .position_id(
            &PositionIdRequest::builder()
                .collateral_token(collateral)
                .collection_id(col_yes.collection_id)
                .build(),
        )
        .await?;
    let pos_no = client
        .position_id(
            &PositionIdRequest::builder()
                .collateral_token(collateral)
                .collection_id(col_no.collection_id)
                .build(),
        )
        .await?;
    Ok((pos_yes.position_id, pos_no.position_id))
}

async fn resolve_merge_balances(
    client: &Client<impl Provider + Clone>,
    prov_read: &impl Provider,
    wallet: Address,
    condition_id: B256,
    asset_hint: Option<(U256, U256)>,
) -> Result<ResolvedMerge> {
    let collaterals = if merge_to_pusd() {
        [PUSD_POLYGON, USDC_POLYGON]
    } else {
        [USDC_POLYGON, PUSD_POLYGON]
    };

    if let Some((yes_id, no_id)) = asset_hint {
        let b_yes = erc1155_balance(prov_read, wallet, yes_id).await?;
        let b_no = erc1155_balance(prov_read, wallet, no_id).await?;
        if b_yes > 0 && b_no > 0 {
            for &collateral in &collaterals {
                let (py, pn) = binary_position_ids(client, condition_id, collateral).await?;
                if py == yes_id && pn == no_id {
                    return Ok(ResolvedMerge {
                        collateral,
                        yes_id,
                        no_id,
                        b_yes,
                        b_no,
                    });
                }
            }
            warn!(
                "API asset IDs have balance but don't match computed position IDs; using preferred collateral {:?}",
                merge_collateral()
            );
            return Ok(ResolvedMerge {
                collateral: merge_collateral(),
                yes_id,
                no_id,
                b_yes,
                b_no,
            });
        }
    }

    for &collateral in &collaterals {
        let (yes_id, no_id) = binary_position_ids(client, condition_id, collateral).await?;
        let b_yes = erc1155_balance(prov_read, wallet, yes_id).await?;
        let b_no = erc1155_balance(prov_read, wallet, no_id).await?;
        if b_yes > 0 && b_no > 0 {
            if collateral == USDC_POLYGON && merge_to_pusd() && merge_wrap_to_pusd() {
                info!("链上持仓为 USDC.e 抵押，merge 后将自动 wrap 为 pUSD");
            } else if collateral == USDC_POLYGON && merge_to_pusd() {
                warn!("链上持仓为 USDC.e 抵押 outcome token，merge 产出 USDC.e（MERGE_WRAP_TO_PUSD=0）");
            }
            return Ok(ResolvedMerge {
                collateral,
                yes_id,
                no_id,
                b_yes,
                b_no,
            });
        }
    }

    let (pref_yes, pref_no) = binary_position_ids(client, condition_id, merge_collateral()).await?;
    let pref_by = erc1155_balance(prov_read, wallet, pref_yes).await.unwrap_or(U256::ZERO);
    let pref_bn = erc1155_balance(prov_read, wallet, pref_no).await.unwrap_or(U256::ZERO);
    if let Some((yes_id, no_id)) = asset_hint {
        let hint_yes = erc1155_balance(prov_read, wallet, yes_id)
            .await
            .unwrap_or(U256::ZERO);
        let hint_no = erc1155_balance(prov_read, wallet, no_id)
            .await
            .unwrap_or(U256::ZERO);
        anyhow::bail!(
            "No mergeable shares: computed YES={} NO={} | API assets yes={} no={} (balances {}/{}) | wallet={:?}",
            pref_by,
            pref_bn,
            yes_id,
            no_id,
            hint_yes,
            hint_no,
            wallet
        );
    }
    anyhow::bail!(
        "No mergeable shares: YES={} NO={} (token ids {} / {}), wallet={:?}",
        pref_by,
        pref_bn,
        pref_yes,
        pref_no,
        wallet
    );
}

fn encode_merge_calldata(req: &MergePositionsRequest) -> Vec<u8> {
    let sel = &keccak256(b"mergePositions(address,bytes32,bytes32,uint256[],uint256)")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(req.collateral_token.as_slice());
    out.extend_from_slice(req.parent_collection_id.as_slice());
    out.extend_from_slice(req.condition_id.as_slice());
    out.extend_from_slice(&U256::from(160u64).to_be_bytes::<32>());
    out.extend_from_slice(&req.amount.to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(req.partition.len()).to_be_bytes::<32>());
    for p in &req.partition {
        out.extend_from_slice(&p.to_be_bytes::<32>());
    }
    out
}

/// Result of a successful merge: on-chain tx hash and verified merged share amount (6-decimal raw).
#[derive(Debug, Clone)]
pub struct MergeResult {
    pub tx_hash: String,
    pub merged_amount: U256,
}

async fn verify_merged<P: Provider>(
    provider: &P,
    wallet: Address,
    pos_yes: U256,
    pos_no: U256,
    before_yes: U256,
    before_no: U256,
    expected: U256,
) -> Result<()> {
    let after_yes = erc1155_balance(provider, wallet, pos_yes).await?;
    let after_no = erc1155_balance(provider, wallet, pos_no).await?;
    let merged_yes = before_yes.saturating_sub(after_yes);
    let merged_no = before_no.saturating_sub(after_no);
    if merged_yes == U256::ZERO || merged_no == U256::ZERO {
        anyhow::bail!(
            "Merge 后 YES/NO 余额未减少 (YES before={} after={} | NO before={} after={})，链上 merge 可能失败",
            before_yes,
            after_yes,
            before_no,
            after_no
        );
    }
    let actual = merged_yes.min(merged_no);
    if actual < expected {
        warn!(
            "Merge 实际数量 {} 小于预期 {}，以链上为准",
            actual, expected
        );
    }
    Ok(())
}

async fn erc1155_balance<P: Provider>(
    provider: &P,
    account: Address,
    token_id: U256,
) -> Result<U256> {
    let erc1155 = IERC1155Balance::new(CTF_POLYGON, provider);
    erc1155
        .balanceOf(account, token_id)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("balanceOf failed: {}", e))
}

/// Shorten long 0x-prefixed hex for logs (reused for compatibility).
pub fn short_hex(s: &str) -> String {
    proxy_relay::short_hex(s)
}

async fn relayer_merge(
    wallet_kind: WalletKind,
    merge_calldata: &[u8],
    merge_to: Address,
    wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    gas_limit: Option<u64>,
) -> Result<String> {
    match wallet_kind {
        WalletKind::DepositWallet => {
            relayer_execute_deposit_wallet_calldata(
                merge_calldata,
                merge_to,
                wallet,
                signer,
                builder_key,
                builder_secret,
                builder_passphrase,
                relayer_url,
                "Merge positions",
            )
            .await
        }
        WalletKind::MagicProxy => {
            relayer_execute_proxy_calldata(
                merge_calldata,
                merge_to,
                wallet,
                signer,
                builder_key,
                builder_secret,
                builder_passphrase,
                relayer_url,
                "Merge positions",
                gas_limit,
            )
            .await
        }
        WalletKind::GnosisSafe => {
            anyhow::bail!("relayer_merge called with GnosisSafe wallet kind");
        }
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

async fn maybe_wrap_merge_output<P: Provider>(
    provider: &P,
    prov_read: &impl Provider,
    wallet: Address,
    collateral: Address,
    merged_amount: U256,
    wallet_kind: WalletKind,
    safe: Option<&IGnosisSafe::IGnosisSafeInstance<P>>,
    signer: &impl alloy::signers::Signer,
    builder: Option<(&str, &str, &str)>,
    relayer_url: &str,
) -> Result<()> {
    if collateral != USDC_POLYGON || !merge_to_pusd() || !merge_wrap_to_pusd() {
        return Ok(());
    }
    let (bk, bs, bp) = match builder {
        Some((k, s, p)) => (Some(k), Some(s), Some(p)),
        None => (None, None, None),
    };
    wrap_usdce_to_pusd(
        provider,
        prov_read,
        wallet,
        merged_amount,
        wallet_kind,
        safe,
        signer,
        bk,
        bs,
        bp,
        relayer_url,
    )
    .await
}

/// Merge maximum available YES+NO to pUSD (default) or USDC.e for given `condition_id` on `wallet`.
/// Pass `asset_hint` (yes_asset, no_asset) from Data API when available for accurate on-chain lookup.
pub async fn merge_max(
    condition_id: B256,
    wallet: Address,
    private_key: &str,
    rpc_url: Option<&str>,
    asset_hint: Option<(U256, U256)>,
) -> Result<MergeResult> {
    let rpc = rpc_url.unwrap_or(RPC_URL_DEFAULT);
    let chain = POLYGON;
    let signer = LocalSigner::from_str(private_key)?.with_chain_id(Some(chain));
    let eoa = signer.address();

    let output = if merge_to_pusd() { "pUSD" } else { "USDC.e" };

    let provider = ProviderBuilder::new().wallet(signer.clone()).connect(rpc).await?;
    let client = Client::new(provider.clone(), chain)?;
    let config = contract_config(chain, false).ok_or_else(|| anyhow::anyhow!("Unsupported chain_id: {}", chain))?;
    let prov_read = ProviderBuilder::new().connect(rpc).await?;
    let ctf = config.conditional_tokens;

    let resolved = resolve_merge_balances(&client, &prov_read, wallet, condition_id, asset_hint).await?;
    let ResolvedMerge {
        collateral,
        yes_id: pos_yes_id,
        no_id: pos_no_id,
        b_yes,
        b_no,
    } = resolved;

    let merge_amount = b_yes.min(b_no);
    if merge_amount == U256::ZERO {
        anyhow::bail!("No mergeable shares: YES={} NO={}, at least one is 0.", b_yes, b_no);
    }
    info!(
        "🔄 Merge amount: {} ({}) | wallet={:?} | collateral={:?} | target={}",
        merge_amount,
        merge_amount / U256::from(1_000_000),
        wallet,
        collateral,
        output
    );

    let merge_req = MergePositionsRequest::for_binary_market(collateral, condition_id, merge_amount);
    let merge_calldata = encode_merge_calldata(&merge_req);
    let merge_to = merge_target_for_collateral(ctf, collateral);
    let gas_limit = merge_gas_for_collateral(collateral);

    let code = provider.get_code_at(wallet).await.unwrap_or_default();
    let wallet_kind = classify_wallet(code.len());

    match wallet_kind {
        WalletKind::DepositWallet | WalletKind::MagicProxy => {
            if matches!(wallet_kind, WalletKind::MagicProxy) {
                let derived = derive_proxy_wallet(eoa, PROXY_FACTORY);
                let try_anyway = env::var("MERGE_TRY_ANYWAY")
                    .map(|s| s.trim() == "1" || s.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                if derived != wallet {
                    if !try_anyway {
                        anyhow::bail!(
                            "POLYMARKET_PROXY_ADDRESS ({:?}) does not match ProxyFactory CREATE2 derive ({:?}). \
                             Use Polymarket web merge or set MERGE_TRY_ANYWAY=1 to force.",
                            wallet,
                            derived
                        );
                    }
                    warn!("MERGE_TRY_ANYWAY=1: derive != proxy, still sending Relayer request.");
                }
            }
            let builder_key = env::var("POLY_BUILDER_API_KEY").ok();
            let builder_secret = env::var("POLY_BUILDER_SECRET").ok();
            let builder_passphrase = env::var("POLY_BUILDER_PASSPHRASE").ok();
            let relayer_url =
                env::var("RELAYER_URL").unwrap_or_else(|_| RELAYER_URL_DEFAULT.to_string());
            match (
                builder_key.as_deref(),
                builder_secret.as_deref(),
                builder_passphrase.as_deref(),
            ) {
                (Some(k), Some(s), Some(p)) => {
                    if collateral == PUSD_POLYGON && merge_to_pusd() {
                        ensure_adapter_approved(
                            &provider,
                            wallet,
                            CTF_COLLATERAL_ADAPTER,
                            &signer,
                            k,
                            s,
                            p,
                            &relayer_url,
                        )
                        .await?;
                    }
                    let out = relayer_merge(
                        wallet_kind,
                        &merge_calldata,
                        merge_to,
                        wallet,
                        &signer,
                        k,
                        s,
                        p,
                        &relayer_url,
                        gas_limit,
                    )
                    .await?;
                    wait_relayer_tx(&provider, &out).await?;
                    verify_merged(
                        &prov_read,
                        wallet,
                        pos_yes_id,
                        pos_no_id,
                        b_yes,
                        b_no,
                        merge_amount,
                    )
                    .await?;
                    let after_yes = erc1155_balance(&prov_read, wallet, pos_yes_id).await?;
                    let after_no = erc1155_balance(&prov_read, wallet, pos_no_id).await?;
                    let merged_amount = b_yes
                        .saturating_sub(after_yes)
                        .min(b_no.saturating_sub(after_no));
                    maybe_wrap_merge_output(
                        &provider,
                        &prov_read,
                        wallet,
                        collateral,
                        merged_amount,
                        wallet_kind,
                        None,
                        &signer,
                        Some((k, s, p)),
                        &relayer_url,
                    )
                    .await?;
                    info!("✅ Relayer merge confirmed: {} | merged={}", out, merged_amount);
                    return Ok(MergeResult {
                        tx_hash: out,
                        merged_amount,
                    });
                }
                _ => anyhow::bail!(
                    "Relayer merge requires POLY_BUILDER_API_KEY, POLY_BUILDER_SECRET, POLY_BUILDER_PASSPHRASE.",
                ),
            }
        }
        WalletKind::GnosisSafe => {}
    }

    let safe = IGnosisSafe::new(wallet, provider.clone());
    if collateral == PUSD_POLYGON
        && merge_to_pusd()
        && !crate::adapter_auth::is_adapter_approved(
            &provider,
            CTF_POLYGON,
            wallet,
            CTF_COLLATERAL_ADAPTER,
        )
        .await?
    {
        let approve_calldata = encode_set_approval_for_all(CTF_COLLATERAL_ADAPTER, true);
        let tx = safe_exec_call(&safe, &signer, CTF_POLYGON, approve_calldata).await?;
        info!("✅ Safe setApprovalForAll tx: {:#x}", tx);
    }

    let tx_hash_out = safe_exec_call(&safe, &signer, merge_to, merge_calldata).await?;
    verify_merged(
        &prov_read,
        wallet,
        pos_yes_id,
        pos_no_id,
        b_yes,
        b_no,
        merge_amount,
    )
    .await?;
    let after_yes = erc1155_balance(&prov_read, wallet, pos_yes_id).await?;
    let after_no = erc1155_balance(&prov_read, wallet, pos_no_id).await?;
    let merged_amount = b_yes
        .saturating_sub(after_yes)
        .min(b_no.saturating_sub(after_no));
    let relayer_url = env::var("RELAYER_URL").unwrap_or_else(|_| RELAYER_URL_DEFAULT.to_string());
    maybe_wrap_merge_output(
        &provider,
        &prov_read,
        wallet,
        collateral,
        merged_amount,
        wallet_kind,
        Some(&safe),
        &signer,
        None,
        &relayer_url,
    )
    .await?;
    info!("✅ Merge success (Safe) tx: {:#x} | merged={}", tx_hash_out, merged_amount);
    Ok(MergeResult {
        tx_hash: format!("{:#x}", tx_hash_out),
        merged_amount,
    })
}

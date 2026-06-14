//! Wallet execution routing for relayer / Safe on-chain ops.

use crate::deposit_wallet_relay::use_deposit_wallet_relayer;

#[derive(Debug, Clone, Copy)]
pub enum WalletKind {
    DepositWallet,
    MagicProxy,
    GnosisSafe,
}

pub fn classify_wallet(code_len: usize) -> WalletKind {
    if use_deposit_wallet_relayer() {
        WalletKind::DepositWallet
    } else if code_len < 150 {
        WalletKind::MagicProxy
    } else {
        WalletKind::GnosisSafe
    }
}

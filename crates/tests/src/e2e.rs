//! End-to-end tests for Namada binaries
//!
//! By default, these tests will run in release mode. This can be disabled
//! by setting environment variable `NAMADA_E2E_DEBUG=true`. For debugging,
//! you'll typically also want to set `RUST_BACKTRACE=1`, e.g.:
//!
//! ```ignore,shell
//! NAMADA_E2E_DEBUG=true RUST_BACKTRACE=1 cargo test e2e -- --test-threads=1 --nocapture
//! ```
//!
//! To keep the temporary files created by a test, use env var
//! `NAMADA_E2E_KEEP_TEMP=true`.

#[cfg(DISABLED_UNTIL_ERC20_WHITELISTS_IMPLEMENTED)]
pub mod eth_bridge_tests;
pub mod helpers;
pub mod ibc_tests;
pub mod ledger_tests;
pub mod setup;
pub mod wallet_tests;

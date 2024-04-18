/// A tx that doesn't do anything.
#[cfg(feature = "tx_no_op")]
pub mod main {
    use namada_tx_prelude::*;

    #[transaction(gas = 1000)]
    fn apply_tx(_ctx: &mut Ctx, _tx_data: Tx) -> TxResult {
        Ok(())
    }
}

/// A tx that fails every time.
#[cfg(feature = "tx_fail")]
pub mod main {
    use namada_tx_prelude::*;

    #[transaction(gas = 1000)]
    fn apply_tx(_ctx: &mut Ctx, _tx_data: Tx) -> TxResult {
        Err(Error::SimpleMessage("failed tx"))
    }
}

/// A tx that allocates a memory of size given from the `tx_data: usize`.
#[cfg(feature = "tx_memory_limit")]
pub mod main {
    use namada_tx_prelude::*;

    #[transaction(gas = 1000)]
    fn apply_tx(_ctx: &mut Ctx, tx_data: Tx) -> TxResult {
        let len = usize::try_from_slice(&tx_data.data().as_ref().unwrap()[..])
            .unwrap();
        log_string(format!("allocate len {}", len));
        let bytes: Vec<u8> = vec![6_u8; len];
        // use the variable to prevent it from compiler optimizing it away
        log_string(format!("{:?}", &bytes[..8]));
        Ok(())
    }
}

/// A tx to be used as proposal_code
#[cfg(feature = "tx_proposal_code")]
pub mod main {
    use namada_tx_prelude::*;

    #[transaction(gas = 1000)]
    fn apply_tx(ctx: &mut Ctx, _tx_data: Tx) -> TxResult {
        // governance
        let target_key = gov_storage::keys::get_min_proposal_grace_epoch_key();
        ctx.write(&target_key, 9_u64)?;

        // parameters
        let target_key = parameters_storage::get_tx_allowlist_storage_key();
        ctx.write(&target_key, vec!["hash"])?;
        Ok(())
    }
}

/// A tx to be used as proposal_code for changing shielded rewards
#[cfg(feature = "tx_proposal_masp_reward")]
pub mod main {
    use std::str::FromStr;

    use dec::Dec;
    use namada_tx_prelude::*;

    #[transaction(gas = 1000)]
    fn apply_tx(ctx: &mut Ctx, _tx_data: Tx) -> TxResult {
        let native_token = ctx.get_native_token()?;
        let shielded_rewards_key =
            token::storage_key::masp_max_reward_rate_key(&native_token);

        ctx.write(&shielded_rewards_key, Dec::from_str("0.05").unwrap())?;

        Ok(())
    }
}

/// A tx to be used as proposal_code to set the inflation params for a token
/// over IBC
#[cfg(feature = "tx_proposal_ibc_token_inflation")]
pub mod main {
    use std::str::FromStr;

    use dec::Dec;
    use namada_tx_prelude::*;

    // Denom of tokens over IBC is always zero
    const IBC_TOKEN_DENOM: u8 = 0;
    const CHANNEL_ID: &str = "channel-0";
    const BASE_TOKEN: &str = "tnam1qyvfwdkz8zgs9n3qn9xhp8scyf8crrxwuq26r6gy";

    #[transaction(gas = 1000)]
    fn apply_tx(ctx: &mut Ctx, _tx_data: Tx) -> TxResult {
        let ibc_denom = format!("transfer/{CHANNEL_ID}/{BASE_TOKEN}");
        let ibc_token = ibc::ibc_token(&ibc_denom);

        let shielded_token_last_inflation_key =
            token::storage_key::masp_last_inflation_key(&ibc_token);
        let shielded_token_last_locked_amount_key =
            token::storage_key::masp_last_locked_amount_key(&ibc_token);
        let shielded_token_max_rewards_key =
            token::storage_key::masp_max_reward_rate_key(&ibc_token);
        let shielded_token_target_locked_amount_key =
            token::storage_key::masp_locked_amount_target_key(&ibc_token);
        let shielded_token_kp_gain_key =
            token::storage_key::masp_kp_gain_key(&ibc_token);
        let shielded_token_kd_gain_key =
            token::storage_key::masp_kd_gain_key(&ibc_token);

        let token_map_key = token::storage_key::masp_token_map_key();
        let mut token_map: masp::TokenMap = ctx.read(&token_map_key)?.unwrap_or_default();
        token_map.insert(ibc_denom, ibc_token);
        ctx.write(&token_map_key, token_map)?;

        ctx.write(
            &shielded_token_last_inflation_key,
            token::Amount::zero(),
        )?;
        ctx.write(
            &shielded_token_last_locked_amount_key,
            token::Amount::zero(),
        )?;
        ctx.write(
            &shielded_token_max_rewards_key,
            Dec::from_str("0.01").unwrap(),
        )?;
        ctx.write(
            &shielded_token_target_locked_amount_key,
            token::Amount::from_uint(1_000_000_000, IBC_TOKEN_DENOM).unwrap(),
        )?;
        ctx.write(
            &shielded_token_kp_gain_key,
            Dec::from_str("120000").unwrap(),
        )?;
        ctx.write(
            &shielded_token_kd_gain_key,
            Dec::from_str("120000").unwrap(),
        )?;
        Ok(())
    }
}

/// A tx that attempts to read the given key from storage.
#[cfg(feature = "tx_read_storage_key")]
pub mod main {
    use namada_tx_prelude::*;

    #[transaction(gas = 1000)]
    fn apply_tx(ctx: &mut Ctx, tx_data: Tx) -> TxResult {
        // Allocates a memory of size given from the `tx_data (usize)`
        let key =
            storage::Key::try_from_slice(&tx_data.data().as_ref().unwrap()[..])
                .unwrap();
        log_string(format!("key {}", key));
        let _result: Vec<u8> = ctx.read(&key)?.unwrap();
        Ok(())
    }
}

/// A tx that attempts to write arbitrary data to the given key
#[cfg(feature = "tx_write")]
pub mod main {
    use namada_test_utils::tx_data::TxWriteData;
    use namada_tx_prelude::{
        log_string, transaction, BorshDeserialize, Ctx, StorageRead,
        StorageWrite, Tx, TxEnv, TxResult,
    };

    const TX_NAME: &str = "tx_write";

    fn log(msg: &str) {
        log_string(format!("[{}] {}", TX_NAME, msg))
    }

    fn fatal(msg: &str, err: impl std::error::Error) -> ! {
        log(&format!("ERROR: {} - {:?}", msg, err));
        panic!()
    }

    fn fatal_msg(msg: &str) -> ! {
        log(msg);
        panic!()
    }

    #[transaction(gas = 1000)]
    fn apply_tx(ctx: &mut Ctx, tx_data: Tx) -> TxResult {
        let signed = tx_data;
        let data = match signed.data() {
            Some(data) => {
                log(&format!("got data ({} bytes)", data.len()));
                data
            }
            None => {
                fatal_msg("no data provided");
            }
        };
        let TxWriteData { key, value } =
            match TxWriteData::try_from_slice(&data[..]) {
                Ok(write_op) => {
                    log(&format!(
                        "parsed WriteOp to key {} ({} bytes)",
                        &write_op.key,
                        &write_op.value.len(),
                    ));
                    write_op
                }
                Err(error) => fatal("deserializing WriteOp", error),
            };
        let existing_value: Option<String> = ctx.read(&key)?;
        match existing_value {
            Some(existing_value) => {
                log(&format!("already present value is {}", existing_value));
            }
            None => {
                log("no already present value");
            }
        }
        log(&format!("attempting to write new value to key {}", key));
        // using `ctx.write_bytes` instead of `ctx.write` here, as we want to
        // write the actual bytes, not a Borsh-serialization of a `Vec<u8>`
        ctx.write_bytes(&key, &value[..])?;
        Ok(())
    }
}

/// A VP that always returns `true`.
#[cfg(feature = "vp_always_true")]
pub mod main {
    use namada_vp_prelude::*;

    #[validity_predicate(gas = 1000)]
    fn validate_tx(
        _ctx: &Ctx,
        _tx_data: Tx,
        _addr: Address,
        _keys_changed: BTreeSet<storage::Key>,
        _verifiers: BTreeSet<Address>,
    ) -> VpResult {
        accept()
    }
}

/// A VP that always returns `false`.
#[cfg(feature = "vp_always_false")]
pub mod main {
    use namada_vp_prelude::*;

    #[validity_predicate(gas = 1000)]
    fn validate_tx(
        _ctx: &Ctx,
        _tx_data: Tx,
        _addr: Address,
        _keys_changed: BTreeSet<storage::Key>,
        _verifiers: BTreeSet<Address>,
    ) -> VpResult {
        reject()
    }
}

/// A VP that runs the VP given in `tx_data` via `eval`. It returns the result
/// of `eval`.
#[cfg(feature = "vp_eval")]
pub mod main {
    use namada_vp_prelude::*;

    #[validity_predicate(gas = 1000)]
    fn validate_tx(
        ctx: &Ctx,
        tx_data: Tx,
        _addr: Address,
        _keys_changed: BTreeSet<storage::Key>,
        _verifiers: BTreeSet<Address>,
    ) -> VpResult {
        use namada_tx_prelude::transaction::eval_vp::EvalVp;
        let EvalVp {
            vp_code_hash,
            input,
        }: EvalVp =
            EvalVp::try_from_slice(&tx_data.data().as_ref().unwrap()[..])
                .unwrap();
        ctx.eval(vp_code_hash, input)
    }
}

// A VP that allocates a memory of size given from the `tx_data: usize`.
// Returns `true`, if the allocation is within memory limits.
#[cfg(feature = "vp_memory_limit")]
pub mod main {
    use namada_vp_prelude::*;

    #[validity_predicate(gas = 1000)]
    fn validate_tx(
        _ctx: &Ctx,
        tx_data: Tx,
        _addr: Address,
        _keys_changed: BTreeSet<storage::Key>,
        _verifiers: BTreeSet<Address>,
    ) -> VpResult {
        let len = usize::try_from_slice(&tx_data.data().as_ref().unwrap()[..])
            .unwrap();
        log_string(format!("allocate len {}", len));
        let bytes: Vec<u8> = vec![6_u8; len];
        // use the variable to prevent it from compiler optimizing it away
        log_string(format!("{:?}", &bytes[..8]));
        accept()
    }
}

/// A VP that attempts to read the given key from storage (state prior to tx
/// execution). Returns `true`, if the allocation is within memory limits.
#[cfg(feature = "vp_read_storage_key")]
pub mod main {
    use namada_vp_prelude::*;

    #[validity_predicate(gas = 1000)]
    fn validate_tx(
        ctx: &Ctx,
        tx_data: Tx,
        _addr: Address,
        _keys_changed: BTreeSet<storage::Key>,
        _verifiers: BTreeSet<Address>,
    ) -> VpResult {
        // Allocates a memory of size given from the `tx_data (usize)`
        let key =
            storage::Key::try_from_slice(&tx_data.data().as_ref().unwrap()[..])
                .unwrap();
        log_string(format!("key {}", key));
        let _result: Vec<u8> = ctx.read_pre(&key)?.unwrap();
        accept()
    }
}

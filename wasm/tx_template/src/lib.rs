use namada_tx_prelude::*;

#[transaction(gas = 1000)]
fn apply_tx(_ctx: &mut Ctx, tx_data: Tx) -> TxResult {
    log_string(format!("apply_tx called with data: {:#?}", tx_data));
    Ok(())
}

#[cfg(test)]
mod tests {
    use namada_tests::tx::*;

    use super::*;

    /// An example test, checking that this transaction performs no storage
    /// modifications.
    #[test]
    fn test_no_op_transaction() {
        // The environment must be initialized first
        tx_host_env::init();

        let tx = Tx::from_type(TxType::Raw);
        apply_tx(ctx(), tx).unwrap();

        let env = tx_host_env::take();
        assert!(env.all_touched_storage_keys().is_empty());
    }
}

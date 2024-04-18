use std::collections::BTreeMap;

pub(super) mod eth_bridge;

use borsh::BorshDeserialize;
use borsh_ext::BorshSerializeExt;
use masp_primitives::asset_type::AssetType;
use masp_primitives::merkle_tree::MerklePath;
use masp_primitives::sapling::Node;
use namada_account::{Account, AccountPublicKeysMap};
use namada_core::address::Address;
use namada_core::dec::Dec;
use namada_core::hash::Hash;
use namada_core::hints;
use namada_core::masp::TokenMap;
use namada_core::storage::{
    self, BlockHeight, BlockResults, Epoch, KeySeg, PrefixValue,
};
use namada_core::token::{Denomination, MaspDigitPos};
use namada_core::uint::Uint;
use namada_state::{DBIter, LastBlock, StateRead, StorageHasher, DB};
use namada_storage::{ResultExt, StorageRead};
use namada_token::storage_key::masp_token_map_key;
#[cfg(any(test, feature = "async-client"))]
use namada_tx::data::TxResult;

use self::eth_bridge::{EthBridge, ETH_BRIDGE};
use crate::events::log::dumb_queries;
use crate::events::{Event, EventType};
use crate::ibc::core::host::types::identifiers::{
    ChannelId, ClientId, PortId, Sequence,
};
use crate::masp::MaspTokenRewardData;
use crate::queries::types::{RequestCtx, RequestQuery};
use crate::queries::{require_latest_height, EncodedResponseQuery};
use crate::tendermint::merkle::proof::ProofOps;

type ConversionWithoutPath = (
    Address,
    Denomination,
    MaspDigitPos,
    Epoch,
    masp_primitives::transaction::components::I128Sum,
);

type Conversion = (
    Address,
    Denomination,
    MaspDigitPos,
    Epoch,
    masp_primitives::transaction::components::I128Sum,
    MerklePath<Node>,
);

router! {SHELL,
    // Shell provides storage read access, block metadata and can dry-run a tx

    // Ethereum bridge specific queries
    ( "eth_bridge" ) = (sub ETH_BRIDGE),

    // Epoch of the last committed block
    ( "epoch" ) -> Epoch = epoch,

    // The address of the native token
    ( "native_token" ) -> Address = native_token,

    // Epoch of the input block height
    ( "epoch_at_height" / [height: BlockHeight]) -> Option<Epoch> = epoch_at_height,

    // Query the last committed block
    ( "last_block" ) -> Option<LastBlock> = last_block,

    // First block height of the current epoch
    ( "first_block_height_of_current_epoch" ) -> BlockHeight = first_block_height_of_current_epoch,

    // Raw storage access - read value
    ( "value" / [storage_key: storage::Key] )
        -> Vec<u8> = (with_options storage_value),

    // Dry run a transaction
    ( "dry_run_tx" ) -> TxResult = (with_options dry_run_tx),

    // Raw storage access - prefix iterator
    ( "prefix" / [storage_key: storage::Key] )
        -> Vec<PrefixValue> = (with_options storage_prefix),

    // Raw storage access - is given storage key present?
    ( "has_key" / [storage_key: storage::Key] )
        -> bool = storage_has_key,

    // Conversion state access - read conversion
    ( "conv" / [asset_type: AssetType] ) -> Option<Conversion> = read_conversion,

    // Conversion state access - read conversion
    ( "conversions" ) -> BTreeMap<AssetType, ConversionWithoutPath> = read_conversions,

    // Conversion state access - read conversion
    ( "masp_reward_tokens" ) -> Vec<MaspTokenRewardData> = masp_reward_tokens,

    // Block results access - read bit-vec
    ( "results" ) -> Vec<BlockResults> = read_results,

    // was the transaction accepted?
    ( "accepted" / [tx_hash: Hash] ) -> Option<Event> = accepted,

    // was the transaction applied?
    ( "applied" / [tx_hash: Hash] ) -> Option<Event> = applied,

    // Query account subspace
    ( "account" / [owner: Address] ) -> Option<Account> = account,

    // Query public key revealad
    ( "revealed" / [owner: Address] ) -> bool = revealed,

    // IBC UpdateClient event
    ( "ibc_client_update" / [client_id: ClientId] / [consensus_height: BlockHeight] ) -> Option<Event> = ibc_client_update,

    // IBC packet event
    ( "ibc_packet" / [event_type: EventType] / [source_port: PortId] / [source_channel: ChannelId] / [destination_port: PortId] / [destination_channel: ChannelId] / [sequence: Sequence]) -> Option<Event> = ibc_packet,
}

// Handlers:

fn dry_run_tx<D, H, V, T>(
    _ctx: RequestCtx<'_, D, H, V, T>,
    _request: &RequestQuery,
) -> namada_storage::Result<EncodedResponseQuery>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    unimplemented!("Dry running tx requires \"wasm-runtime\" feature.")
}

/// Query to read block results from storage
pub fn read_results<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<Vec<BlockResults>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let (iter, _gas) = ctx.state.db_iter_results();
    let mut results = vec![
        BlockResults::default();
        ctx.state.in_mem().block.height.0 as usize + 1
    ];
    for (key, value, _gas) in iter {
        let key = u64::parse(key.clone()).map_err(|_| {
            namada_storage::Error::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("expected integer for block height {}", key),
            ))
        })?;
        let value = BlockResults::try_from_slice(&value).map_err(|_| {
            namada_storage::Error::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected BlockResults bytes",
            ))
        })?;
        let idx: usize = key.try_into().map_err(|_| {
            namada_storage::Error::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected block height to fit into usize",
            ))
        })?;
        results[idx] = value;
    }
    Ok(results)
}

/// Query to read the conversion state
fn read_conversions<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<BTreeMap<AssetType, ConversionWithoutPath>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    Ok(ctx
        .state
        .in_mem()
        .conversion_state
        .assets
        .iter()
        .map(
            |(&asset_type, ((ref addr, denom, digit), epoch, ref conv, _))| {
                (
                    asset_type,
                    (addr.clone(), *denom, *digit, *epoch, conv.clone().into()),
                )
            },
        )
        .collect())
}

/// Query to read a conversion from storage
fn read_conversion<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    asset_type: AssetType,
) -> namada_storage::Result<Option<Conversion>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    // Conversion values are constructed on request
    if let Some(((addr, denom, digit), epoch, conv, pos)) =
        ctx.state.in_mem().conversion_state.assets.get(&asset_type)
    {
        Ok(Some((
            addr.clone(),
            *denom,
            *digit,
            *epoch,
            Into::<masp_primitives::transaction::components::I128Sum>::into(
                conv.clone(),
            ),
            ctx.state.in_mem().conversion_state.tree.path(*pos),
        )))
    } else {
        Ok(None)
    }
}

/// Query to read the tokens that earn masp rewards.
fn masp_reward_tokens<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<Vec<MaspTokenRewardData>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let token_map_key = masp_token_map_key();
    let token_map: TokenMap =
        ctx.state.read(&token_map_key)?.unwrap_or_default();
    let mut data = Vec::<MaspTokenRewardData>::new();
    for (name, token) in token_map {
        let max_reward_rate = ctx
            .state
            .read::<Dec>(&namada_token::storage_key::masp_max_reward_rate_key(
                &token,
            ))?
            .ok_or_else(|| {
                namada_storage::Error::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "Did not find max reward rate set for token {} ({})",
                        &name, &token
                    ),
                ))
            })?;
        let kd_gain = ctx
            .state
            .read::<Dec>(&namada_token::storage_key::masp_kd_gain_key(&token))?
            .ok_or_else(|| {
                namada_storage::Error::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "Did not find kd gain set for token {} ({})",
                        &name, &token
                    ),
                ))
            })?;
        let kp_gain = ctx
            .state
            .read::<Dec>(&namada_token::storage_key::masp_kp_gain_key(&token))?
            .ok_or_else(|| {
                namada_storage::Error::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "Did not find kp gain set for token {} ({})",
                        &name, &token
                    ),
                ))
            })?;
        let locked_amount_target = ctx
            .state
            .read::<Uint>(
                &namada_token::storage_key::masp_locked_amount_target_key(
                    &token,
                ),
            )?
            .ok_or_else(|| {
                namada_storage::Error::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "Did not find target locked ratio set for token {} \
                         ({})",
                        &name, &token
                    ),
                ))
            })?;

        data.push(MaspTokenRewardData {
            name,
            address: token,
            max_reward_rate,
            kp_gain,
            kd_gain,
            locked_amount_target,
        });
    }
    Ok(data)
}

fn epoch<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<Epoch>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let data = ctx.state.in_mem().last_epoch;
    Ok(data)
}

fn native_token<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<Address>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let data = ctx.state.in_mem().native_token.clone();
    Ok(data)
}

fn epoch_at_height<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    height: BlockHeight,
) -> namada_storage::Result<Option<Epoch>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    Ok(ctx.state.in_mem().block.pred_epochs.get_epoch(height))
}

fn last_block<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<Option<LastBlock>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    Ok(ctx.state.in_mem().last_block.clone())
}

fn first_block_height_of_current_epoch<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
) -> namada_storage::Result<BlockHeight>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    ctx.state
        .in_mem()
        .block
        .pred_epochs
        .first_block_heights
        .last()
        .ok_or(namada_storage::Error::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "The pred_epochs is unexpectedly empty",
        )))
        .cloned()
}

/// Returns data with `vec![]` when the storage key is not found. For all
/// borsh-encoded types, it is safe to check `data.is_empty()` to see if the
/// value was found, except for unit - see `fn query_storage_value` in
/// `apps/src/lib/client/rpc.rs` for unit type handling via `storage_has_key`.
fn storage_value<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    request: &RequestQuery,
    storage_key: storage::Key,
) -> namada_storage::Result<EncodedResponseQuery>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let last_committed_height = ctx.state.in_mem().get_last_block_height();
    let queried_height = {
        let height: BlockHeight = request.height.into();
        let is_last_height_query = height.0 == 0;

        if hints::likely(is_last_height_query) {
            last_committed_height
        } else {
            height
        }
    };

    if let Some(past_height_limit) = ctx.storage_read_past_height_limit {
        if queried_height + past_height_limit < last_committed_height {
            return Err(namada_storage::Error::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Cannot query more than {past_height_limit} blocks in the \
                     past (configured via \
                     `shell.storage_read_past_height_limit`)."
                ),
            )));
        }
    }

    match ctx
        .state
        .db_read_with_height(&storage_key, queried_height)
        .into_storage_result()?
    {
        (Some(value), _gas) => {
            let proof = if request.prove {
                let proof = ctx
                    .state
                    .get_existence_proof(&storage_key, &value, queried_height)
                    .into_storage_result()?;
                Some(proof)
            } else {
                None
            };
            Ok(EncodedResponseQuery {
                data: value,
                proof,
                info: Default::default(),
            })
        }
        (None, _gas) => {
            let proof = if request.prove {
                let proof = ctx
                    .state
                    .get_non_existence_proof(&storage_key, queried_height)
                    .into_storage_result()?;
                Some(proof)
            } else {
                None
            };
            Ok(EncodedResponseQuery {
                data: vec![],
                proof,
                info: format!("No value found for key: {}", storage_key),
            })
        }
    }
}

fn storage_prefix<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    request: &RequestQuery,
    storage_key: storage::Key,
) -> namada_storage::Result<EncodedResponseQuery>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    require_latest_height(&ctx, request)?;

    let iter = namada_storage::iter_prefix_bytes(ctx.state, &storage_key)?;
    let data: namada_storage::Result<Vec<PrefixValue>> = iter
        .map(|iter_result| {
            let (key, value) = iter_result?;
            Ok(PrefixValue { key, value })
        })
        .collect();
    let data = data?;
    let proof = if request.prove {
        let queried_height = {
            let last_committed_height =
                ctx.state.in_mem().get_last_block_height();

            let height: BlockHeight = request.height.into();
            let is_last_height_query = height.0 == 0;

            if hints::likely(is_last_height_query) {
                last_committed_height
            } else {
                height
            }
        };
        let mut ops = vec![];
        for PrefixValue { key, value } in &data {
            let mut proof = ctx
                .state
                .get_existence_proof(key, value, queried_height)
                .into_storage_result()?;
            ops.append(&mut proof.ops);
        }
        // ops is not empty in this case
        let proof = ProofOps { ops };
        Some(proof)
    } else {
        None
    };
    let data = data.serialize_to_vec();
    Ok(EncodedResponseQuery {
        data,
        proof,
        ..Default::default()
    })
}

fn storage_has_key<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    storage_key: storage::Key,
) -> namada_storage::Result<bool>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let data = StorageRead::has_key(ctx.state, &storage_key)?;
    Ok(data)
}

fn accepted<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    tx_hash: Hash,
) -> namada_storage::Result<Option<Event>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let matcher = dumb_queries::QueryMatcher::accepted(tx_hash);
    Ok(ctx
        .event_log
        .iter_with_matcher(matcher)
        .by_ref()
        .next()
        .cloned())
}

fn applied<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    tx_hash: Hash,
) -> namada_storage::Result<Option<Event>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let matcher = dumb_queries::QueryMatcher::applied(tx_hash);
    Ok(ctx
        .event_log
        .iter_with_matcher(matcher)
        .by_ref()
        .next()
        .cloned())
}

fn ibc_client_update<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    client_id: ClientId,
    consensus_height: BlockHeight,
) -> namada_storage::Result<Option<Event>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let matcher = dumb_queries::QueryMatcher::ibc_update_client(
        client_id,
        consensus_height,
    );
    Ok(ctx
        .event_log
        .iter_with_matcher(matcher)
        .by_ref()
        .next()
        .cloned())
}

fn ibc_packet<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    event_type: EventType,
    source_port: PortId,
    source_channel: ChannelId,
    destination_port: PortId,
    destination_channel: ChannelId,
    sequence: Sequence,
) -> namada_storage::Result<Option<Event>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let matcher = dumb_queries::QueryMatcher::ibc_packet(
        event_type,
        source_port,
        source_channel,
        destination_port,
        destination_channel,
        sequence,
    );
    Ok(ctx
        .event_log
        .iter_with_matcher(matcher)
        .by_ref()
        .next()
        .cloned())
}

fn account<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    owner: Address,
) -> namada_storage::Result<Option<Account>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let account_exists = namada_account::exists(ctx.state, &owner)?;

    if account_exists {
        let public_keys = namada_account::public_keys(ctx.state, &owner)?;
        let threshold = namada_account::threshold(ctx.state, &owner)?;

        Ok(Some(Account {
            public_keys_map: AccountPublicKeysMap::from_iter(public_keys),
            address: owner,
            threshold: threshold.unwrap_or(1),
        }))
    } else {
        Ok(None)
    }
}

fn revealed<D, H, V, T>(
    ctx: RequestCtx<'_, D, H, V, T>,
    owner: Address,
) -> namada_storage::Result<bool>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let public_keys = namada_account::public_keys(ctx.state, &owner)?;

    Ok(!public_keys.is_empty())
}

#[cfg(test)]
mod test {
    use namada_core::address;
    use namada_token::storage_key::balance_key;

    use crate::queries::RPC;

    #[test]
    fn test_shell_queries_router_paths() {
        let path = RPC.shell().epoch_path();
        assert_eq!("/shell/epoch", path);

        let token_addr = address::testing::established_address_1();
        let owner = address::testing::established_address_2();
        let key = balance_key(&token_addr, &owner);
        let path = RPC.shell().storage_value_path(&key);
        assert_eq!(format!("/shell/value/{}", key), path);

        let path = RPC.shell().dry_run_tx_path();
        assert_eq!("/shell/dry_run_tx", path);

        let path = RPC.shell().storage_prefix_path(&key);
        assert_eq!(format!("/shell/prefix/{}", key), path);

        let path = RPC.shell().storage_has_key_path(&key);
        assert_eq!(format!("/shell/has_key/{}", key), path);
    }
}

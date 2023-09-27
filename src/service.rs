use ckb_chain_spec::consensus::Consensus;
use ckb_jsonrpc_types::{
    BlockNumber, BlockView, Capacity, CellOutput, Cycle, HeaderView, JsonBytes, NodeAddress,
    OutPoint, RemoteNodeProtocol, Script, Transaction, TransactionView, Uint32, Uint64,
};
use ckb_network::{extract_peer_id, NetworkController};
use ckb_systemtime::unix_time_as_millis;
use ckb_traits::HeaderProvider;
use ckb_types::{core, packed, prelude::*, H256};
use jsonrpc_core::{Error, IoHandler, Result};
use jsonrpc_derive::rpc;
use jsonrpc_http_server::{Server, ServerBuilder};
use jsonrpc_server_utils::cors::AccessControlAllowOrigin;
use jsonrpc_server_utils::hosts::DomainsValidation;
use rocksdb::{
    ops::{Get, Iterate},
    Direction, IteratorMode,
};
use serde::{Deserialize, Serialize};
use std::{
    net::ToSocketAddrs,
    sync::{Arc, RwLock},
};

use crate::{
    protocols::{Peers, PendingTxs},
    storage::{
        self, extract_raw_data, Key, KeyPrefix, Storage, StorageWithChainData, LAST_STATE_KEY,
    },
    verify::verify_tx,
};

#[rpc(server)]
pub trait BlockFilterRpc {
    /// curl http://localhost:9000/ -X POST -H "Content-Type: application/json" -d '{"jsonrpc": "2.0", "method":"set_scripts", "params": [{"script": {"code_hash": "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8", "hash_type": "type", "args": "0x50878ce52a68feb47237c29574d82288f58b5d21"}, "block_number": "0x59F74D"}], "id": 1}'
    #[rpc(name = "set_scripts")]
    fn set_scripts(
        &self,
        scripts: Vec<ScriptStatus>,
        command: Option<SetScriptsCommand>,
    ) -> Result<()>;

    #[rpc(name = "get_scripts")]
    fn get_scripts(&self) -> Result<Vec<ScriptStatus>>;

    #[rpc(name = "get_cells")]
    fn get_cells(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>>;

    #[rpc(name = "get_transactions")]
    fn get_transactions(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Tx>>;

    #[rpc(name = "get_cells_capacity")]
    fn get_cells_capacity(&self, search_key: SearchKey) -> Result<CellsCapacity>;
}

#[rpc(server)]
pub trait TransactionRpc {
    #[rpc(name = "send_transaction")]
    fn send_transaction(&self, tx: Transaction) -> Result<H256>;

    #[rpc(name = "get_transaction")]
    fn get_transaction(&self, tx_hash: H256) -> Result<TransactionWithStatus>;

    #[rpc(name = "fetch_transaction")]
    fn fetch_transaction(&self, tx_hash: H256) -> Result<FetchStatus<TransactionWithStatus>>;
}

#[rpc(server)]
pub trait ChainRpc {
    #[rpc(name = "get_tip_header")]
    fn get_tip_header(&self) -> Result<HeaderView>;

    #[rpc(name = "get_genesis_block")]
    fn get_genesis_block(&self) -> Result<BlockView>;

    #[rpc(name = "get_header")]
    fn get_header(&self, block_hash: H256) -> Result<Option<HeaderView>>;

    #[rpc(name = "fetch_header")]
    fn fetch_header(&self, block_hash: H256) -> Result<FetchStatus<HeaderView>>;
}

#[rpc(server)]
pub trait NetRpc {
    #[rpc(name = "local_node_info")]
    fn local_node_info(&self) -> Result<LocalNode>;

    #[rpc(name = "get_peers")]
    fn get_peers(&self) -> Result<Vec<RemoteNode>>;
}

#[derive(Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SetScriptsCommand {
    // Replace all scripts with new scripts, non-exist scripts will be deleted
    All,
    // Update partial scripts with new scripts, non-exist scripts will be ignored
    Partial,
    // Delete scripts, non-exist scripts will be ignored
    Delete,
}

impl From<SetScriptsCommand> for storage::SetScriptsCommand {
    fn from(cmd: SetScriptsCommand) -> Self {
        match cmd {
            SetScriptsCommand::All => Self::All,
            SetScriptsCommand::Partial => Self::Partial,
            SetScriptsCommand::Delete => Self::Delete,
        }
    }
}

#[derive(Deserialize, Serialize, Eq, PartialEq, Debug)]
#[serde(tag = "status")]
#[serde(rename_all = "snake_case")]
pub enum FetchStatus<T> {
    Added { timestamp: Uint64 },
    Fetching { first_sent: Uint64 },
    Fetched { data: T },
    NotFound,
}

#[derive(Deserialize, Serialize)]
pub struct ScriptStatus {
    pub script: Script,
    pub script_type: ScriptType,
    pub block_number: BlockNumber,
}

impl From<storage::ScriptType> for ScriptType {
    fn from(st: storage::ScriptType) -> Self {
        match st {
            storage::ScriptType::Lock => Self::Lock,
            storage::ScriptType::Type => Self::Type,
        }
    }
}

impl From<ScriptType> for storage::ScriptType {
    fn from(st: ScriptType) -> Self {
        match st {
            ScriptType::Lock => Self::Lock,
            ScriptType::Type => Self::Type,
        }
    }
}

impl From<ScriptStatus> for storage::ScriptStatus {
    fn from(ss: ScriptStatus) -> Self {
        Self {
            script: ss.script.into(),
            script_type: ss.script_type.into(),
            block_number: ss.block_number.into(),
        }
    }
}

impl From<storage::ScriptStatus> for ScriptStatus {
    fn from(ss: storage::ScriptStatus) -> Self {
        Self {
            script: ss.script.into(),
            script_type: ss.script_type.into(),
            block_number: ss.block_number.into(),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct LocalNode {
    /// light client node version.
    ///
    /// Example: "version": "0.2.0"
    pub version: String,
    /// The unique node ID derived from the p2p private key.
    ///
    /// The private key is generated randomly on the first boot.
    pub node_id: String,
    /// Whether this node is active.
    ///
    /// An inactive node ignores incoming p2p messages and drops outgoing messages.
    pub active: bool,
    /// P2P addresses of this node.
    ///
    /// A node can have multiple addresses.
    pub addresses: Vec<NodeAddress>,
    /// Supported protocols.
    pub protocols: Vec<LocalNodeProtocol>,
    /// Count of currently connected peers.
    pub connections: Uint64,
}

/// The information of a P2P protocol that is supported by the local node.
#[derive(Deserialize, Serialize)]
pub struct LocalNodeProtocol {
    /// Unique protocol ID.
    pub id: Uint64,
    /// Readable protocol name.
    pub name: String,
    /// Supported versions.
    ///
    /// See [Semantic Version](https://semver.org/) about how to specify a version.
    pub support_versions: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct RemoteNode {
    /// The remote node version.
    pub version: String,
    /// The remote node ID which is derived from its P2P private key.
    pub node_id: String,
    /// The remote node addresses.
    pub addresses: Vec<NodeAddress>,
    /// Elapsed time in milliseconds since the remote node is connected.
    pub connected_duration: Uint64,
    /// Null means chain sync has not started with this remote node yet.
    pub sync_state: Option<PeerSyncState>,
    /// Active protocols.
    ///
    /// CKB uses Tentacle multiplexed network framework. Multiple protocols are running
    /// simultaneously in the connection.
    pub protocols: Vec<RemoteNodeProtocol>,
    // TODO: maybe add this field later.
    // /// Elapsed time in milliseconds since receiving the ping response from this remote node.
    // ///
    // /// Null means no ping responses have been received yet.
    // pub last_ping_duration: Option<Uint64>,
}
#[derive(Deserialize, Serialize)]
pub struct PeerSyncState {
    /// Requested best known header of remote peer.
    ///
    /// This is the best known header yet to be proved.
    pub requested_best_known_header: Option<HeaderView>,
    /// Proved best known header of remote peer.
    pub proved_best_known_header: Option<HeaderView>,
}

#[derive(Deserialize)]
pub struct SearchKey {
    pub(crate) script: Script,
    pub(crate) script_type: ScriptType,
    pub(crate) filter: Option<SearchKeyFilter>,
    pub(crate) with_data: Option<bool>,
    pub(crate) group_by_transaction: Option<bool>,
}

impl Default for SearchKey {
    fn default() -> Self {
        Self {
            script: Script::default(),
            script_type: ScriptType::Lock,
            filter: None,
            with_data: None,
            group_by_transaction: None,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct SearchKeyFilter {
    pub(crate) script: Option<Script>,
    pub(crate) script_len_range: Option<[Uint64; 2]>,
    pub(crate) output_data_len_range: Option<[Uint64; 2]>,
    pub(crate) output_capacity_range: Option<[Uint64; 2]>,
    pub(crate) block_range: Option<[BlockNumber; 2]>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptType {
    Lock,
    Type,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Desc,
    Asc,
}

#[derive(Serialize)]
pub struct Cell {
    output: CellOutput,
    pub(crate) output_data: Option<JsonBytes>,
    pub(crate) out_point: OutPoint,
    block_number: BlockNumber,
    tx_index: Uint32,
}

#[derive(Serialize)]
pub struct CellsCapacity {
    pub capacity: Capacity,
    pub block_hash: H256,
    pub block_number: BlockNumber,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Tx {
    Ungrouped(TxWithCell),
    Grouped(TxWithCells),
}

#[cfg(test)]
impl Tx {
    pub fn tx_hash(&self) -> H256 {
        match self {
            Tx::Ungrouped(tx) => tx.transaction.hash.clone(),
            Tx::Grouped(tx) => tx.transaction.hash.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct TxWithCell {
    transaction: TransactionView,
    block_number: BlockNumber,
    tx_index: Uint32,
    io_index: Uint32,
    io_type: CellType,
}

#[derive(Serialize)]
pub struct TxWithCells {
    transaction: TransactionView,
    block_number: BlockNumber,
    tx_index: Uint32,
    cells: Vec<(CellType, Uint32)>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum CellType {
    Input,
    Output,
}

#[derive(Serialize)]
pub struct Pagination<T> {
    pub(crate) objects: Vec<T>,
    pub(crate) last_cursor: JsonBytes,
}

#[derive(Serialize, Debug, Eq, PartialEq)]
pub struct TransactionWithStatus {
    pub(crate) transaction: Option<TransactionView>,
    pub(crate) cycles: Option<Cycle>,
    pub(crate) tx_status: TxStatus,
}

#[derive(Serialize, Debug, Eq, PartialEq)]
pub struct TxStatus {
    pub status: Status,
    pub block_hash: Option<H256>,
}

#[derive(Serialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Committed,
    Unknown,
}

pub struct BlockFilterRpcImpl {
    pub(crate) swc: StorageWithChainData,
}

pub struct TransactionRpcImpl {
    pub(crate) pending_txs: Arc<RwLock<PendingTxs>>,
    pub(crate) swc: StorageWithChainData,
    pub(crate) consensus: Arc<Consensus>,
}

pub struct ChainRpcImpl {
    pub(crate) swc: StorageWithChainData,
}

pub struct NetRpcImpl {
    network_controller: NetworkController,
    peers: Arc<Peers>,
}

impl BlockFilterRpc for BlockFilterRpcImpl {
    fn set_scripts(
        &self,
        scripts: Vec<ScriptStatus>,
        command: Option<SetScriptsCommand>,
    ) -> Result<()> {
        let mut matched_blocks = self.swc.matched_blocks().write().expect("poisoned");
        let scripts = scripts.into_iter().map(Into::into).collect();
        self.swc
            .storage()
            .update_filter_scripts(scripts, command.map(Into::into).unwrap_or_default());
        matched_blocks.clear();
        Ok(())
    }

    fn get_scripts(&self) -> Result<Vec<ScriptStatus>> {
        let scripts = self.swc.storage().get_filter_scripts();
        Ok(scripts.into_iter().map(Into::into).collect())
    }

    fn get_cells(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after_cursor: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>> {
        let (prefix, from_key, direction, skip) = build_query_options(
            &search_key,
            KeyPrefix::CellLockScript,
            KeyPrefix::CellTypeScript,
            order,
            after_cursor,
        )?;
        let limit = limit.value() as usize;
        if limit == 0 {
            return Err(Error::invalid_params("limit should be greater than 0"));
        }
        let with_data = search_key.with_data.unwrap_or(true);
        let filter_script_type = match search_key.script_type {
            ScriptType::Lock => ScriptType::Type,
            ScriptType::Type => ScriptType::Lock,
        };
        let (
            filter_prefix,
            filter_script_len_range,
            filter_output_data_len_range,
            filter_output_capacity_range,
            filter_block_range,
        ) = build_filter_options(search_key)?;
        let mode = IteratorMode::From(from_key.as_ref(), direction);
        let snapshot = self.swc.storage().db.snapshot();
        let iter = snapshot.iterator(mode).skip(skip);

        let mut last_key = Vec::new();
        let cells = iter
            .take_while(|(key, _value)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                let output_index = u32::from_be_bytes(
                    key[key.len() - 4..]
                        .try_into()
                        .expect("stored output_index"),
                );
                let tx_index = u32::from_be_bytes(
                    key[key.len() - 8..key.len() - 4]
                        .try_into()
                        .expect("stored tx_index"),
                );
                let block_number = u64::from_be_bytes(
                    key[key.len() - 16..key.len() - 8]
                        .try_into()
                        .expect("stored block_number"),
                );

                let tx = packed::Transaction::from_slice(
                    &snapshot
                        .get(Key::TxHash(&tx_hash).into_vec())
                        .expect("get tx should be OK")
                        .expect("stored tx")[12..],
                )
                .expect("from stored tx slice should be OK");
                let output = tx
                    .raw()
                    .outputs()
                    .get(output_index as usize)
                    .expect("get output by index should be OK");
                let output_data = tx
                    .raw()
                    .outputs_data()
                    .get(output_index as usize)
                    .expect("get output data by index should be OK");

                if let Some(prefix) = filter_prefix.as_ref() {
                    match filter_script_type {
                        ScriptType::Lock => {
                            if !extract_raw_data(&output.lock())
                                .as_slice()
                                .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                        ScriptType::Type => {
                            if output.type_().is_none()
                                || !extract_raw_data(&output.type_().to_opt().unwrap())
                                    .as_slice()
                                    .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                    }
                }

                if let Some([r0, r1]) = filter_script_len_range {
                    match filter_script_type {
                        ScriptType::Lock => {
                            let script_len = extract_raw_data(&output.lock()).len();
                            if script_len < r0 || script_len > r1 {
                                return None;
                            }
                        }
                        ScriptType::Type => {
                            let script_len = output
                                .type_()
                                .to_opt()
                                .map(|script| extract_raw_data(&script).len())
                                .unwrap_or_default();
                            if script_len < r0 || script_len > r1 {
                                return None;
                            }
                        }
                    }
                }

                if let Some([r0, r1]) = filter_output_data_len_range {
                    if output_data.len() < r0 || output_data.len() >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_output_capacity_range {
                    let capacity: core::Capacity = output.capacity().unpack();
                    if capacity < r0 || capacity >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_block_range {
                    if block_number < r0 || block_number >= r1 {
                        return None;
                    }
                }

                last_key = key.to_vec();

                Some(Cell {
                    output: output.into(),
                    output_data: if with_data {
                        Some(output_data.into())
                    } else {
                        None
                    },
                    out_point: packed::OutPoint::new(tx_hash, output_index).into(),
                    block_number: block_number.into(),
                    tx_index: tx_index.into(),
                })
            })
            .take(limit)
            .collect::<Vec<_>>();

        Ok(Pagination {
            objects: cells,
            last_cursor: JsonBytes::from_vec(last_key),
        })
    }

    fn get_transactions(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: Uint32,
        after_cursor: Option<JsonBytes>,
    ) -> Result<Pagination<Tx>> {
        let (prefix, from_key, direction, skip) = build_query_options(
            &search_key,
            KeyPrefix::TxLockScript,
            KeyPrefix::TxTypeScript,
            order,
            after_cursor,
        )?;
        let limit = limit.value() as usize;
        if limit == 0 {
            return Err(Error::invalid_params("limit should be greater than 0"));
        }

        let (filter_script, filter_block_range) = if let Some(filter) = search_key.filter.as_ref() {
            if filter.output_data_len_range.is_some() {
                return Err(Error::invalid_params(
                    "doesn't support search_key.filter.output_data_len_range parameter",
                ));
            }
            if filter.output_capacity_range.is_some() {
                return Err(Error::invalid_params(
                    "doesn't support search_key.filter.output_capacity_range parameter",
                ));
            }
            let filter_script: Option<packed::Script> =
                filter.script.as_ref().map(|script| script.clone().into());
            let filter_block_range: Option<[core::BlockNumber; 2]> =
                filter.block_range.map(|r| [r[0].into(), r[1].into()]);
            (filter_script, filter_block_range)
        } else {
            (None, None)
        };

        let filter_script_type = match search_key.script_type {
            ScriptType::Lock => ScriptType::Type,
            ScriptType::Type => ScriptType::Lock,
        };

        let mode = IteratorMode::From(from_key.as_ref(), direction);
        let snapshot = self.swc.storage().db.snapshot();
        let iter = snapshot.iterator(mode).skip(skip);

        if search_key.group_by_transaction.unwrap_or_default() {
            let mut tx_with_cells: Vec<TxWithCells> = Vec::new();
            let mut last_key = Vec::new();

            for (key, value) in iter.take_while(|(key, _value)| key.starts_with(&prefix)) {
                let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                if tx_with_cells.len() == limit
                    && tx_with_cells.last_mut().unwrap().transaction.hash != tx_hash.unpack()
                {
                    break;
                }
                last_key = key.to_vec();
                let tx = packed::Transaction::from_slice(
                    &snapshot
                        .get(Key::TxHash(&tx_hash).into_vec())
                        .expect("get tx should be OK")
                        .expect("stored tx")[12..],
                )
                .expect("from stored tx slice should be OK");

                let block_number = u64::from_be_bytes(
                    key[key.len() - 17..key.len() - 9]
                        .try_into()
                        .expect("stored block_number"),
                );
                let tx_index = u32::from_be_bytes(
                    key[key.len() - 9..key.len() - 5]
                        .try_into()
                        .expect("stored tx_index"),
                );
                let io_index = u32::from_be_bytes(
                    key[key.len() - 5..key.len() - 1]
                        .try_into()
                        .expect("stored io_index"),
                );
                let io_type = if *key.last().expect("stored io_type") == 0 {
                    CellType::Input
                } else {
                    CellType::Output
                };

                if let Some(filter_script) = filter_script.as_ref() {
                    let filter_script_matched = match filter_script_type {
                        ScriptType::Lock => snapshot
                            .get(
                                Key::TxLockScript(
                                    filter_script,
                                    block_number,
                                    tx_index,
                                    io_index,
                                    match io_type {
                                        CellType::Input => storage::CellType::Input,
                                        CellType::Output => storage::CellType::Output,
                                    },
                                )
                                .into_vec(),
                            )
                            .expect("get TxLockScript should be OK")
                            .is_some(),
                        ScriptType::Type => snapshot
                            .get(
                                Key::TxTypeScript(
                                    filter_script,
                                    block_number,
                                    tx_index,
                                    io_index,
                                    match io_type {
                                        CellType::Input => storage::CellType::Input,
                                        CellType::Output => storage::CellType::Output,
                                    },
                                )
                                .into_vec(),
                            )
                            .expect("get TxTypeScript should be OK")
                            .is_some(),
                    };

                    if !filter_script_matched {
                        continue;
                    }
                }

                if let Some([r0, r1]) = filter_block_range {
                    if block_number < r0 || block_number >= r1 {
                        continue;
                    }
                }

                let last_tx_hash_is_same = tx_with_cells
                    .last_mut()
                    .map(|last| {
                        if last.transaction.hash == tx_hash.unpack() {
                            last.cells.push((io_type.clone(), io_index.into()));
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or_default();

                if !last_tx_hash_is_same {
                    tx_with_cells.push(TxWithCells {
                        transaction: tx.into_view().into(),
                        block_number: block_number.into(),
                        tx_index: tx_index.into(),
                        cells: vec![(io_type, io_index.into())],
                    });
                }
            }

            Ok(Pagination {
                objects: tx_with_cells.into_iter().map(Tx::Grouped).collect(),
                last_cursor: JsonBytes::from_vec(last_key),
            })
        } else {
            let mut last_key = Vec::new();
            let txs = iter
                .take_while(|(key, _value)| key.starts_with(&prefix))
                .filter_map(|(key, value)| {
                    let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                    let tx = packed::Transaction::from_slice(
                        &snapshot
                            .get(Key::TxHash(&tx_hash).into_vec())
                            .expect("get tx should be OK")
                            .expect("stored tx")[12..],
                    )
                    .expect("from stored tx slice should be OK");

                    let block_number = u64::from_be_bytes(
                        key[key.len() - 17..key.len() - 9]
                            .try_into()
                            .expect("stored block_number"),
                    );
                    let tx_index = u32::from_be_bytes(
                        key[key.len() - 9..key.len() - 5]
                            .try_into()
                            .expect("stored tx_index"),
                    );
                    let io_index = u32::from_be_bytes(
                        key[key.len() - 5..key.len() - 1]
                            .try_into()
                            .expect("stored io_index"),
                    );
                    let io_type = if *key.last().expect("stored io_type") == 0 {
                        CellType::Input
                    } else {
                        CellType::Output
                    };

                    if let Some(filter_script) = filter_script.as_ref() {
                        match filter_script_type {
                            ScriptType::Lock => {
                                snapshot
                                    .get(
                                        Key::TxLockScript(
                                            filter_script,
                                            block_number,
                                            tx_index,
                                            io_index,
                                            match io_type {
                                                CellType::Input => storage::CellType::Input,
                                                CellType::Output => storage::CellType::Output,
                                            },
                                        )
                                        .into_vec(),
                                    )
                                    .expect("get TxLockScript should be OK")?;
                            }
                            ScriptType::Type => {
                                snapshot
                                    .get(
                                        Key::TxTypeScript(
                                            filter_script,
                                            block_number,
                                            tx_index,
                                            io_index,
                                            match io_type {
                                                CellType::Input => storage::CellType::Input,
                                                CellType::Output => storage::CellType::Output,
                                            },
                                        )
                                        .into_vec(),
                                    )
                                    .expect("get TxTypeScript should be OK")?;
                            }
                        }
                    }

                    if let Some([r0, r1]) = filter_block_range {
                        if block_number < r0 || block_number >= r1 {
                            return None;
                        }
                    }

                    last_key = key.to_vec();
                    Some(Tx::Ungrouped(TxWithCell {
                        transaction: tx.into_view().into(),
                        block_number: block_number.into(),
                        tx_index: tx_index.into(),
                        io_index: io_index.into(),
                        io_type,
                    }))
                })
                .take(limit)
                .collect::<Vec<_>>();

            Ok(Pagination {
                objects: txs,
                last_cursor: JsonBytes::from_vec(last_key),
            })
        }
    }

    fn get_cells_capacity(&self, search_key: SearchKey) -> Result<CellsCapacity> {
        let (prefix, from_key, direction, skip) = build_query_options(
            &search_key,
            KeyPrefix::CellLockScript,
            KeyPrefix::CellTypeScript,
            Order::Asc,
            None,
        )?;
        let filter_script_type = match search_key.script_type {
            ScriptType::Lock => ScriptType::Type,
            ScriptType::Type => ScriptType::Lock,
        };
        let (
            filter_prefix,
            filter_script_len_range,
            filter_output_data_len_range,
            filter_output_capacity_range,
            filter_block_range,
        ) = build_filter_options(search_key)?;
        let mode = IteratorMode::From(from_key.as_ref(), direction);
        let snapshot = self.swc.storage().db.snapshot();
        let iter = snapshot.iterator(mode).skip(skip);

        let capacity: u64 = iter
            .take_while(|(key, _value)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let tx_hash = packed::Byte32::from_slice(&value).expect("stored tx hash");
                let output_index = u32::from_be_bytes(
                    key[key.len() - 4..]
                        .try_into()
                        .expect("stored output_index"),
                );
                let block_number = u64::from_be_bytes(
                    key[key.len() - 16..key.len() - 8]
                        .try_into()
                        .expect("stored block_number"),
                );

                let tx = packed::Transaction::from_slice(
                    &snapshot
                        .get(Key::TxHash(&tx_hash).into_vec())
                        .expect("get tx should be OK")
                        .expect("stored tx")[12..],
                )
                .expect("from stored tx slice should be OK");
                let output = tx
                    .raw()
                    .outputs()
                    .get(output_index as usize)
                    .expect("get output by index should be OK");
                let output_data = tx
                    .raw()
                    .outputs_data()
                    .get(output_index as usize)
                    .expect("get output data by index should be OK");

                if let Some(prefix) = filter_prefix.as_ref() {
                    match filter_script_type {
                        ScriptType::Lock => {
                            if !extract_raw_data(&output.lock())
                                .as_slice()
                                .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                        ScriptType::Type => {
                            if output.type_().is_none()
                                || !extract_raw_data(&output.type_().to_opt().unwrap())
                                    .as_slice()
                                    .starts_with(prefix)
                            {
                                return None;
                            }
                        }
                    }
                }

                if let Some([r0, r1]) = filter_script_len_range {
                    match filter_script_type {
                        ScriptType::Lock => {
                            let script_len = extract_raw_data(&output.lock()).len();
                            if script_len < r0 || script_len > r1 {
                                return None;
                            }
                        }
                        ScriptType::Type => {
                            let script_len = output
                                .type_()
                                .to_opt()
                                .map(|script| extract_raw_data(&script).len())
                                .unwrap_or_default();
                            if script_len < r0 || script_len > r1 {
                                return None;
                            }
                        }
                    }
                }

                if let Some([r0, r1]) = filter_output_data_len_range {
                    if output_data.len() < r0 || output_data.len() >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_output_capacity_range {
                    let capacity: core::Capacity = output.capacity().unpack();
                    if capacity < r0 || capacity >= r1 {
                        return None;
                    }
                }

                if let Some([r0, r1]) = filter_block_range {
                    if block_number < r0 || block_number >= r1 {
                        return None;
                    }
                }

                Some(Unpack::<core::Capacity>::unpack(&output.capacity()).as_u64())
            })
            .sum();

        let key = Key::Meta(LAST_STATE_KEY).into_vec();
        let tip_header = snapshot
            .get(key)
            .expect("snapshot get last state should be ok")
            .map(|data| packed::HeaderReader::from_slice_should_be_ok(&data[32..]).to_entity())
            .expect("tip header should be inited");
        Ok(CellsCapacity {
            capacity: capacity.into(),
            block_hash: tip_header.calc_header_hash().unpack(),
            block_number: tip_header.raw().number().unpack(),
        })
    }
}

const MAX_ADDRS: usize = 50;

impl NetRpc for NetRpcImpl {
    fn local_node_info(&self) -> Result<LocalNode> {
        Ok(LocalNode {
            version: self.network_controller.version().to_owned(),
            node_id: self.network_controller.node_id(),
            active: self.network_controller.is_active(),
            addresses: self
                .network_controller
                .public_urls(MAX_ADDRS)
                .into_iter()
                .map(|(address, score)| NodeAddress {
                    address,
                    score: u64::from(score).into(),
                })
                .collect(),
            protocols: self
                .network_controller
                .protocols()
                .into_iter()
                .map(|(protocol_id, name, support_versions)| LocalNodeProtocol {
                    id: (protocol_id.value() as u64).into(),
                    name,
                    support_versions,
                })
                .collect::<Vec<_>>(),
            connections: (self.network_controller.connected_peers().len() as u64).into(),
        })
    }

    fn get_peers(&self) -> Result<Vec<RemoteNode>> {
        let peers: Vec<RemoteNode> = self
            .network_controller
            .connected_peers()
            .iter()
            .map(|(peer_index, peer)| {
                let mut addresses = vec![&peer.connected_addr];
                addresses.extend(peer.listened_addrs.iter());

                let node_addresses = addresses
                    .iter()
                    .map(|addr| {
                        let score = self
                            .network_controller
                            .addr_info(addr)
                            .map(|addr_info| addr_info.score)
                            .unwrap_or(1);
                        let non_negative_score = if score > 0 { score as u64 } else { 0 };
                        NodeAddress {
                            address: addr.to_string(),
                            score: non_negative_score.into(),
                        }
                    })
                    .collect();

                RemoteNode {
                    version: peer
                        .identify_info
                        .as_ref()
                        .map(|info| info.client_version.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    node_id: extract_peer_id(&peer.connected_addr)
                        .map(|peer_id| peer_id.to_base58())
                        .unwrap_or_default(),
                    addresses: node_addresses,
                    connected_duration: (std::time::Instant::now()
                        .saturating_duration_since(peer.connected_time)
                        .as_millis() as u64)
                        .into(),
                    sync_state: self.peers.get_state(peer_index).map(|state| PeerSyncState {
                        requested_best_known_header: state
                            .get_prove_request()
                            .map(|request| request.get_last_header().header().to_owned().into()),
                        proved_best_known_header: state
                            .get_prove_state()
                            .map(|request| request.get_last_header().header().to_owned().into()),
                    }),
                    protocols: peer
                        .protocols
                        .iter()
                        .map(|(protocol_id, protocol_version)| RemoteNodeProtocol {
                            id: (protocol_id.value() as u64).into(),
                            version: protocol_version.clone(),
                        })
                        .collect(),
                }
            })
            .collect();
        Ok(peers)
    }
}

const MAX_PREFIX_SEARCH_SIZE: usize = u16::max_value() as usize;

// a helper fn to build query options from search paramters, returns prefix, from_key, direction and skip offset
fn build_query_options(
    search_key: &SearchKey,
    lock_prefix: KeyPrefix,
    type_prefix: KeyPrefix,
    order: Order,
    after_cursor: Option<JsonBytes>,
) -> Result<(Vec<u8>, Vec<u8>, Direction, usize)> {
    let mut prefix = match search_key.script_type {
        ScriptType::Lock => vec![lock_prefix as u8],
        ScriptType::Type => vec![type_prefix as u8],
    };
    let script: packed::Script = search_key.script.clone().into();
    let args_len = script.args().len();
    if args_len > MAX_PREFIX_SEARCH_SIZE {
        return Err(Error::invalid_params(format!(
            "search_key.script.args len should be less than {}",
            MAX_PREFIX_SEARCH_SIZE
        )));
    }
    prefix.extend_from_slice(extract_raw_data(&script).as_slice());

    let (from_key, direction, skip) = match order {
        Order::Asc => after_cursor.map_or_else(
            || (prefix.clone(), Direction::Forward, 0),
            |json_bytes| (json_bytes.as_bytes().into(), Direction::Forward, 1),
        ),
        Order::Desc => after_cursor.map_or_else(
            || {
                (
                    [
                        prefix.clone(),
                        vec![0xff; MAX_PREFIX_SEARCH_SIZE - args_len],
                    ]
                    .concat(),
                    Direction::Reverse,
                    0,
                )
            },
            |json_bytes| (json_bytes.as_bytes().into(), Direction::Reverse, 1),
        ),
    };

    Ok((prefix, from_key, direction, skip))
}

// a helper fn to build filter options from search paramters, returns prefix, output_data_len_range, output_capacity_range and block_range
#[allow(clippy::type_complexity)]
fn build_filter_options(
    search_key: SearchKey,
) -> Result<(
    Option<Vec<u8>>,
    Option<[usize; 2]>,
    Option<[usize; 2]>,
    Option<[core::Capacity; 2]>,
    Option<[core::BlockNumber; 2]>,
)> {
    let filter = search_key.filter.unwrap_or_default();
    let filter_script_prefix = if let Some(script) = filter.script {
        let script: packed::Script = script.into();
        if script.args().len() > MAX_PREFIX_SEARCH_SIZE {
            return Err(Error::invalid_params(format!(
                "search_key.filter.script.args len should be less than {}",
                MAX_PREFIX_SEARCH_SIZE
            )));
        }
        let mut prefix = Vec::new();
        prefix.extend_from_slice(extract_raw_data(&script).as_slice());
        Some(prefix)
    } else {
        None
    };

    let filter_script_len_range = filter.script_len_range.map(|[r0, r1]| {
        [
            Into::<u64>::into(r0) as usize,
            Into::<u64>::into(r1) as usize,
        ]
    });

    let filter_output_data_len_range = filter.output_data_len_range.map(|[r0, r1]| {
        [
            Into::<u64>::into(r0) as usize,
            Into::<u64>::into(r1) as usize,
        ]
    });
    let filter_output_capacity_range = filter.output_capacity_range.map(|[r0, r1]| {
        [
            core::Capacity::shannons(r0.into()),
            core::Capacity::shannons(r1.into()),
        ]
    });
    let filter_block_range = filter.block_range.map(|r| [r[0].into(), r[1].into()]);

    Ok((
        filter_script_prefix,
        filter_script_len_range,
        filter_output_data_len_range,
        filter_output_capacity_range,
        filter_block_range,
    ))
}

impl TransactionRpc for TransactionRpcImpl {
    fn send_transaction(&self, tx: Transaction) -> Result<H256> {
        let tx: packed::Transaction = tx.into();
        let tx = tx.into_view();
        let cycles = verify_tx(tx.clone(), &self.swc, Arc::clone(&self.consensus))
            .map_err(|e| Error::invalid_params(format!("invalid transaction: {:?}", e)))?;
        self.pending_txs
            .write()
            .expect("pending_txs lock is poisoned")
            .push(tx.clone(), cycles);

        Ok(tx.hash().unpack())
    }

    fn get_transaction(&self, tx_hash: H256) -> Result<TransactionWithStatus> {
        if let Some((transaction, header)) = self
            .swc
            .storage()
            .get_transaction_with_header(&tx_hash.pack())
        {
            return Ok(TransactionWithStatus {
                transaction: Some(transaction.into_view().into()),
                cycles: None,
                tx_status: TxStatus {
                    block_hash: Some(header.into_view().hash().unpack()),
                    status: Status::Committed,
                },
            });
        }

        if let Some((transaction, cycles, _)) = self
            .pending_txs
            .read()
            .expect("pending_txs lock is poisoned")
            .get(&tx_hash.pack())
        {
            return Ok(TransactionWithStatus {
                transaction: Some(transaction.into_view().into()),
                cycles: Some(cycles.into()),
                tx_status: TxStatus {
                    block_hash: None,
                    status: Status::Pending,
                },
            });
        }

        Ok(TransactionWithStatus {
            transaction: None,
            cycles: None,
            tx_status: TxStatus {
                block_hash: None,
                status: Status::Unknown,
            },
        })
    }

    fn fetch_transaction(&self, tx_hash: H256) -> Result<FetchStatus<TransactionWithStatus>> {
        let tws = self.get_transaction(tx_hash.clone())?;
        if tws.transaction.is_some() {
            return Ok(FetchStatus::Fetched { data: tws });
        }

        let now = unix_time_as_millis();
        if let Some((added_ts, first_sent, missing)) = self.swc.get_tx_fetch_info(&tx_hash) {
            if missing {
                // re-fetch the transaction
                self.swc.add_fetch_tx(tx_hash, now);
                return Ok(FetchStatus::NotFound);
            } else if first_sent > 0 {
                return Ok(FetchStatus::Fetching {
                    first_sent: first_sent.into(),
                });
            } else {
                return Ok(FetchStatus::Added {
                    timestamp: added_ts.into(),
                });
            }
        } else {
            self.swc.add_fetch_tx(tx_hash, now);
        }
        Ok(FetchStatus::Added {
            timestamp: now.into(),
        })
    }
}

impl ChainRpc for ChainRpcImpl {
    fn get_tip_header(&self) -> Result<HeaderView> {
        Ok(self.swc.storage().get_tip_header().into_view().into())
    }

    fn get_genesis_block(&self) -> Result<BlockView> {
        Ok(self.swc.storage().get_genesis_block().into_view().into())
    }

    fn get_header(&self, block_hash: H256) -> Result<Option<HeaderView>> {
        Ok(self.swc.get_header(&block_hash.pack()).map(Into::into))
    }

    fn fetch_header(&self, block_hash: H256) -> Result<FetchStatus<HeaderView>> {
        if let Some(value) = self.get_header(block_hash.clone())? {
            if self.swc.storage().get_header(&block_hash.pack()).is_none() {
                self.swc
                    .storage()
                    .add_fetched_header(&value.inner.clone().into());
            }
            return Ok(FetchStatus::Fetched { data: value });
        }
        let now = unix_time_as_millis();
        if let Some((added_ts, first_sent, missing)) = self.swc.get_header_fetch_info(&block_hash) {
            if missing {
                // re-fetch the header
                self.swc.add_fetch_header(block_hash, now);
                return Ok(FetchStatus::NotFound);
            } else if first_sent > 0 {
                return Ok(FetchStatus::Fetching {
                    first_sent: first_sent.into(),
                });
            } else {
                return Ok(FetchStatus::Added {
                    timestamp: added_ts.into(),
                });
            }
        } else {
            self.swc.add_fetch_header(block_hash, now);
        }
        Ok(FetchStatus::Added {
            timestamp: now.into(),
        })
    }
}

pub(crate) struct Service {
    listen_address: String,
}

impl Service {
    pub fn new(listen_address: &str) -> Self {
        Self {
            listen_address: listen_address.to_string(),
        }
    }

    pub fn start(
        &self,
        network_controller: NetworkController,
        storage: Storage,
        peers: Arc<Peers>,
        pending_txs: Arc<RwLock<PendingTxs>>,
        consensus: Consensus,
    ) -> Server {
        let mut io_handler = IoHandler::new();
        let swc = StorageWithChainData::new(storage, Arc::clone(&peers));
        let block_filter_rpc_impl = BlockFilterRpcImpl { swc: swc.clone() };
        let chain_rpc_impl = ChainRpcImpl { swc: swc.clone() };
        let transaction_rpc_impl = TransactionRpcImpl {
            pending_txs,
            swc,
            consensus: Arc::new(consensus),
        };
        let net_rpc_impl = NetRpcImpl {
            network_controller,
            peers,
        };
        io_handler.extend_with(block_filter_rpc_impl.to_delegate());
        io_handler.extend_with(chain_rpc_impl.to_delegate());
        io_handler.extend_with(transaction_rpc_impl.to_delegate());
        io_handler.extend_with(net_rpc_impl.to_delegate());

        ServerBuilder::new(io_handler)
            .cors(DomainsValidation::AllowOnly(vec![
                AccessControlAllowOrigin::Null,
                AccessControlAllowOrigin::Any,
            ]))
            .health_api(("/ping", "ping"))
            .start_http(
                &self
                    .listen_address
                    .to_socket_addrs()
                    .expect("config listen_address parsed")
                    .next()
                    .expect("config listen_address parsed"),
            )
            .expect("Start Jsonrpc HTTP service")
    }
}

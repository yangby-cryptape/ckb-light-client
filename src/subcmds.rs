use std::sync::{Arc, RwLock};

use ckb_async_runtime::new_global_runtime;
use ckb_chain_spec::ChainSpec;
use ckb_network::{
    BlockingFlag, CKBProtocol, CKBProtocolHandler, DefaultExitHandler, ExitHandler, NetworkService,
    NetworkState, SupportProtocols,
};
use ckb_resource::Resource;

use crate::{
    config::RunConfig,
    error::{Error, Result},
    protocols::{FilterProtocol, LightClientProtocol, PendingTxs, RelayProtocol, SyncProtocol},
    service::Service,
    storage::Storage,
    utils,
};

impl RunConfig {
    pub(crate) fn execute(self) -> Result<()> {
        log::info!("Run ...");

        utils::fs::need_directory(&self.run_env.network.path)?;

        let storage = Storage::new(&self.run_env.store.path);
        let chain_spec = ChainSpec::load_from(&match self.run_env.chain.as_str() {
            "mainnet" => Resource::bundled("specs/mainnet.toml".to_string()),
            "testnet" => Resource::bundled("specs/testnet.toml".to_string()),
            path => Resource::file_system(path.into()),
        })
        .expect("load spec should be OK");
        let consensus = chain_spec
            .build_consensus()
            .expect("build consensus should be OK");
        storage.init_genesis_block(consensus.genesis_block().data());

        let pending_txs = Arc::new(RwLock::new(PendingTxs::new(64)));
        let network_state = NetworkState::from_config(self.run_env.network)
            .map(Arc::new)
            .map_err(|err| {
                let errmsg = format!("failed to initialize network state since {}", err);
                Error::runtime(errmsg)
            })?;
        let required_protocol_ids = vec![
            SupportProtocols::Sync.protocol_id(),
            SupportProtocols::RelayV2.protocol_id(),
            SupportProtocols::LightClient.protocol_id(),
            SupportProtocols::Filter.protocol_id(),
        ];

        let mut blocking_recv_flag = BlockingFlag::default();
        blocking_recv_flag.disable_connected();
        blocking_recv_flag.disable_disconnected();
        blocking_recv_flag.disable_notify();

        let sync_protocol = SyncProtocol::new(storage.clone());
        let relay_protocol = RelayProtocol::new(pending_txs.clone());
        let light_client: Box<dyn CKBProtocolHandler> =
            Box::new(LightClientProtocol::new(consensus.pow.clone()));
        let filter_protocol = FilterProtocol::new(storage.clone());

        let protocols = vec![
            CKBProtocol::new_with_support_protocol(
                SupportProtocols::Sync,
                Box::new(sync_protocol),
                Arc::clone(&network_state),
            ),
            CKBProtocol::new_with_support_protocol(
                SupportProtocols::RelayV2,
                Box::new(relay_protocol),
                Arc::clone(&network_state),
            ),
            CKBProtocol::new_with_support_protocol(
                SupportProtocols::LightClient,
                light_client,
                Arc::clone(&network_state),
            ),
            CKBProtocol::new_with_support_protocol(
                SupportProtocols::Filter,
                Box::new(filter_protocol),
                Arc::clone(&network_state),
            ),
        ];

        let (handle, stop_handler) = new_global_runtime();
        let exit_handler = DefaultExitHandler::default();

        let network_controller = NetworkService::new(
            Arc::clone(&network_state),
            protocols,
            required_protocol_ids,
            consensus.identify_name(),
            clap::crate_version!().to_owned(),
            exit_handler.clone(),
        )
        .start(&handle)
        .map_err(|err| {
            let errmsg = format!("failed to start network since {}", err);
            Error::runtime(errmsg)
        })?;

        let service = Service::new("127.0.0.1:9000");
        let rpc_server = service.start(network_controller, storage, pending_txs);

        let exit_handler_clone = exit_handler.clone();
        ctrlc::set_handler(move || {
            exit_handler_clone.notify_exit();
        })
        .map_err(|err| {
            let errmsg = format!("failed to set Ctrl-C handler since {}", err);
            Error::runtime(errmsg)
        })?;
        exit_handler.wait_for_exit();
        rpc_server.close();
        Ok(())
    }
}

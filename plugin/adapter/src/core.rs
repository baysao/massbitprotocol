use crate::setting::*;
pub use crate::stream_mod::{
    streamout_client::StreamoutClient, ChainType, DataType, GenericDataProto, GetBlocksRequest,
};
pub use crate::{HandlerProxyType, PluginRegistrar, WasmHandlerProxyType};
use graph::data::subgraph::SubgraphManifest;
use graph::semver::Op;
use graph_chain_ethereum::Chain;
use graph_chain_ethereum::{DataSource, DataSourceTemplate};
use graph_runtime_wasm::ValidModule;
use index_store::postgres::store_builder::*;
use index_store::{IndexerState, Store};
use lazy_static::lazy_static;
use libloading::Library;
use massbit_common::prelude::tokio::time::{sleep, timeout, Duration};
use massbit_common::NetworkType;
use serde_yaml::Value;
use std::path::Path;
use std::{
    alloc::System, collections::HashMap, env, error::Error, ffi::OsStr, fmt, path::PathBuf,
    sync::Arc,
};
use tonic::transport::Channel;
use tonic::{Request, Streaming};
use tower::timeout::Timeout;

lazy_static! {
    static ref CHAIN_READER_URL: String =
        env::var("CHAIN_READER_URL").unwrap_or(String::from("http://127.0.0.1:50051"));
    static ref IPFS_ADDRESS: String =
        env::var("IPFS_ADDRESS").unwrap_or(String::from("0.0.0.0:5001"));
    static ref GENERATED_FOLDER: String = String::from("index-manager/generated/");
    static ref COMPONENT_NAME: String = String::from("[Adapter-Manager]");
}
const GET_BLOCK_TIMEOUT_SEC: u64 = 30;
const GET_STREAM_TIMEOUT_SEC: u64 = 30;
#[global_allocator]
static ALLOCATOR: System = System;

#[derive(Copy, Clone)]
pub struct AdapterDeclaration {
    pub register: unsafe extern "C" fn(&mut dyn PluginRegistrar),
}
pub struct AdapterHandler {
    indexer_hash: String,
    pub lib: Arc<Library>,
    pub handler_proxies: HashMap<String, HandlerProxyType>,
}

impl AdapterHandler {
    fn new(hash: String, lib: Arc<Library>) -> AdapterHandler {
        AdapterHandler {
            indexer_hash: hash,
            lib,
            handler_proxies: HashMap::default(),
        }
    }
}

pub struct WasmAdapter {
    indexer_hash: String,
    pub wasm: Arc<ValidModule>,
    pub handler_proxies: HashMap<String, Arc<Option<WasmHandlerProxyType>>>,
}

impl WasmAdapter {
    fn new(hash: String, wasm: Arc<ValidModule>) -> WasmAdapter {
        WasmAdapter {
            indexer_hash: hash,
            wasm,
            handler_proxies: HashMap::default(),
        }
    }
}

pub struct AdapterManager {
    //store: Option<dyn Store>,
    libs: HashMap<String, Arc<Library>>,
    map_handlers: HashMap<String, AdapterHandler>,
}

impl AdapterManager {
    pub fn new() -> AdapterManager {
        AdapterManager {
            //store: None,
            libs: HashMap::default(),
            map_handlers: HashMap::default(),
        }
    }
    pub async fn init(
        &mut self,
        hash: &String,
        config: &Value,
        mapping: &PathBuf,
        schema: &PathBuf,
        manifest: &Option<SubgraphManifest<Chain>>,
    ) -> Result<(), Box<dyn Error>> {
        let mut data_sources: Vec<DataSource> = vec![];
        let mut templates: Vec<DataSourceTemplate> = vec![];
        if let Some(sgd) = manifest {
            data_sources = sgd
                .data_sources
                .iter()
                .map(|ds| ds.clone())
                .collect::<Vec<DataSource>>();
            templates = sgd
                .templates
                .iter()
                .map(|tpl| tpl.clone())
                .collect::<Vec<DataSourceTemplate>>();
        }

        let arc_templates = Arc::new(templates);
        //Todo: Currently adapter only works with one datasource
        assert_eq!(
            data_sources.len(),
            1,
            "Error: Number datasource: {} is not 1.",
            data_sources.len()
        );
        match data_sources.get(0) {
            Some(data_source) => {
                log::info!(
                    "{} Init Streamout client for chain {} using language {}",
                    &*COMPONENT_NAME,
                    &data_source.kind,
                    &data_source.mapping.language
                );
                //let chain_type = get_chain_type(data_source);
                let channel = Channel::from_static(CHAIN_READER_URL.as_str())
                    .connect()
                    .await?;
                let timeout_channel =
                    Timeout::new(channel, Duration::from_secs(GET_BLOCK_TIMEOUT_SEC));
                let mut client = StreamoutClient::new(timeout_channel);
                match data_source.mapping.language.as_str() {
                    "wasm/assemblyscript" => {
                        self.handle_wasm_mapping(
                            hash,
                            data_source,
                            arc_templates.clone(),
                            schema,
                            &mut client,
                        )
                        .await
                    }
                    //Default use rust
                    _ => {
                        self.handle_rust_mapping(hash, data_source, mapping, schema, &mut client)
                            .await
                    }
                }
            }
            _ => Ok(()),
        }
    }

    async fn handle_wasm_mapping<P: AsRef<Path>>(
        &mut self,
        indexer_hash: &String,
        data_source: &DataSource,
        templates: Arc<Vec<DataSourceTemplate>>,
        schema_path: P,
        client: &mut StreamoutClient<Timeout<Channel>>,
    ) -> Result<(), Box<dyn Error>> {
        let store =
            Arc::new(StoreBuilder::create_store(indexer_hash.as_str(), &schema_path).unwrap());
        //let registry = Arc::new(MockMetricsRegistry::new());

        log::info!("{} Start mapping using wasm binary", &*COMPONENT_NAME);
        let adapter_name = data_source
            .kind
            .split("/")
            .collect::<Vec<&str>>()
            .get(0)
            .unwrap()
            .to_string();
        //Todo: store indexer state including start_block in db
        let mut start_block = data_source.source.start_block as u64;
        let chain_type = get_chain_type(data_source);
        let mut opt_stream: Option<Streaming<GenericDataProto>> = None;
        let mut handler_proxy = WasmHandlerProxyType::create_proxy(
            &adapter_name,
            indexer_hash,
            store,
            data_source.clone(), //Arc::clone(&valid_module),
            templates,
        );
        if let Some(ref mut proxy) = handler_proxy {
            loop {
                match opt_stream {
                    None => {
                        log::info!(
                            "Wasm mapping get new stream for chain {:?} from block {}.",
                            &chain_type,
                            start_block
                        );
                        opt_stream = try_create_stream(
                            client,
                            &chain_type,
                            start_block,
                            &data_source.network,
                        )
                        .await;
                        if opt_stream.is_none() {
                            //Sleep for a while and reconnect
                            sleep(Duration::from_secs(GET_STREAM_TIMEOUT_SEC)).await;
                        }
                    }
                    Some(ref mut stream) => {
                        let response =
                            timeout(Duration::from_secs(GET_BLOCK_TIMEOUT_SEC), stream.message())
                                .await;
                        match response {
                            Ok(Ok(res)) => {
                                if let Some(mut data) = res {
                                    let data_type = DataType::from_i32(data.data_type).unwrap();
                                    let data_chain_type =
                                        ChainType::from_i32(data.chain_type).unwrap();
                                    log::info!(
                                        "{} Chain {:?} received data block = {:?}, hash = {:?}, data type = {:?}",
                                        &*COMPONENT_NAME,
                                        &data_chain_type,
                                        data.block_number,
                                        data.block_hash,
                                        data_type
                                    );
                                    if data_chain_type == chain_type {
                                        match proxy.handle_wasm_mapping(&mut data) {
                                            Err(err) => {
                                                log::error!(
                                                    "{} Error while handle received message",
                                                    err
                                                );
                                                start_block = data.block_number;
                                            }
                                            Ok(_) => {
                                                start_block = data.block_number + 1;
                                            }
                                        }
                                    } else {
                                        log::error!("Chain type is not matched. Received {:?}, expected {:?}", data_chain_type, chain_type)
                                    }
                                } else {
                                    log::warn!("Stream message response: {:?}", res)
                                }
                            }
                            _ => {
                                log::info!("Error while get message from reader stream {:?}. Destroy old stream", &response);
                                opt_stream = None;
                            }
                        }
                    }
                }
            }
        };
        Ok(())
    }

    async fn handle_rust_mapping<P: AsRef<Path>>(
        &mut self,
        indexer_hash: &String,
        data_source: &DataSource,
        mapping_path: P,
        schema_path: P,
        client: &mut StreamoutClient<Timeout<Channel>>,
    ) -> Result<(), Box<dyn Error>> {
        let store = StoreBuilder::create_store(indexer_hash.as_str(), &schema_path).unwrap();
        let mut indexer_state = IndexerState::new(Arc::new(store));

        //Use unsafe to inject a store pointer into user's lib
        unsafe {
            match self
                .load(
                    indexer_hash,
                    mapping_path.as_ref().as_os_str(),
                    &indexer_state,
                )
                .await
            {
                Ok(_) => log::info!("{} Load library successfully", &*COMPONENT_NAME),
                Err(err) => log::error!("Load library with error {:?}", err),
            }
        }
        log::info!("{} Start mapping using rust", &*COMPONENT_NAME);
        let adapter_name = data_source
            .kind
            .split("/")
            .collect::<Vec<&str>>()
            .get(0)
            .unwrap()
            .to_string();
        if let Some(adapter_handler) = self.map_handlers.get_mut(indexer_hash.as_str()) {
            if let Some(handler_proxy) = adapter_handler.handler_proxies.get(&adapter_name) {
                let mut start_block = data_source.source.start_block as u64;
                let chain_type = get_chain_type(data_source);
                let mut opt_stream: Option<Streaming<GenericDataProto>> = None;
                log::info!(
                    "Rust mapping get new stream for chain {:?} from block {}.",
                    &chain_type,
                    start_block
                );
                loop {
                    match opt_stream {
                        None => {
                            opt_stream = try_create_stream(
                                client,
                                &chain_type,
                                start_block,
                                &data_source.network,
                            )
                            .await;
                            if opt_stream.is_none() {
                                //Sleep for a while and reconnect
                                sleep(Duration::from_secs(GET_STREAM_TIMEOUT_SEC)).await;
                            }
                        }
                        Some(ref mut stream) => {
                            let response = timeout(
                                Duration::from_secs(GET_BLOCK_TIMEOUT_SEC),
                                stream.message(),
                            )
                            .await;
                            match response {
                                Ok(Ok(res)) => {
                                    if let Some(mut data) = res {
                                        let data_type = DataType::from_i32(data.data_type).unwrap();
                                        let data_chain_type =
                                            ChainType::from_i32(data.chain_type).unwrap();
                                        log::info!(
                                            "{} Chain {:?} received data block = {:?}, hash = {:?}, data type = {:?}",
                                            &*COMPONENT_NAME,
                                            &data_chain_type,
                                            data.block_number,
                                            data.block_hash,
                                            DataType::from_i32(data.data_type).unwrap()
                                        );
                                        if data_chain_type == chain_type {
                                            match handler_proxy
                                                .handle_rust_mapping(&mut data, &mut indexer_state)
                                            {
                                                Err(err) => {
                                                    log::error!(
                                                        "{} Error while handle received message",
                                                        err
                                                    );
                                                    start_block = data.block_number;
                                                }
                                                Ok(_) => {
                                                    start_block = data.block_number + 1;
                                                }
                                            }
                                        } else {
                                            log::error!("Chain type is not matched. Received {:?}, expected {:?}", data_chain_type, chain_type)
                                        }
                                    }
                                }
                                _ => {
                                    log::info!("Error while get message from reader stream {:?}. Recreate stream", &response);
                                    opt_stream = None;
                                }
                            }
                        }
                    }
                }
            } else {
                log::debug!(
                    "{} Cannot find proxy for adapter {}",
                    *COMPONENT_NAME,
                    adapter_name
                );
            }
        } else {
            log::debug!(
                "{} Cannot find adapter handler for indexer {}",
                &*COMPONENT_NAME,
                &indexer_hash
            );
        }
        Ok(())
    }
    /// Load a plugin library
    /// A plugin library **must** be implemented using the
    /// [`model::adapter_declaration!()`] macro. Trying manually implement
    /// a plugin without going through that macro will result in undefined
    /// behaviour.
    pub async unsafe fn load<P: AsRef<OsStr>>(
        &mut self,
        indexer_hash: &String,
        library_path: P,
        store: &dyn Store,
    ) -> Result<(), Box<dyn Error>> {
        let lib = Arc::new(Library::new(library_path)?);
        // inject store to plugin
        lib.get::<*mut Option<&dyn Store>>(b"STORE\0")?
            .write(Some(store));
        let adapter_decl = lib
            .get::<*mut AdapterDeclaration>(b"adapter_declaration\0")?
            .read();
        let mut registrar = AdapterHandler::new(indexer_hash.clone(), Arc::clone(&lib));
        (adapter_decl.register)(&mut registrar);
        self.map_handlers.insert(indexer_hash.clone(), registrar);
        self.libs.insert(indexer_hash.clone(), lib);
        Ok(())
    }
}
async fn try_create_stream(
    client: &mut StreamoutClient<Timeout<Channel>>,
    chain_type: &ChainType,
    start_block: u64,
    network: &Option<NetworkType>,
) -> Option<Streaming<GenericDataProto>> {
    log::info!("Create new stream from block {}", start_block);
    let get_blocks_request = GetBlocksRequest {
        start_block_number: start_block,
        end_block_number: 0,
        chain_type: *chain_type as i32,
        network: network.clone().unwrap_or(Default::default()),
    };
    match client
        .list_blocks(Request::new(get_blocks_request.clone()))
        .await
    {
        Ok(res) => {
            return Some(res.into_inner());
        }
        Err(err) => {
            log::info!("Create new stream with error {:?}", &err);
        }
    }
    return None;
}
// General trait for handling message,
// every adapter proxies must implement this trait
pub trait MessageHandler {
    fn handle_rust_mapping(
        &self,
        message: &mut GenericDataProto,
        store: &mut dyn Store,
    ) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn handle_wasm_mapping(
        &mut self,
        message: &mut GenericDataProto,
    ) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct AdapterError(String);

impl AdapterError {
    pub fn new(msg: &str) -> AdapterError {
        AdapterError(msg.to_string())
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for AdapterError {
    fn description(&self) -> &str {
        &self.0
    }
}

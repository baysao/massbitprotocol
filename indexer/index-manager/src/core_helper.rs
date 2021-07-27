use diesel::{Connection, PgConnection, RunQueryDsl};
use lazy_static::lazy_static;
use postgres::{Connection as PostgreConnection, TlsMode};
use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::{env, fs};
use tonic::Request;
use std::time::Instant;

// Massbit dependencies
use crate::types::{DeployParams, DeployType, Indexer};
use index_store::core::IndexStore;
use plugin::manager::PluginManager;
use stream_mod::{GetBlocksRequest, GenericDataProto, ChainType, DataType, streamout_client::StreamoutClient};
use crate::builder::{IndexConfigLocalBuilder, IndexConfigIpfsBuilder};
use crate::hasura::{track_hasura_table, track_hasura_with_ddl_gen_plugin};
use crate::store::{create_new_indexer_detail_table, insert_new_indexer, migrate_with_ddl_gen_plugin, create_indexers_table_if_not_exists};
use massbit_chain_substrate::data_type::{decode, SubstrateBlock, get_extrinsics_from_block, SubstrateEventRecord};
use massbit_chain_solana::data_type::{decode as solana_decode, SolanaEncodedBlock, convert_solana_encoded_block_to_solana_block, SolanaTransaction, SolanaLogMessages};
use crate::config::read_config_file;

pub mod stream_mod {
    tonic::include_proto!("chaindata");
}
lazy_static! {
    static ref CHAIN_READER_URL: String =
        env::var("CHAIN_READER_URL").unwrap_or(String::from("http://127.0.0.1:50051"));
    static ref DATABASE_CONNECTION_STRING: String = env::var("DATABASE_CONNECTION_STRING")
        .unwrap_or(String::from("postgres://graph-node:let-me-in@localhost"));
    static ref IPFS_ADDRESS: String =
        env::var("IPFS_ADDRESS").unwrap_or(String::from("0.0.0.0:5001"));
}

pub async fn loop_blocks(params: DeployParams) -> Result<(), Box<dyn Error>> {
    let mut store = IndexStore::new(DATABASE_CONNECTION_STRING.as_str());

    // Get user index mapping logic, query for migration and index's configurations
    let index_config = match params.deploy_type {
        DeployType::Local => {
            let index_config = IndexConfigLocalBuilder::default()
                .query(params.query)
                .config(params.config)
                .mapping(params.mapping)
                .schema(params.schema)
                .build();
            index_config
        }
        DeployType::Ipfs => {
            let index_config = IndexConfigIpfsBuilder::default()
                .query(params.query).await
                .config(params.config).await
                .mapping(params.mapping).await
                .schema(params.schema).await
                .build();
            index_config
        }
    };

    let connection = PgConnection::establish(&DATABASE_CONNECTION_STRING).expect(&format!(
        "Error connecting to {}",
        *DATABASE_CONNECTION_STRING
    ));

    // Parsing config file
    let config = read_config_file(&index_config.config);

    // Refactor these 4 functions as function of DDL Gen Plugin Struct
    migrate_with_ddl_gen_plugin(&params.index_name, &index_config.schema, &index_config.config); // Create tables for the new index
    track_hasura_with_ddl_gen_plugin(&params.index_name).await; // Track the newly created tables in hasura
    create_indexers_table_if_not_exists(&connection); // Create indexers table so we can keep track of the indexers status. TODO: Refactor as part of ddl gen plugin
    insert_new_indexer(&connection, &params.index_name, &config);  // Create a new indexer so we can keep track of it's status

    // Use correct chain type based on config
    let chain_type = match config["dataSources"][0]["kind"].as_str().unwrap() {
        "substrate" => ChainType::Substrate,
        "solana" => ChainType::Solana,
        _ => ChainType::Substrate, // If not provided, assume it's substrate network
    };

    // Chain Reader Client Configuration to subscribe and get latest block from Chain Reader Server
    let mut client = StreamoutClient::connect(CHAIN_READER_URL.clone())
        .await
        .unwrap();

    let get_blocks_request = GetBlocksRequest {
        start_block_number: 0,
        end_block_number: 1,
        chain_type: chain_type as i32,
    };
    let mut stream = client
        .list_blocks(Request::new(get_blocks_request))
        .await?
        .into_inner();

    // Subscribe new blocks
    log::info!("[Index Manager Helper] Start processing block");
    while let Some(data) = stream.message().await? {
        let now = Instant::now();
        let mut data = data as GenericDataProto;
        log::info!("[Index Manager Helper] Received chain: {:?}, data block = {:?}, hash = {:?}, data type = {:?}",
                 ChainType::from_i32(data.chain_type).unwrap(),
                 data.block_number,
                 data.block_hash,
                 DataType::from_i32(data.data_type).unwrap());

        // Need to refactor this or this will be called every time a new block comes
        let mut plugins = PluginManager::new(&mut store);
        unsafe {
            plugins.load("1234", index_config.mapping.clone()).unwrap();
        }

        match chain_type {
            ChainType::Substrate => {
                match DataType::from_i32(data.data_type) {
                    Some(DataType::Block) => {
                        let block: SubstrateBlock = decode(&mut data.payload).unwrap();
                        println!("Received BLOCK: {:?}", &block.block.header.number);
                        let extrinsics = get_extrinsics_from_block(&block);
                        for extrinsic in extrinsics {
                            println!("Received EXTRINSIC: {:?}", extrinsic);
                            plugins.handle_substrate_extrinsic("1234", &extrinsic);
                        }
                        plugins.handle_substrate_block("1234", &block);
                    }
                    Some(DataType::Event) => {
                        let event: SubstrateEventRecord = decode(&mut data.payload).unwrap();
                        println!("Received Event: {:?}", event);
                        plugins.handle_substrate_event("1234", &event);
                    }
                    // Some(DataType::Transaction) => {}
                    _ => {
                        println!("Not support data type: {:?}", &data.data_type);
                    }
                } // End of Substrate i32 data
            } // End of Substrate type
            ChainType::Solana => {
                match DataType::from_i32(data.data_type) {
                    Some(DataType::Block) => {
                        let encoded_block: SolanaEncodedBlock = solana_decode(&mut data.payload).unwrap();
                        let block = convert_solana_encoded_block_to_solana_block(encoded_block); // Decoding
                        //let rc_block = Arc::new(block.clone());
                        println!("Received SOLANA BLOCK with block height: {:?}, hash: {:?}", &block.block.block_height.unwrap(), &block.block.blockhash);
                        plugins.handle_solana_block("1234", &block);

                        let mut print_flag = true;
                        for origin_transaction in block.clone().block.transactions {
                            let origin_log_messages = origin_transaction.meta.clone().unwrap().log_messages;
                            let transaction = SolanaTransaction {
                                block_number: ((&block).block.block_height.unwrap() as u32),
                                transaction: origin_transaction.clone(),
                                //block: rc_block.clone(),
                                log_messages: origin_log_messages.clone(),
                                success: false
                            };

                            let log_messages = SolanaLogMessages {
                                block_number: ((&block).block.block_height.unwrap() as u32),
                                log_messages: origin_log_messages.clone(),
                                transaction: origin_transaction.clone(),
                            };
                            if print_flag {
                                //println!("Received Solana transaction & log messages");
                                println!("Recieved SOLANA TRANSACTION with Block number: {:?}, transaction: {:?}", &transaction.block_number, &transaction.transaction.transaction.signatures);
                                println!("Recieved SOLANA LOG_MESSAGES with Block number: {:?}, log_messages: {:?}", &log_messages.block_number, &log_messages.log_messages.clone().unwrap().get(0));
                                print_flag = false;
                            }
                            plugins.handle_solana_transaction("1234", &transaction);
                            plugins.handle_solana_log_messages("1234", &log_messages);
                        }
                    },
                    _ => {
                        println!("Not support type in Solana");
                    }
                } // End of Solana i32 data
            }, // End of Solana type
            _ => {
                println!("Not support this package chain-type");
            }
        }
        let elapsed = now.elapsed();
        println!("Elapsed processing block: {:.2?}", elapsed);
    }
    Ok(())
}

// Return indexer list
pub async fn list_handler_helper() -> Result<Vec<Indexer>, Box<dyn Error>> {
    // Create indexers table if it doesn't exists. We should do this with migration at the start.
    let connection = PgConnection::establish(&DATABASE_CONNECTION_STRING).expect(&format!(
        "Error connecting to {}",
        *DATABASE_CONNECTION_STRING
    ));
    create_indexers_table_if_not_exists(&connection);

    // User postgre lib for easy query
    let client =
        PostgreConnection::connect(DATABASE_CONNECTION_STRING.clone(), TlsMode::None).unwrap();
    let mut indexers: Vec<Indexer> = Vec::new();

    for row in &client
        .query("SELECT id, network, name FROM indexers", &[])
        .unwrap()
    {
        let indexer = Indexer {
            id: row.get(0),
            network: row.get(1),
            name: row.get(2),
        };
        indexers.push(indexer);
    }

    Ok(indexers)
}

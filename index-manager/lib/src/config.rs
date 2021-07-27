/**
*** Objective of this file is to parse the config project.yaml file to get
*** information like: chain type, index name, ...
**/

// Generic dependencies
use serde_yaml::Value;
// Massbit dependencies
use crate::types::stream_mod::{ChainType};

pub fn get_chain_type(config: &Value) -> ChainType {
    let chain_type = match config["dataSources"][0]["kind"].as_str().unwrap() {
        "substrate" => ChainType::Substrate,
        "solana" => ChainType::Solana,
        _ => ChainType::Substrate, // If not provided, assume it's substrate network
    };
    chain_type
}
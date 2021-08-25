//use crate::graph::components::store::StoredDynamicDataSource;
use crate::graph::prelude::CheapClone;
use crate::store::model::BlockNumber;
use massbit_common::prelude::{
    anyhow::{self, anyhow, ensure, Context, Error},
    async_trait::async_trait,
    ethabi::{Address, Event, Function, LogParam, ParamType, RawLog},
    serde_derive::{Deserialize, Serialize},
    serde_json, serde_yaml,
};
use slog::{info, trace};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::{convert::TryFrom, sync::Arc};
use tiny_keccak::keccak256;
use web3::types::{Log, Transaction, H256};
/*
use crate::graph::data::subgraph::{
    BlockHandlerFilter, DataSourceContext, Mapping, MappingABI, MappingBlockHandler,
    MappingCallHandler, MappingEventHandler, Source, TemplateSource, UnresolvedMapping,
};
*/
use super::trigger::{EthereumBlockTriggerType, EthereumTrigger, MappingTrigger};
use super::Chain;
use crate::chain::ethereum::types::{EthereumCall, LightEthereumBlock};
use crate::indexer::blockchain;
use crate::indexer::blockchain::Blockchain;
use crate::indexer::manifest::{
    self, BlockHandlerFilter, DataSourceContext, DataSourceTemplateInfo, LinkResolver, Mapping,
    MappingABI, MappingBlockHandler, MappingCallHandler, MappingEventHandler, Source,
    StoredDynamicDataSource, TemplateSource, UnresolvedMapping,
};
use crate::prelude::Logger;
use crate::store::Entity;
use massbit_common::prelude::serde_yaml::Value;
use semver::Version;

const API_VERSION_0_0_4: Version = Version::new(0, 0, 4);
const API_VERSION_0_0_5: Version = Version::new(0, 0, 5);

/// Runtime representation of a data source.
// Note: Not great for memory usage that this needs to be `Clone`, considering how there may be tens
// of thousands of data sources in memory at once.
#[derive(Clone, Debug)]
pub struct DataSource {
    pub kind: String,
    pub network: Option<String>,
    pub name: String,
    pub source: Source,
    pub mapping: Mapping,
    pub context: Arc<Option<DataSourceContext>>,
    pub creation_block: Option<BlockNumber>,
    //pub contract_abi: Arc<MappingABI>,
}

impl blockchain::DataSource<Chain> for DataSource {
    fn address(&self) -> Option<&[u8]> {
        self.source.address.as_ref().map(|x| x.as_bytes())
    }

    fn start_block(&self) -> BlockNumber {
        self.source.start_block
    }
    /*
    fn match_and_decode(
        &self,
        trigger: &<Chain as Blockchain>::TriggerData,
        block: Arc<<Chain as Blockchain>::Block>,
        logger: &Logger,
    ) -> Result<Option<<Chain as Blockchain>::MappingTrigger>, Error> {
        let block = block.light_block();
        self.match_and_decode(trigger, block, logger)
    }
    */
    fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    fn from_manifest(
        kind: String,
        network: Option<String>,
        name: String,
        source: Source,
        mapping: Mapping,
        context: Option<DataSourceContext>,
    ) -> Result<Self, Error> {
        // Data sources in the manifest are created "before genesis" so they have no creation block.
        let creation_block = None;
        let contract_abi = mapping
            .find_abi(&source.abi)
            .with_context(|| format!("data source `{}`", name))?;

        Ok(DataSource {
            kind,
            network,
            name,
            source,
            mapping,
            context: Arc::new(context),
            creation_block,
            //contract_abi,
        })
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    fn network(&self) -> Option<&str> {
        self.network.as_ref().map(|s| s.as_str())
    }

    fn context(&self) -> Arc<Option<DataSourceContext>> {
        self.context.cheap_clone()
    }

    fn creation_block(&self) -> Option<BlockNumber> {
        self.creation_block
    }

    fn is_duplicate_of(&self, other: &Self) -> bool {
        let DataSource {
            kind,
            network,
            name,
            source,
            mapping,
            context,

            // The creation block is ignored for detection duplicate data sources.
            // Contract ABI equality is implicit in `source` and `mapping.abis` equality.
            creation_block: _,
            //contract_abi: _,
        } = self;

        // mapping_request_sender, host_metrics, and (most of) host_exports are operational structs
        // used at runtime but not needed to define uniqueness; each runtime host should be for a
        // unique data source.
        kind == &other.kind
            && network == &other.network
            && name == &other.name
            && source == &other.source
            && mapping.abis == other.mapping.abis
            && mapping.event_handlers == other.mapping.event_handlers
            && mapping.call_handlers == other.mapping.call_handlers
            && mapping.block_handlers == other.mapping.block_handlers
            && context == &other.context
    }

    fn as_stored_dynamic_data_source(&self) -> StoredDynamicDataSource {
        StoredDynamicDataSource {
            name: self.name.to_owned(),
            source: self.source.clone(),
            context: self
                .context
                .as_ref()
                .as_ref()
                .map(|ctx| serde_json::to_string(&ctx).unwrap()),
            creation_block: self.creation_block,
        }
    }

    fn from_stored_dynamic_data_source(
        templates: &BTreeMap<&str, &DataSourceTemplate>,
        stored: StoredDynamicDataSource,
    ) -> Result<Self, Error> {
        let StoredDynamicDataSource {
            name,
            source,
            context,
            creation_block,
        } = stored;
        let template = templates
            .get(name.as_str())
            .ok_or_else(|| anyhow!("no template named `{}` was found", name))?;
        let context = context
            .map(|ctx| serde_json::from_str::<Entity>(&ctx))
            .transpose()?;

        let contract_abi = template.mapping.find_abi(&template.source.abi)?;

        Ok(DataSource {
            kind: template.kind.to_string(),
            network: template.network.as_ref().map(|s| s.to_string()),
            name,
            source,
            mapping: template.mapping.clone(),
            context: Arc::new(context),
            creation_block,
            //contract_abi,
        })
    }
}

impl DataSource {
    fn handlers_for_log(&self, log: &Log) -> Result<Vec<MappingEventHandler>, Error> {
        // Get signature from the log
        let topic0 = log.topics.get(0).context("Ethereum event has no topics")?;

        let handlers = self
            .mapping
            .event_handlers
            .iter()
            .filter(|handler| *topic0 == handler.topic0())
            .cloned()
            .collect::<Vec<_>>();

        Ok(handlers)
    }

    fn handler_for_call(&self, call: &EthereumCall) -> Result<Option<MappingCallHandler>, Error> {
        // First four bytes of the input for the call are the first four
        // bytes of hash of the function signature
        ensure!(
            call.input.0.len() >= 4,
            "Ethereum call has input with less than 4 bytes"
        );

        let target_method_id = &call.input.0[..4];

        Ok(self
            .mapping
            .call_handlers
            .iter()
            .find(move |handler| {
                let fhash = keccak256(handler.function.as_bytes());
                let actual_method_id = [fhash[0], fhash[1], fhash[2], fhash[3]];
                target_method_id == actual_method_id
            })
            .cloned())
    }

    fn handler_for_block(
        &self,
        trigger_type: &EthereumBlockTriggerType,
    ) -> Option<MappingBlockHandler> {
        match trigger_type {
            EthereumBlockTriggerType::Every => self
                .mapping
                .block_handlers
                .iter()
                .find(move |handler| handler.filter == None)
                .cloned(),
            EthereumBlockTriggerType::WithCallTo(_address) => self
                .mapping
                .block_handlers
                .iter()
                .find(move |handler| handler.filter == Some(BlockHandlerFilter::Call))
                .cloned(),
        }
    }
    /*
    /// Returns the contract event with the given signature, if it exists. A an event from the ABI
    /// will be matched if:
    /// 1. An event signature is equal to `signature`.
    /// 2. There are no equal matches, but there is exactly one event that equals `signature` if all
    ///    `indexed` modifiers are removed from the parameters.
    fn contract_event_with_signature(&self, signature: &str) -> Option<&Event> {
        // Returns an `Event(uint256,address)` signature for an event, without `indexed` hints.
        fn ambiguous_event_signature(event: &Event) -> String {
            format!(
                "{}({})",
                event.name,
                event
                    .inputs
                    .iter()
                    .map(|input| format!("{}", event_param_type_signature(&input.kind)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }

        // Returns an `Event(indexed uint256,address)` type signature for an event.
        fn event_signature(event: &Event) -> String {
            format!(
                "{}({})",
                event.name,
                event
                    .inputs
                    .iter()
                    .map(|input| format!(
                        "{}{}",
                        if input.indexed { "indexed " } else { "" },
                        event_param_type_signature(&input.kind)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }

        // Returns the signature of an event parameter type (e.g. `uint256`).
        fn event_param_type_signature(kind: &ParamType) -> String {
            use ParamType::*;

            match kind {
                Address => "address".into(),
                Bytes => "bytes".into(),
                Int(size) => format!("int{}", size),
                Uint(size) => format!("uint{}", size),
                Bool => "bool".into(),
                String => "string".into(),
                Array(inner) => format!("{}[]", event_param_type_signature(&*inner)),
                FixedBytes(size) => format!("bytes{}", size),
                FixedArray(inner, size) => {
                    format!("{}[{}]", event_param_type_signature(&*inner), size)
                }
                Tuple(components) => format!(
                    "({})",
                    components
                        .iter()
                        .map(|component| event_param_type_signature(&component))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            }
        }

        self.contract_abi
            .contract
            .events()
            .find(|event| event_signature(event) == signature)
            .or_else(|| {
                // Fallback for subgraphs that don't use `indexed` in event signatures yet:
                //
                // If there is only one event variant with this name and if its signature
                // without `indexed` matches the event signature from the manifest, we
                // can safely assume that the event is a match, we don't need to force
                // the subgraph to add `indexed`.

                // Extract the event name; if there is no '(' in the signature,
                // `event_name` will be empty and not match any events, so that's ok
                let parens = signature.find("(").unwrap_or(0);
                let event_name = &signature[0..parens];

                let matching_events = self
                    .contract_abi
                    .contract
                    .events()
                    .filter(|event| event.name == event_name)
                    .collect::<Vec<_>>();

                // Only match the event signature without `indexed` if there is
                // only a single event variant
                if matching_events.len() == 1
                    && ambiguous_event_signature(matching_events[0]) == signature
                {
                    Some(matching_events[0])
                } else {
                    // More than one event variant or the signature
                    // still doesn't match, even if we ignore `indexed` hints
                    None
                }
            })
    }
    fn contract_function_with_signature(&self, target_signature: &str) -> Option<&Function> {
        self.contract_abi
            .contract
            .functions()
            .filter(|function| match function.state_mutability {
                ethabi::StateMutability::Payable | ethabi::StateMutability::NonPayable => true,
                ethabi::StateMutability::Pure | ethabi::StateMutability::View => false,
            })
            .find(|function| {
                // Construct the argument function signature:
                // `address,uint256,bool`
                let mut arguments = function
                    .inputs
                    .iter()
                    .map(|input| format!("{}", input.kind))
                    .collect::<Vec<String>>()
                    .join(",");
                // `address,uint256,bool)
                arguments.push_str(")");
                // `operation(address,uint256,bool)`
                let actual_signature = vec![function.name.clone(), arguments].join("(");
                target_signature == actual_signature
            })
    }
    */
    fn matches_trigger_address(&self, trigger: &EthereumTrigger) -> bool {
        let ds_address = match self.source.address {
            Some(addr) => addr,

            // 'wildcard' data sources match any trigger address.
            None => return true,
        };

        let trigger_address = match trigger {
            EthereumTrigger::Block(_, EthereumBlockTriggerType::WithCallTo(address)) => address,
            EthereumTrigger::Call(call) => &call.to,
            EthereumTrigger::Log(log) => &log.address,

            // Unfiltered block triggers match any data source address.
            EthereumTrigger::Block(_, EthereumBlockTriggerType::Every) => return true,
        };

        ds_address == *trigger_address
    }
    /*
    /// Checks if `trigger` matches this data source, and if so decodes it into a `MappingTrigger`.
    /// A return of `Ok(None)` mean the trigger does not match.
    fn match_and_decode(
        &self,
        trigger: &EthereumTrigger,
        block: Arc<LightEthereumBlock>,
        logger: &Logger,
    ) -> Result<Option<MappingTrigger>, Error> {
        if !self.matches_trigger_address(&trigger) {
            return Ok(None);
        }

        if self.source.start_block > block.number() {
            return Ok(None);
        }

        match trigger {
            EthereumTrigger::Block(_, trigger_type) => {
                let handler = match self.handler_for_block(trigger_type) {
                    Some(handler) => handler,
                    None => return Ok(None),
                };
                Ok(Some(MappingTrigger::Block { block, handler }))
            }
            EthereumTrigger::Log(log) => {
                let potential_handlers = self.handlers_for_log(log)?;

                // Map event handlers to (event handler, event ABI) pairs; fail if there are
                // handlers that don't exist in the contract ABI
                let valid_handlers = potential_handlers
                    .into_iter()
                    .map(|event_handler| {
                        // Identify the event ABI in the contract
                        let event_abi = self
                            .contract_event_with_signature(event_handler.event.as_str())
                            .with_context(|| {
                                anyhow!(
                                    "Event with the signature \"{}\" not found in \
                                            contract \"{}\" of data source \"{}\"",
                                    event_handler.event,
                                    self.contract_abi.name,
                                    self.name,
                                )
                            })?;
                        Ok((event_handler, event_abi))
                    })
                    .collect::<Result<Vec<_>, anyhow::Error>>()?;

                // Filter out handlers whose corresponding event ABIs cannot decode the
                // params (this is common for overloaded events that have the same topic0
                // but have indexed vs. non-indexed params that are encoded differently).
                //
                // Map (handler, event ABI) pairs to (handler, decoded params) pairs.
                let mut matching_handlers = valid_handlers
                    .into_iter()
                    .filter_map(|(event_handler, event_abi)| {
                        event_abi
                            .parse_log(RawLog {
                                topics: log.topics.clone(),
                                data: log.data.clone().0,
                            })
                            .map(|log| log.params)
                            .map_err(|e| {
                                trace!(
                                    logger,
                                    "Skipping handler because the event parameters do not \
                                    match the event signature. This is typically the case \
                                    when parameters are indexed in the event but not in the \
                                    signature or the other way around";
                                    "handler" => &event_handler.handler,
                                    "event" => &event_handler.event,
                                    "error" => format!("{}", e),
                                );
                            })
                            .ok()
                            .map(|params| (event_handler, params))
                    })
                    .collect::<Vec<_>>();

                if matching_handlers.is_empty() {
                    return Ok(None);
                }

                // Process the event with the matching handler
                let (event_handler, params) = matching_handlers.pop().unwrap();

                ensure!(
                    matching_handlers.is_empty(),
                    format!(
                        "Multiple handlers defined for event `{}`, only one is supported",
                        &event_handler.event
                    )
                );

                // Special case: In Celo, there are Epoch Rewards events, which do not have an
                // associated transaction and instead have `transaction_hash == block.hash`,
                // in which case we pass a dummy transaction to the mappings.
                // See also ca0edc58-0ec5-4c89-a7dd-2241797f5e50.
                let transaction = if log.transaction_hash != block.hash {
                    block
                        .transaction_for_log(&log)
                        .context("Found no transaction for event")?
                } else {
                    // Infer some fields from the log and fill the rest with zeros.
                    Transaction {
                        hash: log.transaction_hash.unwrap(),
                        block_hash: block.hash,
                        block_number: block.number,
                        transaction_index: log.transaction_index,
                        ..Transaction::default()
                    }
                };

                Ok(Some(MappingTrigger::Log {
                    block,
                    transaction: Arc::new(transaction),
                    log: log.cheap_clone(),
                    params,
                    handler: event_handler,
                }))
            }
            EthereumTrigger::Call(call) => {
                // Identify the call handler for this call
                let handler = match self.handler_for_call(&call)? {
                    Some(handler) => handler,
                    None => return Ok(None),
                };

                // Identify the function ABI in the contract
                let function_abi = self
                    .contract_function_with_signature(handler.function.as_str())
                    .with_context(|| {
                        anyhow!(
                            "Function with the signature \"{}\" not found in \
                    contract \"{}\" of data source \"{}\"",
                            handler.function,
                            self.contract_abi.name,
                            self.name
                        )
                    })?;

                // Parse the inputs
                //
                // Take the input for the call, chop off the first 4 bytes, then call
                // `function.decode_input` to get a vector of `Token`s. Match the `Token`s
                // with the `Param`s in `function.inputs` to create a `Vec<LogParam>`.
                let tokens = function_abi
                    .decode_input(&call.input.0[4..])
                    .with_context(|| {
                        format!(
                            "Generating function inputs for the call {:?} failed, raw input: {}",
                            &function_abi,
                            hex::encode(&call.input.0)
                        )
                    })?;

                ensure!(
                    tokens.len() == function_abi.inputs.len(),
                    "Number of arguments in call does not match \
                    number of inputs in function signature."
                );

                let inputs = tokens
                    .into_iter()
                    .enumerate()
                    .map(|(i, token)| LogParam {
                        name: function_abi.inputs[i].name.clone(),
                        value: token,
                    })
                    .collect::<Vec<_>>();

                // Parse the outputs
                //
                // Take the output for the call, then call `function.decode_output` to
                // get a vector of `Token`s. Match the `Token`s with the `Param`s in
                // `function.outputs` to create a `Vec<LogParam>`.
                let tokens = function_abi
                    .decode_output(&call.output.0)
                    .with_context(|| {
                        format!(
                            "Decoding function outputs for the call {:?} failed, raw output: {}",
                            &function_abi,
                            hex::encode(&call.output.0)
                        )
                    })?;

                ensure!(
                    tokens.len() == function_abi.outputs.len(),
                    "Number of parameters in the call output does not match \
                        number of outputs in the function signature."
                );

                let outputs = tokens
                    .into_iter()
                    .enumerate()
                    .map(|(i, token)| LogParam {
                        name: function_abi.outputs[i].name.clone(),
                        value: token,
                    })
                    .collect::<Vec<_>>();

                let transaction = Arc::new(
                    block
                        .transaction_for_call(&call)
                        .context("Found no transaction for call")?,
                );

                Ok(Some(MappingTrigger::Call {
                    block,
                    transaction,
                    call: call.cheap_clone(),
                    inputs,
                    outputs,
                    handler,
                }))
            }
        }
    }
    //End match_and_decode
    */
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct UnresolvedDataSource {
    pub kind: String,
    pub network: Option<String>,
    pub name: String,
    pub source: Source,
    pub mapping: UnresolvedMapping,
    pub context: Option<DataSourceContext>,
}

#[async_trait]
impl blockchain::UnresolvedDataSource<Chain> for UnresolvedDataSource {
    async fn resolve(
        self,
        resolver: &impl LinkResolver,
        logger: &Logger,
    ) -> Result<DataSource, anyhow::Error> {
        let UnresolvedDataSource {
            kind,
            network,
            name,
            source,
            mapping,
            context,
        } = self;

        info!(logger, "Resolve data source"; "name" => &name, "source" => &source.start_block);

        let mapping = mapping.resolve(&*resolver, logger).await?;

        manifest::DataSource::from_manifest(kind, network, name, source, mapping, context)
    }
}

impl TryFrom<DataSourceTemplateInfo<Chain>> for DataSource {
    type Error = anyhow::Error;

    fn try_from(info: DataSourceTemplateInfo<Chain>) -> Result<Self, anyhow::Error> {
        let DataSourceTemplateInfo {
            template,
            params,
            context,
            creation_block,
        } = info;

        // Obtain the address from the parameters
        let string = params
            .get(0)
            .with_context(|| {
                format!(
                    "Failed to create data source from template `{}`: address parameter is missing",
                    template.name
                )
            })?
            .trim_start_matches("0x");

        let address = Address::from_str(string).with_context(|| {
            format!(
                "Failed to create data source from template `{}`, invalid address provided",
                template.name
            )
        })?;

        let contract_abi = template
            .mapping
            .find_abi(&template.source.abi)
            .with_context(|| format!("template `{}`", template.name))?;

        Ok(DataSource {
            kind: template.kind,
            network: template.network,
            name: template.name,
            source: Source {
                address: Some(address),
                abi: template.source.abi,
                start_block: 0,
            },
            mapping: template.mapping,
            context: Arc::new(context),
            creation_block: Some(creation_block),
            //contract_abi,
        })
    }
}
impl TryFrom<&serde_yaml::Value> for DataSource {
    type Error = anyhow::Error;

    fn try_from(value: &Value) -> Result<Self, anyhow::Error> {
        let network = match &value["network"] {
            Value::String(val) => Some(val.clone()),
            _ => None,
        };
        //source
        let map: &Value = &value["source"];
        let address = match &map["address"] {
            Value::String(addr) => {
                //let vec = hex::decode(addr).unwrap();
                Some(
                    Address::from_str(addr.trim_start_matches("0x")).with_context(|| {
                        format!(
                            "Failed to create address from value `{}`, invalid address provided",
                            addr
                        )
                    })?,
                )
            }
            _ => None,
        };
        let source = Source {
            address,
            abi: map["abi"].as_str().unwrap_or("").to_string(),
            start_block: map["startBlock"].as_i64().unwrap_or(0) as i32,
        };

        let map = &value["mapping"];

        let block_handlers = match map["blockHandlers"].as_sequence() {
            Some(seqs) => seqs
                .iter()
                .map(|val| MappingBlockHandler {
                    handler: val["handler"].as_str().unwrap().to_string(),
                    filter: None,
                })
                .collect::<Vec<MappingBlockHandler>>(),
            _ => Vec::default(),
        };
        let call_handlers = match map["callHandlers"].as_sequence() {
            Some(seqs) => seqs
                .iter()
                .map(|val| MappingCallHandler {
                    function: val["function"].as_str().unwrap().to_string(),
                    handler: val["handler"].as_str().unwrap().to_string(),
                })
                .collect::<Vec<MappingCallHandler>>(),
            _ => Vec::default(),
        };
        let event_handlers = match map["eventHandlers"].as_sequence() {
            Some(seqs) => seqs
                .iter()
                .map(|val| MappingEventHandler {
                    event: val["event"].as_str().unwrap().to_string(),
                    topic0: None,
                    handler: val["handler"].as_str().unwrap().to_string(),
                })
                .collect::<Vec<MappingEventHandler>>(),
            _ => Vec::default(),
        };
        let mapping = Mapping {
            kind: map["kind"].as_str().unwrap_or("").to_string(),
            api_version: Version::new(0, 0, 4),
            language: map["language"].as_str().unwrap_or("rust").to_string(),
            entities: vec![],
            abis: vec![],
            block_handlers,
            call_handlers,
            event_handlers,
            runtime: Arc::new(vec![]),
            //link: Default::default(),
        };
        Ok(DataSource {
            kind: value["kind"].as_str().unwrap_or("").to_string(),
            network,
            name: value["name"].as_str().unwrap_or("").to_string(),
            source,
            mapping,
            context: Arc::new(None),
            creation_block: None,
            /*
            contract_abi: Arc::new(MappingABI {
                name: "".to_string(),
                contract: Default::default(),
            }),
             */
        })
    }
}

#[derive(Clone, Debug, Default, Hash, Eq, PartialEq, Deserialize)]
pub struct BaseDataSourceTemplate<M> {
    pub kind: String,
    pub network: Option<String>,
    pub name: String,
    pub source: TemplateSource,
    pub mapping: M,
}

pub type UnresolvedDataSourceTemplate = BaseDataSourceTemplate<UnresolvedMapping>;
pub type DataSourceTemplate = BaseDataSourceTemplate<Mapping>;

impl TryFrom<&serde_yaml::Value> for DataSourceTemplate {
    type Error = anyhow::Error;

    fn try_from(value: &Value) -> Result<Self, anyhow::Error> {
        let network = match &value["network"] {
            Value::String(val) => Some(val.clone()),
            _ => None,
        };
        //TemplateSource
        let map: &Value = &value["source"];
        let source = TemplateSource {
            abi: map["abi"].as_str().unwrap_or("").to_string(),
        };

        let map = &value["mapping"];

        let block_handlers = match map["blockHandlers"].as_sequence() {
            Some(seqs) => seqs
                .iter()
                .map(|val| MappingBlockHandler {
                    handler: val["handler"].as_str().unwrap().to_string(),
                    filter: None,
                })
                .collect::<Vec<MappingBlockHandler>>(),
            _ => Vec::default(),
        };
        let call_handlers = match map["callHandlers"].as_sequence() {
            Some(seqs) => seqs
                .iter()
                .map(|val| MappingCallHandler {
                    function: val["function"].as_str().unwrap().to_string(),
                    handler: val["handler"].as_str().unwrap().to_string(),
                })
                .collect::<Vec<MappingCallHandler>>(),
            _ => Vec::default(),
        };
        let event_handlers = match map["eventHandlers"].as_sequence() {
            Some(seqs) => seqs
                .iter()
                .map(|val| MappingEventHandler {
                    event: val["event"].as_str().unwrap().to_string(),
                    topic0: None,
                    handler: val["handler"].as_str().unwrap().to_string(),
                })
                .collect::<Vec<MappingEventHandler>>(),
            _ => Vec::default(),
        };
        let mapping = Mapping {
            kind: map["kind"].as_str().unwrap_or("").to_string(),
            api_version: Version::new(0, 0, 4),
            language: map["language"].as_str().unwrap_or("rust").to_string(),
            entities: vec![],
            abis: vec![],
            block_handlers,
            call_handlers,
            event_handlers,
            runtime: Arc::new(vec![]),
            //link: Default::default(),
        };
        Ok(DataSourceTemplate {
            kind: value["kind"].as_str().unwrap_or("").to_string(),
            network,
            name: value["name"].as_str().unwrap_or("").to_string(),
            source,
            mapping,
        })
    }
}

#[async_trait]
impl blockchain::UnresolvedDataSourceTemplate<Chain> for UnresolvedDataSourceTemplate {
    async fn resolve(
        self,
        resolver: &impl LinkResolver,
        logger: &Logger,
    ) -> Result<DataSourceTemplate, anyhow::Error> {
        let UnresolvedDataSourceTemplate {
            kind,
            network,
            name,
            source,
            mapping,
        } = self;

        info!(logger, "Resolve data source template"; "name" => &name);

        Ok(DataSourceTemplate {
            kind,
            network,
            name,
            source,
            mapping: mapping.resolve(resolver, logger).await?,
        })
    }
}

impl blockchain::DataSourceTemplate<Chain> for DataSourceTemplate {
    fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    fn name(&self) -> &str {
        &self.name
    }
}
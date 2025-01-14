use crate::core::{AdapterError, MessageHandler};
pub use crate::stream_mod::{DataType, GenericDataProto};
use index_store::Store;
use libloading::Library;
use massbit_chain_substrate::data_type::{
    decode, get_extrinsics_from_block, SubstrateBlock, SubstrateEventRecord,
    SubstrateUncheckedExtrinsic,
};
use std::{error::Error, sync::Arc};
crate::prepare_adapter!(Substrate, {
     handle_block:SubstrateBlock,
     handle_extrinsic:SubstrateUncheckedExtrinsic,
     handle_event:SubstrateEventRecord
});
impl MessageHandler for SubstrateHandlerProxy {
    fn handle_rust_mapping(
        &self,
        data: &mut GenericDataProto,
        store: &mut dyn Store,
    ) -> Result<(), Box<dyn Error>> {
        match DataType::from_i32(data.data_type) {
            Some(DataType::Block) => {
                let block: SubstrateBlock = decode(&mut data.payload).unwrap();
                //println!("Received BLOCK: {:?}", &block.block.header.number);
                log::info!(
                    "{} Received BLOCK: {:?}",
                    &*COMPONENT_NAME,
                    &block.block.header.number
                );
                let extrinsics = get_extrinsics_from_block(&block);
                for extrinsic in extrinsics {
                    log::info!("{} Received EXTRINSIC: {:?}", &*COMPONENT_NAME, extrinsic);
                    self.handler.handle_extrinsic(&extrinsic);
                }
                self.handler.handle_block(&block);
                store.flush(&data.block_hash, data.block_number)
            }
            Some(DataType::Event) => {
                let event: SubstrateEventRecord = decode(&mut data.payload).unwrap();
                log::info!("{} Received Event: {:?}", &*COMPONENT_NAME, event);
                self.handler.handle_event(&event);
                store.flush(&data.block_hash, data.block_number)
            }
            _ => {
                log::warn!(
                    "{} Not support data type: {:?}",
                    &*COMPONENT_NAME,
                    &data.data_type
                );
                Err(Box::new(AdapterError::new(
                    format!("Not support data type: {:?}", &data.data_type).as_str(),
                )))
            }
        } // End of Substrate i32 data
    }
}

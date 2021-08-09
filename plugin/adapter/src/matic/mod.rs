use libloading::Library;
use std::{error::Error, sync::Arc};
pub use crate::stream_mod::{DataType, GenericDataProto};
use crate::core::{MessageHandler};

crate::prepare_adapter!(Matic, {

});

impl MessageHandler for MaticHandlerProxy {
    fn handle_message(&self, _data: &mut GenericDataProto) -> Result<(), Box<dyn Error>> {
        todo!()
    }
}

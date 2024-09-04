use anyhow::Result;
use serde::ser;
use std::fmt;

use crate::nmp_hdr::*;
use crate::transport_ble::{BluetoothSpecs, BluetoothTransport};
use crate::transport_serial::{SerialSpecs, SerialTransport};

// Error representing a chunk that is too large to be sent on the transport
#[derive(Debug)]
pub struct ErrTooLargeChunk(pub usize);
impl fmt::Display for ErrTooLargeChunk {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "too large chunk")
    }
}

pub trait SmpTransportImpl {
    // send SMP request frame bytes and receive response frame bytes
    // @req_frame SMP frame bytes to send
    // @return response SMP frame bytes
    fn transceive_raw(&mut self, req_frame: &Vec<u8>) -> Result<Vec<u8>>;

    // @return MTU of the transport
    fn mtu(&self) -> usize;

    // timeout for waiting response
    fn set_timeout(&mut self, timeout: std::time::Duration) -> Result<()>;
}

impl NmpId for u8 {
    fn to_u8(&self) -> u8 {
        *self
    }
}

pub struct SmpTransport {
    transport_impl: Box<dyn SmpTransportImpl>,
    seq_id: u8,
}

impl SmpTransport {
    fn new(transport_impl: Box<dyn SmpTransportImpl>) -> Self {
        SmpTransport {
            transport_impl,
            seq_id: rand::random::<u8>(),
        }
    }

    pub fn new_serial(specs: &SerialSpecs) -> Result<Self> {
        Ok(Self::new(Box::new(SerialTransport::new(specs)?)))
    }

    pub fn new_ble(specs: &BluetoothSpecs) -> Result<Self> {
        Ok(Self::new(Box::new(BluetoothTransport::new(specs)?)))
    }

    pub fn transceive(
        &mut self,
        op: NmpOp,
        group: NmpGroup,
        id: impl NmpId,
        req: &impl ser::Serialize,
    ) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)> {
        // cbor serialize request
        let req_body = serde_cbor::to_vec(req)?;
        // encode into NMP frame
        let mut req_header = NmpHdr::new_req(op, group, id);
        req_header.seq = self.seq_id;
        req_header.len = req_body.len() as u16;

        let mut req_frame = req_header.serialize()?;
        req_frame.extend(req_body);

        let rsp = match self.transport_impl.transceive_raw(&req_frame) {
            Err(e) if e.is::<ErrTooLargeChunk>() => {
                // don't increment seq_id if chunk is too large
                // because the request is never sent
                return Err(e);
            }
            Err(e) => {
                self.seq_id = self.seq_id.wrapping_add(1);
                return Err(e.into());
            }
            Ok(rsp) => {
                self.seq_id = self.seq_id.wrapping_add(1);
                rsp
            }
        };

        // parse response header and body
        let mut cursor = std::io::Cursor::new(&rsp);
        let rsp_header = NmpHdr::deserialize(&mut cursor)?;
        let rsp_body = serde_cbor::from_reader(cursor)?;
        Ok((req_header, rsp_header, rsp_body))
    }

    pub fn mtu(&self) -> usize {
        self.transport_impl.mtu()
    }

    pub fn set_timeout(&mut self, timeout: std::time::Duration) -> Result<()> {
        self.transport_impl.set_timeout(timeout)
    }
}

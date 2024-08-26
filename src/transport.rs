use anyhow::{anyhow, Result};
use serde::ser;
use std::fmt;

use crate::nmp_hdr::*;
use crate::transfer::*;

// Transport Error
#[derive(Debug)]
pub enum TransportError {
    // encoded frame is larger than the MTU
    // usize: the number of bytes that overflow
    TooLargeChunk(usize),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "too large chunk")
    }
}

pub trait NmpTransport {
    fn transceive<T>(
        &mut self,
        op: NmpOp,
        group: NmpGroup,
        id: impl NmpId,
        body: &T,
    ) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)>
    where
        T: ser::Serialize;
}

pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    linelength: usize,
    mtu: usize,
    seq_id: u8,
}

impl SerialTransport {
    pub fn new(specs: &SerialSpecs) -> Result<SerialTransport> {
        let port = open_port(specs)?;
        Ok(SerialTransport {
            port,
            linelength: specs.linelength,
            mtu: specs.mtu,
            seq_id: rand::random::<u8>(),
        })
    }
}

impl NmpTransport for SerialTransport {
    fn transceive<T>(
        &mut self,
        op: NmpOp,
        group: NmpGroup,
        id: impl NmpId,
        req: &T,
    ) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)>
    where
        T: ser::Serialize,
    {
        // convert to bytes with CBOR
        let req_body = serde_cbor::to_vec(req)?;
        let (data, request_header) =
            encode_request(self.linelength, op, group, id, &req_body, self.seq_id)?;
        if data.len() > self.mtu {
            let reduce = data.len() - self.mtu;
            return Err(anyhow!(TransportError::TooLargeChunk(reduce)));
        }

        self.seq_id = self.seq_id.wrapping_add(1);
        let (response_header, response_body) = transceive(&mut *self.port, &data)?;

        Ok((request_header, response_header, response_body))
    }
}

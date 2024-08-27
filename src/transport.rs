use anyhow::{anyhow, Result};
use serde::ser;
use std::fmt;

use crate::nmp_hdr::*;
use crate::transfer::*;
use crate::transport_ble::BluetoothTransport;

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
    // @body cbor encoded message
    fn transceive(
        &mut self,
        op: NmpOp,
        group: NmpGroup,
        id: u8,
        body_cbor: &Vec<u8>,
    ) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)>;

    fn mtu(&self) -> usize;
}

pub fn transceive(
    transport: &mut dyn NmpTransport,
    op: NmpOp,
    group: NmpGroup,
    id: impl NmpId,
    req: &impl ser::Serialize,
) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)> {
    // convert to bytes with CBOR
    let body_cbor = serde_cbor::to_vec(req)?;
    transport.transceive(op, group, id.to_u8(), &body_cbor)
}

struct SerialTransport {
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

impl NmpId for u8 {
    fn to_u8(&self) -> u8 {
        *self
    }
}

impl NmpTransport for SerialTransport {
    fn mtu(&self) -> usize {
        self.mtu * 3 / 4
    }

    fn transceive(
        &mut self,
        op: NmpOp,
        group: NmpGroup,
        id: u8,
        body: &Vec<u8>,
    ) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)> {
        // encode into serial frame
        let (frame, request_header) =
            encode_request(self.linelength, op, group, id, &body, self.seq_id)?;

        if frame.len() > self.mtu {
            let reduce = frame.len() - self.mtu;
            return Err(anyhow!(TransportError::TooLargeChunk(reduce)));
        }

        self.seq_id = self.seq_id.wrapping_add(1);

        let (response_header, response_body) = serial_transceive(&mut *self.port, &frame)?;
        Ok((request_header, response_header, response_body))
    }
}

pub fn open_transport(specs: &SerialSpecs) -> Result<Box<dyn NmpTransport>> {
    let transport: Box<dyn NmpTransport> = if specs.device.starts_with("bt:") {
        let id = specs.device[3..].to_string();
        Box::new(BluetoothTransport::open(&id)?)
    } else {
        Box::new(SerialTransport::new(specs)?)
    };
    Ok(transport)
}

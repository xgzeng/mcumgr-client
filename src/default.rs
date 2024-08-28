// Copyright © 2023-2024 Vouch.io LLC

use anyhow::{bail, Error, Result};
use log::debug;
use log::info;
use serde_cbor;
use serde_json;

use crate::nmp_hdr::*;
use crate::transport::{transceive, NmpTransport};

pub fn reset(transport: &mut dyn NmpTransport) -> Result<(), Error> {
    info!("send reset request");

    // send request
    let body = Vec::<u8>::new();
    let (request_header, response_header, response_body) = transceive(
        transport,
        NmpOp::Write,
        NmpGroup::Default,
        NmpIdDef::Reset,
        &body,
    )?;

    // verify sequence id
    if response_header.seq != request_header.seq {
        bail!("wrong sequence number");
    }

    // verify response
    if response_header.op != NmpOp::WriteRsp || response_header.group != NmpGroup::Default {
        bail!("wrong response types");
    }

    // verify result code
    debug!(
        "response_body: {}",
        serde_json::to_string_pretty(&response_body)?
    );
    if let serde_cbor::Value::Map(object) = response_body {
        for (key, val) in object.iter() {
            match key {
                serde_cbor::Value::Text(rc_key) if rc_key == "rc" => {
                    if let serde_cbor::Value::Integer(rc) = val {
                        if *rc != 0 {
                            bail!("rc = {}", rc);
                        } else {
                            info!("reset complete");
                        }
                    }
                }
                _ => (),
            }
        }
    }

    Ok(())
}

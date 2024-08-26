// Copyright © 2023-2024 Vouch.io LLC

use anyhow::{bail, Error, Result};
use humantime::format_duration;
use log::{debug, info, warn};
use serde_cbor;
use serde_json;
use sha2::{Digest, Sha256};
use std::fs::read;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crate::nmp_hdr::*;
use crate::transfer::SerialSpecs;
use crate::transport::{NmpTransport, SerialTransport, TransportError};

fn get_rc(response_body: &serde_cbor::Value) -> Option<u32> {
    let mut rc: Option<u32> = None;
    if let serde_cbor::Value::Map(object) = response_body {
        for (key, val) in object.iter() {
            match key {
                serde_cbor::Value::Text(rc_key) if rc_key == "rc" => {
                    if let serde_cbor::Value::Integer(parsed_rc) = val {
                        rc = Some(*parsed_rc as u32);
                    }
                }
                _ => (),
            }
        }
    }
    rc
}

fn check_answer(request_header: &NmpHdr, response_header: &NmpHdr) -> bool {
    // verify sequence id
    if response_header.seq != request_header.seq {
        log::debug!("wrong sequence number");
        return false;
    }

    let expected_op_type = match request_header.op {
        NmpOp::Read => NmpOp::ReadRsp,
        NmpOp::Write => NmpOp::WriteRsp,
        _ => return false,
    };

    // verify response
    if response_header.op != expected_op_type || response_header.group != request_header.group {
        log::debug!("wrong response types");
        return false;
    }

    true
}

pub fn erase(specs: &SerialSpecs, slot: Option<u32>) -> Result<(), Error> {
    info!("erase request");

    // open serial port
    let mut port = SerialTransport::new(specs)?;

    let req = ImageEraseReq { slot: slot };
    // send request
    let (request_header, response_header, response_body) =
        port.transceive(NmpOp::Write, NmpGroup::Image, NmpIdImage::Erase, &req)?;

    if !check_answer(&request_header, &response_header) {
        bail!("wrong answer types")
    }

    if let Some(rc) = get_rc(&response_body) {
        if rc != 0 {
            bail!("Error from device: {}", rc);
        }
    }

    log::debug!("{:?}", response_body);
    Ok(())
}

pub fn test(specs: &SerialSpecs, hash: Vec<u8>, confirm: Option<bool>) -> Result<(), Error> {
    info!("set image pending request");

    // open serial port
    let mut port = SerialTransport::new(specs)?;

    let req = ImageStateReq {
        hash: hash,
        confirm: confirm,
    };
    // send request
    let (request_header, response_header, response_body) =
        port.transceive(NmpOp::Write, NmpGroup::Image, NmpIdImage::State, &req)?;

    if !check_answer(&request_header, &response_header) {
        bail!("wrong answer types")
    }

    if let Some(rc) = get_rc(&response_body) {
        if rc != 0 {
            return Err(anyhow::format_err!("Error from device: {}", rc));
        }
    }

    log::debug!("{:?}", response_body);
    Ok(())
}

pub fn list(specs: &SerialSpecs) -> Result<ImageStateRsp, Error> {
    info!("send image list request");

    // open serial port
    let mut transport = SerialTransport::new(specs)?;

    // send request
    let req = std::collections::BTreeMap::<String, String>::new();

    let (request_header, response_header, response_body) =
        transport.transceive(NmpOp::Read, NmpGroup::Image, NmpIdImage::State, &req)?;

    if !check_answer(&request_header, &response_header) {
        bail!("wrong answer types")
    }

    let ans: ImageStateRsp = serde_cbor::value::from_value(response_body)
        .map_err(|e| anyhow::format_err!("unexpected answer from device | {}", e))?;

    Ok(ans)
}

pub fn upload<F>(
    specs: &SerialSpecs,
    filename: &PathBuf,
    slot: u8,
    mut progress: Option<F>,
) -> Result<(), Error>
where
    F: FnMut(u64, u64),
{
    let filename_string = filename.to_string_lossy();
    info!("upload file: {}", filename_string);

    // special feature: if the name contains "slot1" or "slot3", then use this slot
    let filename_lowercase = filename_string.to_lowercase();
    let mut slot = slot;
    if filename_lowercase.contains(&"slot1".to_lowercase()) {
        slot = 1;
    }
    if filename_lowercase.contains(&"slot3".to_lowercase()) {
        slot = 3;
    }
    info!("flashing to slot {}", slot);

    // open serial port
    let mut port = SerialTransport::new(specs)?;

    // load file
    let data = read(filename)?;
    info!("{} bytes to transfer", data.len());

    // transfer in blocks
    let mut off: usize = 0;
    let start_time = Instant::now();
    let mut sent_blocks: u32 = 0;
    let mut confirmed_blocks: u32 = 0;
    loop {
        let mut nb_retry = specs.nb_retry;
        let off_start = off;
        let mut try_length = specs.mtu;
        debug!("try_length: {}", try_length);
        loop {
            // get slot
            let image_num = slot;

            // create image upload request
            if off + try_length > data.len() {
                try_length = data.len() - off;
            }
            let chunk = data[off..off + try_length].to_vec();
            let len = data.len() as u32;
            let req = if off == 0 {
                ImageUploadReq {
                    image_num,
                    off: off as u32,
                    len: Some(len),
                    data_sha: Some(Sha256::digest(&data).to_vec()),
                    upgrade: None,
                    data: chunk,
                }
            } else {
                ImageUploadReq {
                    image_num,
                    off: off as u32,
                    len: None,
                    data_sha: None,
                    upgrade: None,
                    data: chunk,
                }
            };
            debug!("req: {:?}", req);

            // send request
            sent_blocks += 1;
            let (request_header, response_header, response_body) =
                match port.transceive(NmpOp::Write, NmpGroup::Image, NmpIdImage::Upload, &req) {
                    Ok(ret) => ret,
                    Err(e) if e.to_string() == "Operation timed out" => {
                        if nb_retry == 0 {
                            return Err(e);
                        }
                        nb_retry -= 1;
                        sent_blocks -= 1;
                        debug!("missed answer, nb_retry: {}", nb_retry);
                        continue;
                    }
                    Err(e) if e.is::<TransportError>() => {
                        match e.downcast::<TransportError>().unwrap() {
                            TransportError::TooLargeChunk(reduce) => {
                                if reduce > try_length {
                                    bail!("MTU too small");
                                }

                                // number of bytes to reduce is base64 encoded, calculate back the number of bytes
                                // and then reduce a bit more for base64 filling and rounding
                                try_length -= reduce * 3 / 4 + 3;
                                debug!("new try_length: {}", try_length);
                                sent_blocks -= 1;
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        return Err(e);
                    }
                };

            if !check_answer(&request_header, &response_header) {
                bail!("wrong answer types")
            }

            // verify result code and update offset
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
                                }
                            }
                        }
                        serde_cbor::Value::Text(off_key) if off_key == "off" => {
                            if let serde_cbor::Value::Integer(off_val) = val {
                                off = *off_val as usize;
                            }
                        }
                        _ => (),
                    }
                }
            }
            confirmed_blocks += 1;
            break;
        }

        // next chunk, next off should have been sent from the device
        if off_start == off {
            bail!("wrong offset received");
        }

        if let Some(ref mut f) = progress {
            f(off as u64, data.len() as u64);
        }

        //info!("{}% uploaded", 100 * off / data.len());
        if off == data.len() {
            break;
        }

        // The first packet was sent and the device has cleared its internal flash
        // We can now lower the timeout in case of failed transmission
        // port.set_timeout(Duration::from_millis(specs.subsequent_timeout_ms as u64))?;
    }

    let elapsed = start_time.elapsed().as_secs_f64().round();
    let elapsed_duration = Duration::from_secs(elapsed as u64);
    let formatted_duration = format_duration(elapsed_duration);
    info!("upload took {}", formatted_duration);
    if confirmed_blocks != sent_blocks {
        warn!(
            "upload packet loss {}%",
            100 - confirmed_blocks * 100 / sent_blocks
        );
    }

    Ok(())
}

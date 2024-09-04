use anyhow::{anyhow, Context, Result};
use bluest::{Adapter, Characteristic, Device};

use futures::stream::Stream;
use futures::stream::StreamExt;
use log::info;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;
use tokio::runtime::Runtime;
use uuid::Uuid;

use crate::nmp_hdr::*;
use crate::transport::{ErrTooLargeChunk, SmpTransportImpl};

// NMP Service UUID
const NMP_SERVICE_UUID: Uuid = uuid::uuid!("8D53DC1D-1DB7-4CD3-868B-8A527460AA84");
const NMP_CHARACTERISTIC_UUID: Uuid = uuid::uuid!("DA2E7828-FBCE-4E01-AE9E-261174997C48");

pub struct BluetoothSpecs {
    // device id or name
    pub device: String,
    // mtu can be bigger than chrc_mtu if device support assembly
    pub mtu: usize,
    // chrc_mtu is max bytes written to gatt characteristic
    // which is determined by device l2cap settings
    pub chrc_mtu: usize,
    pub timeout: Duration,
}

pub(crate) struct BluetoothTransport {
    runtime: Rc<Runtime>,
    _adapter: Adapter,
    _device: Device,
    chrc: Characteristic,
    response_stream: Box<dyn Stream<Item = bluest::Result<Vec<u8>>> + Unpin>,
    mtu: usize,
    chrc_mtu: usize,
    timeout: Duration,
}

async fn open_adapter() -> Result<Adapter> {
    let adapter = Adapter::default()
        .await
        .ok_or(anyhow!("Bluetooth adapter not found"))?;
    adapter
        .wait_available()
        .await
        .context("wait adapter available")?;
    Ok(adapter)
}

async fn scan_and_list_devices() -> Result<()> {
    let adapter = open_adapter().await?;
    info!("ble adapter opened");
    let mut scan_stream = adapter.scan(&[]).await?;
    let mut discovered = HashSet::new();
    while let Some(adv_device) = scan_stream.next().await {
        let id = adv_device.device.id();
        // skip duplicates
        if !discovered.insert(id.clone()) {
            continue;
        }
        let local_name = adv_device
            .device
            .name_async()
            .await
            .unwrap_or("".to_string());
        println!("{}: name='{}'", id, local_name);
    }
    Ok(())
}

async fn find_peripheral(adapter: &Adapter, id_or_name: &str) -> Result<Device> {
    info!("searching for peripheral: {}", id_or_name);
    let mut scan_stream = adapter.scan(&[]).await?;

    while let Some(adv_device) = scan_stream.next().await {
        if adv_device.device.id().to_string() == id_or_name {
            return Ok(adv_device.device);
        }
        let local_name = adv_device
            .device
            .name_async()
            .await
            .unwrap_or("".to_string());
        if local_name == id_or_name {
            return Ok(adv_device.device);
        }
    }

    Err(anyhow!("peripheral not found: {}", id_or_name))
}

pub fn bt_scan() -> Result<()> {
    let rt = Runtime::new()?;
    rt.block_on(scan_and_list_devices())?;
    Ok(())
}

async fn discover_chrc(
    device: &Device,
    service_uuid: Uuid,
    chrc_uuid: Uuid,
) -> Result<Characteristic> {
    let services = device.discover_services_with_uuid(service_uuid).await?;
    let service = services.first().ok_or(anyhow!("service not found"))?;
    let chrc_list = service
        .discover_characteristics_with_uuid(chrc_uuid)
        .await?;
    let chrc = chrc_list
        .first()
        .ok_or(anyhow!("characteristic not found"))?;
    Ok(chrc.clone())
}

impl BluetoothTransport {
    async fn new_async(runtime: Rc<Runtime>, specs: &BluetoothSpecs) -> Result<BluetoothTransport> {
        let adapter = open_adapter().await?;
        let device = find_peripheral(&adapter, &specs.device).await?;
        info!("connecting to ble peripheral");
        adapter.connect_device(&device).await?;
        info!("ble peripheral connected");

        let chrc = discover_chrc(&device, NMP_SERVICE_UUID, NMP_CHARACTERISTIC_UUID).await?;

        info!(
            "BLE transport mtu={} chrc_mtu={}",
            specs.mtu, specs.chrc_mtu
        );

        let response_stream = Box::new(chrc.notify().await?);

        let transport = BluetoothTransport {
            runtime,
            _adapter: adapter,
            _device: device,
            chrc,
            response_stream,
            mtu: specs.mtu,
            chrc_mtu: specs.chrc_mtu,
            timeout: specs.timeout,
        };
        Ok(transport)
    }

    pub fn new(specs: &BluetoothSpecs) -> Result<BluetoothTransport> {
        let runtime = Rc::new(Runtime::new()?);
        runtime.block_on(Self::new_async(runtime.clone(), &specs))
    }
}

async fn write_request(chrc: &Characteristic, data: &Vec<u8>, chrc_mtu: usize) -> Result<()> {
    // split data into chunks write to characteristic
    for chunk in data.chunks(chrc_mtu) {
        chrc.write_without_response(chunk).await?;
    }
    Ok(())
}

async fn read_response(
    notify_stream: &mut (impl Stream<Item = bluest::Result<Vec<u8>>> + Unpin),
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut response: Vec<u8> = vec![];
    // wait for notifitcations
    loop {
        let notification = tokio::time::timeout(timeout, notify_stream.next())
            .await
            .context(format!("timeout({:?}) waiting for response", timeout))?;

        if let Some(value) = notification {
            response.extend(value?);
        } else {
            break;
        }

        // read len field in header
        if response.len() >= NMP_HDR_LEN {
            // read bigendian u16 from response at offset 2
            let len = u16::from_be_bytes([response[2], response[3]]);
            if response.len() >= NMP_HDR_LEN + len as usize {
                // whole response received
                break;
            }
        }
    }

    Ok(response)
}

impl SmpTransportImpl for BluetoothTransport {
    fn mtu(&self) -> usize {
        self.mtu
    }

    fn set_timeout(&mut self, timeout: std::time::Duration) -> Result<()> {
        self.timeout = timeout;
        Ok(())
    }

    fn transceive_raw(&mut self, req_frame: &Vec<u8>) -> Result<Vec<u8>> {
        if req_frame.len() > self.mtu {
            let reduce = req_frame.len() - self.mtu;
            return Err(anyhow!(ErrTooLargeChunk(reduce)));
        }

        let rsp = self.runtime.block_on(async {
            write_request(&self.chrc, &req_frame, self.chrc_mtu).await?;
            read_response(&mut self.response_stream, self.timeout).await
        })?;

        Ok(rsp)
    }
}

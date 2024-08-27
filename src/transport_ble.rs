use anyhow::{anyhow, Context, Result};
use btleplug::api::{
    Central, CentralEvent, CharPropFlags, Characteristic, Descriptor, Manager as _,
    Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::stream::StreamExt;
use futures::Stream;
use log::info;
use std::collections::BTreeSet;
use std::pin::Pin;
use std::time::Duration;
use tokio::runtime::Runtime;
use uuid::Uuid;

use crate::nmp_hdr::*;
use crate::transport::{NmpTransport, TransportError};

// NMP Service UUID
const NMP_SERVICE_UUID: Uuid = uuid::uuid!("8D53DC1D-1DB7-4CD3-868B-8A527460AA84");
const NMP_CHARACTERISTIC_UUID: Uuid = uuid::uuid!("DA2E7828-FBCE-4E01-AE9E-261174997C48");
const NMP_TRANSPORT_CHRC: &Characteristic = &Characteristic {
    uuid: NMP_CHARACTERISTIC_UUID,
    service_uuid: NMP_SERVICE_UUID,
    properties: CharPropFlags::WRITE_WITHOUT_RESPONSE.union(CharPropFlags::NOTIFY),
    descriptors: BTreeSet::<Descriptor>::new(),
};

struct BluetoothSpec {
    pub device: String,
    // mtu can be bigger than chrc_mtu if device support assembly
    pub mtu: usize,
    // chrc_mtu is max bytes written to gatt characteristic
    // which is determined by device l2cap settings
    pub chrc_mtu: usize,
}

type NotificationStream = Pin<Box<dyn Stream<Item = ValueNotification>>>;
pub struct BluetoothTransport {
    // adapter: Adapter,
    runtime: Runtime,
    peripheral: Peripheral,
    mtu: usize,
    chrc_mtu: usize,
    seq_id: u8,
    notification_stream: NotificationStream,
}

async fn open_adapter() -> Result<Adapter> {
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no adapter found"))?;
    Ok(adapter)
}

async fn print_peripheral_info(central: &impl Central, id: &PeripheralId) -> Result<()> {
    let peripheral = central.peripheral(id).await?;
    let Some(props) = peripheral.properties().await? else {
        println!("{}: NO PROPERTIES", id);
        return Ok(());
    };
    let local_name = props.local_name.unwrap_or("".into());
    let rssi = props.rssi.map_or("".into(), |rssi| rssi.to_string());
    println!("{}: name='{}' rssi={}", id, local_name, rssi);
    Ok(())
}

async fn scan_and_list_devices() -> Result<()> {
    let central = open_adapter().await?;
    let mut event_stream = central.events().await?;
    central.start_scan(ScanFilter::default()).await?;
    while let Some(event) = event_stream.next().await {
        match event {
            CentralEvent::DeviceDiscovered(id) => {
                if let Err(e) = print_peripheral_info(&central, &id).await {
                    eprintln!("{}, Error: {:?}", id, e);
                }
            }
            _ => (),
        }
    }
    Ok(())
}

async fn find_peripheral(id_or_name: &str) -> Result<Peripheral> {
    info!("searching for peripheral: {}", id_or_name);
    let central = open_adapter().await?;
    let mut event_stream = central.events().await?;
    central.start_scan(ScanFilter::default()).await?;

    let mut peripheral_found: Option<Peripheral> = None;
    while let Some(event) = event_stream.next().await {
        match event {
            CentralEvent::DeviceDiscovered(id) => {
                let peripheral = central.peripheral(&id).await?;
                // check if the id matches the given id_or_name
                if id.to_string() == id_or_name {
                    return Ok(peripheral);
                }
                if let Some(props) = peripheral.properties().await? {
                    if props.local_name == Some(id_or_name.to_string()) {
                        info!("found peripheral: {}", id_or_name);
                        peripheral_found = Some(peripheral);
                        break;
                    }
                }
            }
            _ => (),
        }
    }
    central.stop_scan().await?;

    peripheral_found.ok_or_else(|| anyhow!("peripheral not found: {}", id_or_name))
}

pub fn bt_scan() -> Result<()> {
    let rt = Runtime::new()?;
    rt.block_on(scan_and_list_devices())?;
    Ok(())
}

async fn drain_stream<T>(stream: &mut Pin<Box<dyn Stream<Item = ValueNotification> + Send>>) {
    // drain the notification stream
    loop {
        if let Err(_) = tokio::time::timeout(Duration::from_millis(100), stream.next()).await {
            break;
        }
    }
}

impl BluetoothTransport {
    async fn open_async(id_or_name: &str) -> Result<(Peripheral, NotificationStream)> {
        let peripheral = find_peripheral(id_or_name).await?;
        peripheral.connect().await?;
        peripheral.subscribe(NMP_TRANSPORT_CHRC).await?;
        info!("ble peripheral connected");
        let mut notification_stream = peripheral.notifications().await?;
        drain_stream::<ValueNotification>(&mut notification_stream).await;
        Ok((peripheral, notification_stream))
    }

    pub fn open(id_or_name: &str) -> Result<BluetoothTransport> {
        let runtime = Runtime::new()?;
        let (peripheral, notification_stream) = runtime
            .block_on(Self::open_async(id_or_name))
            .context("open ble peripheral")?;

        let transport = BluetoothTransport {
            runtime,
            peripheral,
            mtu: 2048,
            chrc_mtu: 480,
            seq_id: rand::random::<u8>(),
            notification_stream,
        };
        Ok(transport)
    }

    // pub fn new(peripheral: Peripheral) -> Result<BluetoothTransport> {
    //     Ok(BluetoothTransport {
    //         peripheral,
    //         mtu: 200,
    //         seq_id: rand::random::<u8>(),
    //     })
    // }
}

async fn write_request(peripheral: &Peripheral, data: &Vec<u8>, chrc_mtu: usize) -> Result<()> {
    // split data into chunks write to characteristic
    for chunk in data.chunks(chrc_mtu) {
        peripheral
            .write(NMP_TRANSPORT_CHRC, chunk, WriteType::WithoutResponse)
            .await?;
    }
    Ok(())
}

const NMP_HDR_LEN: usize = 8;

async fn read_response(notification_stream: &mut NotificationStream) -> Result<Vec<u8>> {
    let mut response: Vec<u8> = vec![];
    // wait for notifitcations
    loop {
        let notification = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            notification_stream.next(),
        )
        .await
        .context("timeout waiting for response")?;

        let notification = notification.ok_or_else(|| anyhow!("no response"))?;
        response.extend(notification.value);

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

impl NmpTransport for BluetoothTransport {
    fn mtu(&self) -> usize {
        self.mtu
    }

    fn transceive(
        &mut self,
        op: NmpOp,
        group: NmpGroup,
        id: u8,
        body: &Vec<u8>, // cbor encoded message
    ) -> Result<(NmpHdr, NmpHdr, serde_cbor::Value)> {
        // encode into NMP frame
        let mut request_header = NmpHdr::new_req(op, group, id);
        request_header.seq = self.seq_id;
        request_header.len = body.len() as u16;

        let mut frame = request_header.serialize()?;
        frame.extend(body);

        if frame.len() > self.mtu {
            let reduce = frame.len() - self.mtu;
            return Err(anyhow!(TransportError::TooLargeChunk(reduce)));
        }

        let rsp = self.runtime.block_on(async {
            write_request(&self.peripheral, &frame, self.chrc_mtu).await?;
            read_response(&mut self.notification_stream).await
        })?;

        // parse header
        let mut cursor = std::io::Cursor::new(&rsp);
        let response_header = NmpHdr::deserialize(&mut cursor)?;
        // parse cbor body
        let rsp_body = serde_cbor::from_reader(cursor)?;

        Ok((request_header, response_header, rsp_body))
    }
}

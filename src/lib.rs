mod default;
mod image;
mod nmp_hdr;
mod transport;
mod transport_ble;
mod transport_serial;

pub use crate::default::reset;
pub use crate::image::{erase, list, test, upload};

pub use crate::transport::SmpTransport;
// mod test_serial_port;
pub use crate::transport_serial::SerialSpecs;
pub use crate::transport_ble::{bt_scan, BluetoothSpecs};

//! Enttec DMX USB Pro (and the many interfaces that speak its serial protocol).

use super::dmx::{DmxOutput, Universe, SLOTS};
use std::io::Write;
use std::time::Duration;

const START: u8 = 0x7e;
const END: u8 = 0xe7;
/// "Output only send DMX packet request".
const LABEL_SEND_DMX: u8 = 6;

/// A send-DMX message: start code 0 plus 512 slots.
pub fn dmx_message(universe: &Universe, out: &mut Vec<u8>) {
    let len = (SLOTS + 1) as u16;
    out.clear();
    out.extend_from_slice(&[START, LABEL_SEND_DMX, len as u8, (len >> 8) as u8, 0]);
    out.extend_from_slice(&universe.data);
    out.push(END);
}

/// Serial ports present (for the output picker). Blocks briefly: call it off the UI thread.
pub fn ports() -> Vec<String> {
    serialport::available_ports().map(|p| p.into_iter().map(|p| p.port_name).collect()).unwrap_or_default()
}

/// True for names a serial port can have (`/dev/ttyUSB0`, `/dev/cu.usbserial-1`, `COM3`): the port
/// comes from the show file, which mustn't make OpenQ open (and write DMX into) any other file.
pub fn is_serial_port_name(port: &str) -> bool {
    if cfg!(windows) {
        let p = port.strip_prefix(r"\\.\").unwrap_or(port);
        p.len() > 3 && p.get(..3).is_some_and(|c| c.eq_ignore_ascii_case("COM")) && p.bytes().skip(3).all(|b| b.is_ascii_digit())
    } else {
        let Some(name) = port.strip_prefix("/dev/") else { return false };
        !name.is_empty() && !name.split('/').any(|c| c == ".." || c.is_empty()) && (name.starts_with("tty") || name.starts_with("cu.") || name.starts_with("serial/"))
    }
}

/// An open USB Pro. Only the first universe sent each frame goes out (the device has one port).
pub struct EnttecPro {
    port: Box<dyn Write + Send>,
    buf: Vec<u8>,
    sent_this_frame: bool,
}

impl EnttecPro {
    /// Open a serial port.
    pub fn open(port: &str) -> anyhow::Result<Self> {
        if port.trim().is_empty() {
            anyhow::bail!("no serial port chosen");
        }
        if !is_serial_port_name(port) {
            anyhow::bail!("{port} is not a serial port");
        }
        // The baud rate is ignored by the USB device but must be valid for the driver.
        let p = serialport::new(port, 115_200).timeout(Duration::from_millis(500)).open()?;
        Ok(Self::with_writer(Box::new(p)))
    }

    /// Wrap any writer (tests).
    pub fn with_writer(port: Box<dyn Write + Send>) -> Self {
        Self { port, buf: Vec::with_capacity(SLOTS + 6), sent_this_frame: false }
    }
}

impl DmxOutput for EnttecPro {
    fn send(&mut self, universe: &Universe) -> anyhow::Result<()> {
        if !self.sent_this_frame {
            dmx_message(universe, &mut self.buf);
            self.port.write_all(&self.buf)?;
            self.sent_this_frame = true;
        }
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.sent_this_frame = false;
        self.port.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_port_names() {
        if cfg!(windows) {
            assert!(is_serial_port_name("COM3") && is_serial_port_name(r"\\.\COM12"));
            assert!(!is_serial_port_name(r"C:\x") && !is_serial_port_name("COM"));
        } else {
            for ok in ["/dev/ttyUSB0", "/dev/ttyACM1", "/dev/cu.usbserial-EN1", "/dev/serial/by-id/usb-ENTTEC_DMX_USB_PRO-if00"] {
                assert!(is_serial_port_name(ok), "{ok}");
            }
            for bad in ["/dev/sda", "/home/u/.bashrc", "/dev/tty/../sda", "/dev/../etc/passwd", "ttyUSB0", "/dev/"] {
                assert!(!is_serial_port_name(bad), "{bad}");
            }
        }
    }
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn message_framing() {
        let sink = Sink::default();
        let mut d = EnttecPro::with_writer(Box::new(sink.clone()));
        let mut u = Universe::new(1);
        u.data[0] = 0xaa;
        d.send(&u).unwrap();
        d.send(&Universe::new(2)).unwrap(); // second universe in the same frame: ignored
        d.flush().unwrap();
        let b = sink.0.lock().clone();
        assert_eq!(b.len(), 5 + 512 + 1);
        assert_eq!(&b[..6], &[0x7e, 6, 0x01, 0x02, 0, 0xaa]);
        assert_eq!(*b.last().unwrap(), 0xe7);
    }
}

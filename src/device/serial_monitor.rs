use std::collections::VecDeque;
use std::io::{self, Write};

use uuid::Uuid;

use crate::device::{Device, device_status};

pub(crate) const INTERFACE_ID: u32 = 0x0300;
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
const AVAILABLE_RESPONSE_SIZE: usize = size_of::<u32>();

mod operation {
    pub const WRITE: u32 = 0x00;
    pub const READ: u32 = 0x01;
    pub const PEEK: u32 = 0x02;
    pub const AVAILABLE: u32 = 0x03;
}

pub(crate) struct SerialMonitor {
    uuid: Uuid,
    name: String,
    input: VecDeque<u8>,
}

impl SerialMonitor {
    pub(crate) fn new(uuid: Uuid, name: String) -> Self {
        Self {
            uuid,
            name,
            input: VecDeque::new(),
        }
    }

    fn write_output(output: &mut impl Write, bytes: &[u8]) -> i32 {
        match output.write_all(bytes).and_then(|_| output.flush()) {
            Ok(()) => device_status::OK,
            Err(_) => device_status::DEVICE_BROKEN,
        }
    }
}

impl Device for SerialMonitor {
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        match operation {
            operation::WRITE => Self::write_output(&mut io::stdout().lock(), arguments),
            operation::READ | operation::PEEK => {
                if !arguments.is_empty() {
                    return device_status::INVALID_ARGUMENT;
                }
                if response.is_empty() {
                    return device_status::RESPONSE_OVERRUN;
                }
                let count = response.len().min(self.input.len());
                if operation == operation::READ {
                    for byte in &mut response[..count] {
                        *byte = self.input.pop_front().expect("input length was checked");
                    }
                } else {
                    for (dest, byte) in response[..count].iter_mut().zip(self.input.iter()) {
                        *dest = *byte;
                    }
                }
                i32::try_from(count).unwrap_or(device_status::DEVICE_BROKEN)
            }
            operation::AVAILABLE => {
                if !arguments.is_empty() {
                    return device_status::INVALID_ARGUMENT;
                }
                let Some(dest) = response.get_mut(..AVAILABLE_RESPONSE_SIZE) else {
                    return device_status::RESPONSE_OVERRUN;
                };
                let count =
                    u32::try_from(self.input.len()).expect("input FIFO is bounded to 64 KiB");
                dest.copy_from_slice(&count.to_le_bytes());
                AVAILABLE_RESPONSE_SIZE as i32
            }
            _ => device_status::INVALID_OPERATION,
        }
    }

    fn interface_id(&self) -> u32 {
        INTERFACE_ID
    }

    fn uuid(&self) -> Uuid {
        self.uuid
    }

    fn host_name(&self) -> Option<&str> {
        Some(&self.name)
    }

    fn receive_host_input(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_INPUT_BYTES - self.input.len() {
            return Err(format!(
                "serial monitor input buffer is full (capacity {MAX_INPUT_BYTES} bytes)"
            ));
        }
        self.input.extend(bytes.iter().copied());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_INPUT_BYTES, SerialMonitor, operation};
    use crate::device::{Device, device_status};
    use uuid::Uuid;

    #[test]
    fn input_is_read_in_order_without_losing_partial_messages() {
        let mut monitor = SerialMonitor::new(Uuid::new_v4(), "terminal".into());
        monitor.receive_host_input(b"hello\nworld\n").unwrap();

        let mut first = [0; 4];
        assert_eq!(monitor.operate(operation::READ, &[], &mut first), 4);
        assert_eq!(&first, b"hell");
        let mut rest = [0; 16];
        assert_eq!(monitor.operate(operation::READ, &[], &mut rest), 8);
        assert_eq!(&rest[..8], b"o\nworld\n");
        assert_eq!(monitor.operate(operation::READ, &[], &mut rest), 0);
    }

    #[test]
    fn peek_returns_queued_bytes_without_consuming_them() {
        let mut monitor = SerialMonitor::new(Uuid::new_v4(), "terminal".into());
        let mut response = [b'X'; 4];
        assert_eq!(monitor.operate(operation::PEEK, &[], &mut response), 0);

        monitor.receive_host_input(b"ab\x00\xff").unwrap();
        assert_eq!(monitor.operate(operation::PEEK, &[], &mut response[..2]), 2);
        assert_eq!(&response[..2], b"ab");
        assert_eq!(monitor.operate(operation::PEEK, &[], &mut response), 4);
        assert_eq!(response, [b'a', b'b', 0, 0xff]);

        assert_eq!(monitor.operate(operation::READ, &[], &mut response[..2]), 2);
        assert_eq!(&response[..2], b"ab");
        assert_eq!(monitor.operate(operation::PEEK, &[], &mut response), 2);
        assert_eq!(&response[..2], &[0, 0xff]);
        assert_eq!(monitor.operate(operation::READ, &[], &mut response), 2);
        assert_eq!(&response[..2], &[0, 0xff]);
        assert_eq!(monitor.operate(operation::PEEK, &[], &mut response), 0);
    }

    #[test]
    fn available_reports_queued_byte_count_without_copying_or_consuming_input() {
        let mut monitor = SerialMonitor::new(Uuid::new_v4(), "terminal".into());
        let mut response = [b'X'; 8];
        assert_eq!(monitor.operate(operation::AVAILABLE, &[], &mut response), 4);
        assert_eq!(&response[..4], &0_u32.to_le_bytes());
        assert_eq!(&response[4..], b"XXXX");

        monitor.receive_host_input(b"A\x00\xff").unwrap();
        assert_eq!(monitor.operate(operation::AVAILABLE, &[], &mut response), 4);
        assert_eq!(&response[..4], &3_u32.to_le_bytes());
        assert_eq!(&response[4..], b"XXXX");
        assert_eq!(monitor.operate(operation::PEEK, &[], &mut [0; 1]), 1);
        assert_eq!(monitor.operate(operation::AVAILABLE, &[], &mut response), 4);
        assert_eq!(&response[..4], &3_u32.to_le_bytes());

        assert_eq!(monitor.operate(operation::READ, &[], &mut [0; 2]), 2);
        assert_eq!(monitor.operate(operation::AVAILABLE, &[], &mut response), 4);
        assert_eq!(&response[..4], &1_u32.to_le_bytes());
    }

    #[test]
    fn rejects_oversized_input_without_adding_a_partial_message() {
        let mut monitor = SerialMonitor::new(Uuid::new_v4(), "terminal".into());
        monitor
            .receive_host_input(&vec![b'x'; MAX_INPUT_BYTES])
            .unwrap();
        assert!(monitor.receive_host_input(b"extra\n").is_err());

        let mut first = [0; 8];
        assert_eq!(monitor.operate(operation::READ, &[], &mut first), 8);
        assert_eq!(&first, b"xxxxxxxx");
        assert!(monitor.receive_host_input(b"extra\n").is_ok());
    }

    #[test]
    fn operation_statuses_follow_device_contract() {
        let mut monitor = SerialMonitor::new(Uuid::new_v4(), "terminal".into());
        assert_eq!(
            monitor.operate(operation::READ, b"unexpected", &mut [0; 1]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            monitor.operate(operation::READ, &[], &mut []),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            monitor.operate(operation::PEEK, b"unexpected", &mut [0; 1]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            monitor.operate(operation::PEEK, &[], &mut []),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            monitor.operate(operation::AVAILABLE, b"unexpected", &mut [0; 4]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            monitor.operate(operation::AVAILABLE, &[], &mut [0; 3]),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            monitor.operate(99, &[], &mut []),
            device_status::INVALID_OPERATION
        );

        let mut output = Vec::new();
        assert_eq!(
            SerialMonitor::write_output(&mut output, b"hello"),
            device_status::OK
        );
        assert_eq!(output, b"hello");
    }

    #[test]
    fn carries_raw_bytes_from_host_input_and_guest_output() {
        let mut monitor = SerialMonitor::new(Uuid::new_v4(), "terminal".into());
        monitor.receive_host_input(b"\x00\xff").unwrap();
        let mut response = [0; 2];
        assert_eq!(monitor.operate(operation::READ, &[], &mut response), 2);
        assert_eq!(response, [0, 0xff]);

        let mut output = Vec::new();
        assert_eq!(
            SerialMonitor::write_output(&mut output, b"\x00\xff"),
            device_status::OK
        );
        assert_eq!(output, [0, 0xff]);
    }
}

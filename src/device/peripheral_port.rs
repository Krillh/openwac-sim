use uuid::Uuid;

use crate::device::{Device, device_status};

pub(crate) struct PeripheralPort {
    uuid: Uuid,
}

impl PeripheralPort {
    pub(crate) fn new(uuid: Uuid) -> Self {
        Self { uuid }
    }
}

impl Device for PeripheralPort {
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        if operation != 0 {
            return device_status::INVALID_OPERATION;
        }
        if !arguments.is_empty() {
            return device_status::INVALID_ARGUMENT;
        }
        if response.len() < 4 {
            return device_status::RESPONSE_OVERRUN;
        }
        response[..4].copy_from_slice(&6_u32.to_le_bytes());
        4
    }
    fn interface_id(&self) -> u32 {
        0x0500
    }
    fn uuid(&self) -> Uuid {
        self.uuid
    }
}

#[cfg(test)]
mod tests {
    use super::PeripheralPort;
    use crate::device::{Device, device_status};
    use uuid::Uuid;

    #[test]
    fn reports_six_adjacent_positions_and_validates_the_contract() {
        let mut port = PeripheralPort::new(Uuid::new_v4());
        let mut response = [0xff; 8];
        assert_eq!(port.operate(0, &[], &mut response), 4);
        assert_eq!(&response[..4], &6_u32.to_le_bytes());
        assert_eq!(&response[4..], &[0xff; 4]);
        assert_eq!(
            port.operate(0, &[1], &mut response),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            port.operate(1, &[], &mut response),
            device_status::INVALID_OPERATION
        );
        assert_eq!(
            port.operate(0, &[], &mut [0; 3]),
            device_status::RESPONSE_OVERRUN
        );
    }
}

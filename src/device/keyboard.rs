use std::collections::VecDeque;

use uuid::Uuid;

use crate::device::{Device, device_status};

const CAPACITY: usize = 256;
const RECORD_BYTES: usize = 24;
const KEY_TEXT: u32 = 4;
const RESET: u32 = 5;

#[derive(Clone, Copy)]
struct Event {
    kind: u32,
    scancode: u32,
    keycode: u32,
    modifiers: u32,
    codepoint: u32,
    sequence: u32,
}

pub(crate) struct Keyboard {
    uuid: Uuid,
    name: String,
    events: VecDeque<Event>,
    sequence: u32,
    dropped: u32,
    capturing: bool,
}

impl Keyboard {
    pub(crate) fn new(uuid: Uuid, name: String) -> Self {
        Self {
            uuid,
            name,
            events: VecDeque::new(),
            sequence: 0,
            dropped: 0,
            capturing: false,
        }
    }
    fn enqueue(&mut self, kind: u32, codepoint: u32) {
        if self.events.len() >= CAPACITY {
            self.dropped = self.dropped.wrapping_add(self.events.len() as u32);
            self.events.clear();
            self.sequence = self.sequence.wrapping_add(1);
            self.events.push_back(Event {
                kind: RESET,
                scancode: 0,
                keycode: 0,
                modifiers: 0,
                codepoint: 0,
                sequence: self.sequence,
            });
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.events.push_back(Event {
            kind,
            scancode: 0,
            keycode: 0,
            modifiers: 0,
            codepoint,
            sequence: self.sequence,
        });
    }
}

impl Device for Keyboard {
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        if !matches!(operation, 0 | 1 | 0x10) {
            return device_status::INVALID_OPERATION;
        }
        if !arguments.is_empty() {
            return device_status::INVALID_ARGUMENT;
        }
        match operation {
            0 => {
                if response.len() < 20 {
                    return device_status::RESPONSE_OVERRUN;
                }
                for (i, value) in [
                    RECORD_BYTES as u32,
                    CAPACITY as u32,
                    self.events.len() as u32,
                    self.dropped,
                    if self.capturing { 1 } else { 0 },
                ]
                .into_iter()
                .enumerate()
                {
                    response[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                20
            }
            1 => {
                if response.len() < 4 {
                    return device_status::RESPONSE_OVERRUN;
                }
                response[..4].copy_from_slice(&(self.events.len() as u32).to_le_bytes());
                4
            }
            _ => {
                if response.len() < 4 + RECORD_BYTES {
                    return device_status::RESPONSE_OVERRUN;
                }
                let count = self.events.len().min((response.len() - 4) / RECORD_BYTES);
                response[..4].copy_from_slice(&(count as u32).to_le_bytes());
                for index in 0..count {
                    let event = self
                        .events
                        .pop_front()
                        .expect("count is bounded by queue length");
                    let fields = [
                        event.kind,
                        event.scancode,
                        event.keycode,
                        event.modifiers,
                        event.codepoint,
                        event.sequence,
                    ];
                    for (field, value) in fields.into_iter().enumerate() {
                        let start = 4 + index * RECORD_BYTES + field * 4;
                        response[start..start + 4].copy_from_slice(&value.to_le_bytes());
                    }
                }
                (4 + count * RECORD_BYTES) as i32
            }
        }
    }
    fn interface_id(&self) -> u32 {
        0x0600
    }
    fn uuid(&self) -> Uuid {
        self.uuid
    }
    fn host_name(&self) -> Option<&str> {
        Some(&self.name)
    }
    fn receive_host_input(&mut self, bytes: &[u8]) -> Result<(), String> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| "keyboard input must be UTF-8 text".to_owned())?;
        if !self.capturing {
            self.capturing = true;
            self.enqueue(RESET, 0);
        }
        for character in text.chars() {
            self.enqueue(KEY_TEXT, character as u32);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Keyboard;
    use crate::device::{Device, device_status};
    use uuid::Uuid;

    #[test]
    fn host_text_becomes_bounded_wire_events() {
        let mut keyboard = Keyboard::new(Uuid::new_v4(), "keys".into());
        let mut info = [0; 20];
        assert_eq!(keyboard.operate(0, &[], &mut info), 20);
        assert_eq!(u32::from_le_bytes(info[16..20].try_into().unwrap()), 0);

        keyboard.receive_host_input("Aé".as_bytes()).unwrap();
        assert_eq!(keyboard.operate(1, &[], &mut info[..4]), 4);
        assert_eq!(u32::from_le_bytes(info[..4].try_into().unwrap()), 3);

        let mut events = [0; 4 + 3 * 24];
        assert_eq!(
            keyboard.operate(0x10, &[], &mut events),
            events.len() as i32
        );
        assert_eq!(u32::from_le_bytes(events[..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(events[4..8].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(events[28..32].try_into().unwrap()), 4);
        assert_eq!(
            u32::from_le_bytes(events[44..48].try_into().unwrap()),
            'A' as u32
        );
        assert_eq!(u32::from_le_bytes(events[52..56].try_into().unwrap()), 4);
        assert_eq!(
            u32::from_le_bytes(events[68..72].try_into().unwrap()),
            'é' as u32
        );

        assert_eq!(
            keyboard.operate(0x10, &[], &mut [0; 27]),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            keyboard.operate(1, &[0], &mut [0; 4]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(keyboard.host_name(), Some("keys"));
    }
}

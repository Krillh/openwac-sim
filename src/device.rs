pub(crate) mod disk;
pub(crate) mod eeprom;
pub(crate) mod file_importer;
pub(crate) mod keyboard;
pub(crate) mod peripheral_port;
pub(crate) mod serial_monitor;
pub(crate) mod ttd;

use std::collections::HashSet;
use std::error::Error;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) trait Device: Send {
    /// Returns a negative status on failure, zero on success without response
    /// data, or the number of bytes written to `response` on success.
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32;

    /// Returns the type of interface this device uses.
    /// Refer to documentation for how to use each hardware interface.
    fn interface_id(&self) -> u32;

    /// Returns the uuid of this device.
    fn uuid(&self) -> uuid::Uuid;

    /// Optional name used by the host console to address this device.
    fn host_name(&self) -> Option<&str> {
        None
    }

    /// Accepts host input for devices that expose an input stream.
    fn receive_host_input(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Err("device does not accept host input".into())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DeviceConfig {
    Disk {
        #[serde(default)]
        uuid: Option<String>,
        #[serde(default)]
        is_root: bool,
        #[serde(default = "disk::default_block_count")]
        block_count: u64,
        #[serde(default = "disk::default_block_size")]
        block_size: u64,
        #[serde(default)]
        remove_on_drop: bool,
    },
    FileImporter {
        path: PathBuf,
        #[serde(default)]
        uuid: Option<String>,
    },
    SerialMonitor {
        name: String,
        #[serde(default)]
        uuid: Option<String>,
    },
    Ttd {
        #[serde(default)]
        uuid: Option<String>,
        #[serde(default)]
        headless: bool,
    },
    Eeprom {
        path: PathBuf,
        #[serde(default)]
        uuid: Option<String>,
    },
    Keyboard {
        #[serde(default = "default_keyboard_name")]
        name: String,
        #[serde(default)]
        uuid: Option<String>,
    },
    PeripheralPort {
        #[serde(default)]
        uuid: Option<String>,
    },
}

impl DeviceConfig {
    pub(crate) fn build(
        &self,
        runtime_root: &Path,
    ) -> Result<Box<dyn Device>, Box<dyn Error + Send + Sync>> {
        match self {
            Self::Disk {
                uuid,
                is_root: _,
                block_count,
                block_size,
                remove_on_drop,
            } => {
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                let block_count = u32::try_from(*block_count)?;
                let block_size = u32::try_from(*block_size)?;
                Ok(Box::new(disk::Disk::configured(
                    runtime_root,
                    uuid,
                    false,
                    block_count,
                    block_size,
                    *remove_on_drop,
                )?))
            }
            Self::FileImporter { path, uuid } => {
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                Ok(Box::new(file_importer::FileImporter::configured(
                    uuid,
                    runtime_root,
                    path,
                )?))
            }
            Self::SerialMonitor { name, uuid } => {
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                Ok(Box::new(serial_monitor::SerialMonitor::new(
                    uuid,
                    name.clone(),
                )))
            }
            Self::Ttd { uuid, headless } => {
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                Ok(Box::new(ttd::Ttd::new(uuid, *headless)?))
            }
            Self::Eeprom { path, uuid } => {
                eeprom::validate_path(path)?;
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                Ok(Box::new(eeprom::Eeprom::configured(
                    uuid,
                    runtime_root,
                    path,
                )))
            }
            Self::Keyboard { name, uuid } => {
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                Ok(Box::new(keyboard::Keyboard::new(uuid, name.clone())))
            }
            Self::PeripheralPort { uuid } => {
                let uuid = uuid
                    .as_deref()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or_else(Uuid::new_v4);
                Ok(Box::new(peripheral_port::PeripheralPort::new(uuid)))
            }
        }
    }

    pub(crate) fn serial_monitor_name(&self) -> Option<&str> {
        match self {
            Self::SerialMonitor { name, .. } => Some(name),
            _ => None,
        }
    }

    pub(crate) fn host_input_name(&self) -> Option<&str> {
        match self {
            Self::SerialMonitor { name, .. } | Self::Keyboard { name, .. } => Some(name),
            _ => None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Self::Eeprom { path, .. } = self {
            eeprom::validate_path(path)?;
        }
        if let Some(name) = self.host_input_name()
            && !valid_console_name(name)
        {
            return Err(format!(
                "host input name '{name}' must contain 1 to 64 ASCII letters, digits, '_' or '-'"
            ));
        }
        Ok(())
    }
}

fn default_keyboard_name() -> String {
    "keyboard".to_owned()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComputerSpec {
    #[serde(default)]
    pub(crate) name: Option<String>,
    pub(crate) bootloader: PathBuf,
    #[serde(default)]
    pub(crate) devices: Vec<DeviceConfig>,
    #[serde(default)]
    pub(crate) cpu: CpuConfig,
}

impl ComputerSpec {
    pub(crate) fn resolved_name(&self, id: u64) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("computer-{id}"))
    }

    pub(crate) fn validate(&self, id: u64) -> Result<(), String> {
        self.cpu.validate()?;
        if self.devices.len() > 64 {
            return Err("computer may attach at most 64 devices".into());
        }
        let name = self.resolved_name(id);
        if !valid_console_name(&name) {
            return Err("name must contain 1 to 64 ASCII letters, digits, '_' or '-'".into());
        }
        let mut monitor_names = HashSet::new();
        for device in &self.devices {
            device.validate()?;
            if let Some(name) = device.host_input_name()
                && !monitor_names.insert(name)
            {
                return Err(format!("host input name '{name}' is duplicated"));
            }
        }
        Ok(())
    }
}

fn valid_console_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CpuConfig {
    pub(crate) fuel_per_tick: u64,
    pub(crate) quantum_fuel: u64,
    #[serde(default = "default_max_processes")]
    pub(crate) max_processes: u32,
    #[serde(default = "default_max_memory_bytes")]
    pub(crate) max_memory_bytes: u64,
}

const fn default_max_processes() -> u32 {
    8
}
const fn default_max_memory_bytes() -> u64 {
    64 * 1024 * 1024
}

impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            fuel_per_tick: 4096,
            quantum_fuel: 256,
            max_processes: default_max_processes(),
            max_memory_bytes: default_max_memory_bytes(),
        }
    }
}

impl CpuConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.fuel_per_tick == 0 {
            return Err("computer CPU fuel_per_tick must be non-zero".into());
        }
        if self.fuel_per_tick > 1_000_000 {
            return Err("computer CPU fuel_per_tick must be at most 1000000".into());
        }
        if self.quantum_fuel == 0 {
            return Err("computer CPU quantum_fuel must be non-zero".into());
        }
        if self.quantum_fuel > self.fuel_per_tick {
            return Err("computer CPU quantum_fuel must not exceed fuel_per_tick".into());
        }
        if !(1..=256).contains(&self.max_processes) {
            return Err("computer CPU max_processes must be in 1..=256".into());
        }
        if !(65_536..=256 * 1024 * 1024).contains(&self.max_memory_bytes)
            || self.max_memory_bytes % 65_536 != 0
        {
            return Err(
                "computer CPU max_memory_bytes must be 64 KiB..=256 MiB and page aligned".into(),
            );
        }
        Ok(())
    }
}

pub mod device_status {
    pub const OK: i32 = 0;
    #[allow(dead_code)]
    pub const UNKNOWN_ERROR: i32 = -1;
    pub const INVALID_OPERATION: i32 = -2;
    pub const INVALID_ARGUMENT: i32 = -3;
    pub const ARGUMENT_OVERRUN: i32 = -4;
    pub const RESPONSE_OVERRUN: i32 = -5;
    pub const DEVICE_BROKEN: i32 = -6;
    pub const DEVICE_NOT_AVAILABLE: i32 = -7;
    pub const POINTER_INVALID: i32 = -8;
    pub const MEMORY_INVALID: i32 = -9;
    pub const OUT_OF_BOUNDS: i32 = -10;
    pub const SYSCALL_TOO_EXPENSIVE: i32 = -11;
    pub const PERMISSION_DENIED: i32 = -12;
    pub const KERNEL_NOT_INSTALLED: i32 = -13;
    pub const KERNEL_PANICKED: i32 = -14;
    pub const KERNEL_ALREADY_INSTALLED: i32 = -15;
    pub const KERNEL_INVALID: i32 = -16;
}

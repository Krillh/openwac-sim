use std::collections::HashSet;
use std::error::Error;
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::mpsc::{self, Sender};
use std::task::Poll;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::device::{ComputerSpec, CpuConfig, DeviceConfig};

pub(crate) type ComputerId = u64;
pub(crate) type ProcessId = u64;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimulationConfig {
    pub(crate) max_ticks: u64,
    #[serde(default = "default_tick_interval_ms")]
    pub(crate) tick_interval_ms: u64,
    pub(crate) computers: Vec<ComputerSpec>,
    #[serde(skip)]
    pub(crate) runtime_root: PathBuf,
}

impl SimulationConfig {
    pub(crate) fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let source = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&source)?;
        config.runtime_root = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.tick_interval_ms == 0 {
            return Err("simulation tick_interval_ms must be non-zero".into());
        }
        let mut names = HashSet::new();
        let mut monitor_names = HashSet::new();
        for (index, computer) in self.computers.iter().enumerate() {
            let id = u64::try_from(index).map_err(|_| "too many configured computers")?;
            computer
                .validate(id)
                .map_err(|error| format!("computer {index}: {error}"))?;
            let name = computer.resolved_name(id);
            if !names.insert(name.clone()) {
                return Err(format!("computer name '{name}' is duplicated"));
            }
            for monitor_name in computer
                .devices
                .iter()
                .filter_map(DeviceConfig::serial_monitor_name)
            {
                if !monitor_names.insert(monitor_name) {
                    return Err(format!(
                        "serial monitor name '{monitor_name}' is duplicated"
                    ));
                }
            }
        }
        Ok(())
    }
}

const fn default_tick_interval_ms() -> u64 {
    50
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            max_ticks: 64,
            tick_interval_ms: default_tick_interval_ms(),
            computers: Vec::new(),
            runtime_root: PathBuf::new(),
        }
    }
}

pub(crate) struct SimulationConfigBuilder {
    max_ticks: u64,
    bootloader: PathBuf,
    computer_count: usize,
    disks_per_computer: usize,
    remove_disks_on_drop: bool,
}

impl SimulationConfigBuilder {
    pub(crate) fn new(
        max_ticks: u64,
        bootloader: PathBuf,
        computer_count: usize,
        disks_per_computer: usize,
        remove_disks_on_drop: bool,
    ) -> Self {
        Self {
            max_ticks,
            bootloader,
            computer_count,
            disks_per_computer,
            remove_disks_on_drop,
        }
    }

    pub(crate) fn build(self) -> SimulationConfig {
        let computers = (0..self.computer_count)
            .map(|_| ComputerSpec {
                name: None,
                bootloader: self.bootloader.clone(),
                devices: (0..self.disks_per_computer)
                    .map(|index| DeviceConfig::Disk {
                        uuid: None,
                        is_root: index == 0,
                        block_count: crate::device::disk::default_block_count(),
                        block_size: crate::device::disk::default_block_size(),
                        remove_on_drop: self.remove_disks_on_drop,
                    })
                    .collect(),
                cpu: CpuConfig::default(),
            })
            .collect();
        SimulationConfig {
            max_ticks: self.max_ticks,
            tick_interval_ms: default_tick_interval_ms(),
            computers,
            runtime_root: PathBuf::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProcessEvent {
    pub(crate) computer_id: ComputerId,
    pub(crate) process_id: ProcessId,
    pub(crate) description: String,
}

pub(crate) struct TickReport {
    pub(crate) tick: u64,
    pub(crate) events: Vec<ProcessEvent>,
    pub(crate) hardware_operations: Vec<(ComputerId, u64)>,
    pub(crate) all_finished: bool,
}

#[allow(dead_code)]
pub(crate) enum ControllerCommand {
    Synchronize(Sender<Result<TickReport, String>>),
    CreateProcess {
        computer_id: ComputerId,
        reply: Sender<Result<ProcessId, String>>,
    },
    KillProcess {
        process_id: ProcessId,
        reply: Sender<Result<(), String>>,
    },
    SpawnComputer {
        spec: ComputerSpec,
        reply: Sender<Result<ComputerId, String>>,
    },
    AddDevice {
        computer_id: ComputerId,
        device: crate::device::DeviceConfig,
        reply: Sender<Result<Uuid, String>>,
    },
    RemoveDevice {
        computer_id: ComputerId,
        device_id: Uuid,
        reply: Sender<Result<(), String>>,
    },
    SendSerial {
        monitor_name: String,
        bytes: Vec<u8>,
        reply: Sender<Result<(), String>>,
    },
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct SimulationHandle {
    commands: Sender<ControllerCommand>,
}

impl SimulationHandle {
    pub(crate) fn new(commands: Sender<ControllerCommand>) -> Self {
        Self { commands }
    }

    pub(crate) fn synchronize(&self) -> Result<TickReport, Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands.send(ControllerCommand::Synchronize(reply))?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub(crate) fn create_process(
        &self,
        computer_id: ComputerId,
    ) -> Result<ProcessId, Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands
            .send(ControllerCommand::CreateProcess { computer_id, reply })?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub(crate) fn kill_process(&self, process_id: ProcessId) -> Result<(), Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands
            .send(ControllerCommand::KillProcess { process_id, reply })?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub(crate) fn spawn_computer(&self, spec: ComputerSpec) -> Result<ComputerId, Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands
            .send(ControllerCommand::SpawnComputer { spec, reply })?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub(crate) fn add_device(
        &self,
        computer_id: ComputerId,
        device: crate::device::DeviceConfig,
    ) -> Result<Uuid, Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands.send(ControllerCommand::AddDevice {
            computer_id,
            device,
            reply,
        })?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub(crate) fn remove_device(
        &self,
        computer_id: ComputerId,
        device_id: Uuid,
    ) -> Result<(), Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands.send(ControllerCommand::RemoveDevice {
            computer_id,
            device_id,
            reply,
        })?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    pub(crate) fn send_serial(
        &self,
        monitor_name: &str,
        bytes: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        let (reply, result) = mpsc::channel();
        self.commands.send(ControllerCommand::SendSerial {
            monitor_name: monitor_name.to_owned(),
            bytes: bytes.to_vec(),
            reply,
        })?;
        result
            .recv()?
            .map_err(std::io::Error::other)
            .map_err(Into::into)
    }

    pub(crate) fn shutdown(&self) -> Result<(), Box<dyn Error>> {
        self.commands.send(ControllerCommand::Shutdown)?;
        Ok(())
    }
}

pub(crate) async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await
}

pub(crate) struct TickPacer {
    interval: Duration,
    deadline: Instant,
}

impl TickPacer {
    pub(crate) fn new(interval: Duration) -> Self {
        Self::with_start(interval, Instant::now())
    }

    fn with_start(interval: Duration, start: Instant) -> Self {
        Self {
            interval,
            deadline: start,
        }
    }

    /// Signed offset from the schedule in milliseconds at `now`, for the most
    /// recently advanced tick: positive is behind schedule, negative is ahead.
    pub(crate) fn offset_ms_at(&self, now: Instant) -> f64 {
        match now.checked_duration_since(self.deadline) {
            Some(behind) => behind.as_secs_f64() * 1000.0,
            None => -(self.deadline - now).as_secs_f64() * 1000.0,
        }
    }

    pub(crate) fn advance_at(&mut self, now: Instant) -> Option<Duration> {
        self.deadline += self.interval;
        self.deadline.checked_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{DeviceConfig, SimulationConfig, SimulationConfigBuilder, TickPacer};
    use crate::device::CpuConfig;

    #[test]
    fn example_toml_deserializes_into_computer_and_device_specs() {
        let config: SimulationConfig =
            toml::from_str(include_str!("../examples/basic/simulation.example.toml"))
                .expect("example simulation config is valid TOML");
        assert_eq!(config.max_ticks, 1_000_000);
        assert_eq!(config.tick_interval_ms, 10);
        assert_eq!(config.computers.len(), 1);
        assert_eq!(config.computers[0].bootloader, Path::new("boot.wasm"));
        assert_eq!(config.computers[0].cpu.fuel_per_tick, 65_636);
        assert_eq!(config.computers[0].cpu.quantum_fuel, 256);
        assert_eq!(config.computers[0].devices.len(), 3);
        assert!(matches!(
            &config.computers[0].devices[0],
            DeviceConfig::Disk {
                remove_on_drop: true,
                ..
            }
        ));
    }

    #[test]
    fn loaded_config_uses_its_parent_as_the_runtime_root() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("basic")
            .join("simulation.example.toml");
        let config = SimulationConfig::load(&path).expect("example config loads");
        assert_eq!(
            config.runtime_root,
            path.parent()
                .expect("config has a parent")
                .canonicalize()
                .expect("example root canonicalizes")
        );
    }

    #[test]
    fn full_example_uses_the_current_hardware_profile() {
        let config: SimulationConfig =
            toml::from_str(include_str!("../examples/full/config.toml")).unwrap();
        config.validate().unwrap();
        assert_eq!(config.computers[0].cpu.fuel_per_tick, 65_536);
        assert_eq!(config.computers[0].cpu.max_processes, 16);
        assert_eq!(config.computers[0].cpu.max_memory_bytes, 1_048_576);
    }

    #[test]
    fn file_importer_device_config_deserializes() {
        let config: SimulationConfig = toml::from_str(
            r#"
[[computers]]
bootloader = "bootloader.wat"

[[computers.devices]]
type = "file_importer"
path = "gcd.wat"
"#,
        )
        .expect("file importer configuration parses");
        assert!(matches!(
            &config.computers[0].devices[0],
            DeviceConfig::FileImporter { path, uuid: None } if path == Path::new("gcd.wat")
        ));
    }

    #[test]
    fn config_builder_generates_round_trippable_toml() {
        let config =
            SimulationConfigBuilder::new(12, PathBuf::from("bootloader.wat"), 2, 1, false).build();
        let serialized = toml::to_string_pretty(&config).expect("config serializes");
        let parsed: SimulationConfig = toml::from_str(&serialized).expect("config parses");
        assert_eq!(parsed.max_ticks, 12);
        assert_eq!(parsed.tick_interval_ms, 50);
        assert_eq!(parsed.computers.len(), 2);
        assert_eq!(parsed.computers[0].devices.len(), 1);
    }

    #[test]
    fn legacy_config_uses_valid_cpu_and_tick_defaults() {
        let config: SimulationConfig = toml::from_str(
            r#"
max_ticks = 3

[[computers]]
bootloader = "boot.wasm"
"#,
        )
        .expect("legacy config parses");
        assert_eq!(config.tick_interval_ms, 50);
        assert_eq!(config.computers[0].cpu.fuel_per_tick, 4096);
        assert_eq!(config.computers[0].cpu.quantum_fuel, 256);
        config.validate().expect("defaults are valid");
    }

    #[test]
    fn serial_monitor_names_are_unique_across_computers() {
        let mut config =
            SimulationConfigBuilder::new(4, PathBuf::from("boot.wat"), 2, 0, false).build();
        config.computers[0]
            .devices
            .push(DeviceConfig::SerialMonitor {
                name: "console".into(),
                uuid: None,
            });
        config.computers[1]
            .devices
            .push(DeviceConfig::SerialMonitor {
                name: "console".into(),
                uuid: None,
            });
        assert!(config.validate().unwrap_err().contains("duplicated"));
        if let DeviceConfig::SerialMonitor { name, .. } = &mut config.computers[1].devices[0] {
            *name = "other".into();
        }
        config.validate().expect("distinct monitor names are valid");
    }

    #[test]
    fn cpu_and_tick_configuration_reject_zero_or_invalid_quantums() {
        let mut config = SimulationConfig {
            max_ticks: 1,
            tick_interval_ms: 50,
            runtime_root: PathBuf::new(),
            computers: vec![crate::device::ComputerSpec {
                name: None,
                bootloader: PathBuf::from("boot.wasm"),
                devices: Vec::new(),
                cpu: CpuConfig {
                    fuel_per_tick: 10,
                    quantum_fuel: 11,
                    ..CpuConfig::default()
                },
            }],
        };
        assert!(config.validate().is_err());
        config.computers[0].cpu.quantum_fuel = 0;
        assert!(config.validate().is_err());
        config.computers[0].cpu = CpuConfig::default();
        config.tick_interval_ms = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn tick_pacer_runs_immediately_and_preserves_deadlines_after_overruns() {
        let start = Instant::now();
        let mut pacer = TickPacer::with_start(Duration::from_millis(50), start);

        assert_eq!(
            pacer.advance_at(start + Duration::from_millis(15)),
            Some(Duration::from_millis(35))
        );
        // A tick that overran leaves the pacer behind schedule, so the next
        // ticks run back to back until it catches up.
        assert!((pacer.offset_ms_at(start + Duration::from_millis(15)) + 35.0).abs() < 1e-6);
        assert_eq!(pacer.advance_at(start + Duration::from_millis(120)), None);
        assert!((pacer.offset_ms_at(start + Duration::from_millis(120)) - 20.0).abs() < 1e-6);
        assert_eq!(
            pacer.advance_at(start + Duration::from_millis(125)),
            Some(Duration::from_millis(25))
        );
        assert!((pacer.offset_ms_at(start + Duration::from_millis(125)) + 25.0).abs() < 1e-6);
    }
}

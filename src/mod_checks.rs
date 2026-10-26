//! Boot handoff checks plus optional checks of the mod's shipped diagnostic firmware.
//! Run the latter with `cargo test --offline mod_checks -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use uuid::Uuid;

use crate::controller::Controller;
use crate::device::{ComputerSpec, CpuConfig, DeviceConfig};
use crate::simulation::{SimulationConfig, SimulationHandle};

struct Fixture {
    root: PathBuf,
    handle: Option<SimulationHandle>,
    worker: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new(firmware: &str, devices: Vec<DeviceConfig>) -> Self {
        Self::with_setup(firmware, devices, |_| {})
    }

    fn with_setup(firmware: &str, devices: Vec<DeviceConfig>, setup: impl FnOnce(&Path)) -> Self {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mod_root = std::env::var_os("OPENWAC_MOD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest.join("../openwac"));
        let image = std::fs::read(mod_root.join("bundled-data/firmware").join(firmware))
            .expect("set OPENWAC_MOD_ROOT to the mod checkout containing bundled firmware");
        Self::with_image(&image, devices, setup)
    }

    fn with_image(image: &[u8], devices: Vec<DeviceConfig>, setup: impl FnOnce(&Path)) -> Self {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
            .is_test(true)
            .try_init();
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .join("target/mod-checks")
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(root.join("bootloaders")).unwrap();
        std::fs::create_dir_all(root.join("external-files")).unwrap();
        std::fs::write(
            root.join("external-files/hello.txt"),
            b"Hello from the simulator.\nSecond line.\n",
        )
        .unwrap();
        std::fs::write(root.join("bootloaders/firmware.wasm"), image).unwrap();
        setup(&root);
        let config = SimulationConfig {
            max_ticks: 1000,
            tick_interval_ms: 50,
            runtime_root: root.clone(),
            computers: vec![ComputerSpec {
                name: Some("mod-firmware-check".into()),
                bootloader: PathBuf::from("firmware.wasm"),
                devices,
                cpu: CpuConfig {
                    fuel_per_tick: 65_536,
                    quantum_fuel: 256,
                    max_processes: 16,
                    max_memory_bytes: 1024 * 1024,
                },
            }],
        };
        let mut fixture = Self {
            root,
            handle: None,
            worker: None,
        };
        let (worker, commands) = Controller::start(config).expect("firmware controller starts");
        fixture.handle = Some(SimulationHandle::new(commands));
        fixture.worker = Some(worker);
        fixture
    }

    fn expect_chat(&self, expected: &str) {
        let mut messages = Vec::new();
        let mut recent_events = std::collections::VecDeque::new();
        for _ in 0..1000 {
            let report = self
                .handle
                .as_ref()
                .unwrap()
                .synchronize()
                .expect("firmware tick completes");
            for event in report.events {
                recent_events.push_back(format!(
                    "tick {} pid {}: {}",
                    report.tick, event.process_id, event.description
                ));
                if recent_events.len() > 16 {
                    recent_events.pop_front();
                }
                assert!(
                    !event.description.starts_with("trapped:")
                        && !event.description.contains("computer crashed")
                        && !event.description.starts_with("kernel panic:")
                        && !event.description.starts_with("spawn failed:"),
                    "firmware failure: {}",
                    event.description
                );
                if let Some(message) = event.description.strip_prefix("chat: ") {
                    println!("{message}");
                    messages.push(message.to_owned());
                }
            }
            assert!(
                !messages
                    .iter()
                    .any(|message| message.starts_with("FAIL:") || message.contains("1 failed")),
                "diagnostic failure: {messages:?}"
            );
            if messages.iter().any(|message| message.contains(expected)) {
                return;
            }
        }
        panic!(
            "firmware never reported {expected:?}; messages: {messages:?}; recent events: {recent_events:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.shutdown();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn hello_example_runs_to_completion() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = SimulationConfig::load(&manifest.join("examples/hello/simulation.toml")).unwrap();
    let (worker, commands) = Controller::start(config).unwrap();
    let handle = SimulationHandle::new(commands);
    let mut messages = Vec::new();
    let mut finished = false;
    for _ in 0..64 {
        let report = handle.synchronize().unwrap();
        for event in report.events {
            assert!(
                !event.description.starts_with("trapped:")
                    && !event.description.contains("crashed"),
                "{}",
                event.description
            );
            messages.push(event.description);
        }
        if report.all_finished {
            finished = true;
            break;
        }
    }
    handle.shutdown().unwrap();
    worker.join().unwrap();
    assert!(finished, "hello guest should finish within its tick limit");
    assert!(
        messages
            .iter()
            .any(|message| message == "chat: OpenWAC simulator ready.")
    );
}

#[test]
fn restart_reboots_and_preserves_disk_contents() {
    let disk_id = Uuid::new_v4();
    let escaped_uuid: String = disk_id
        .as_bytes()
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect();
    let bios = wat::parse_str(format!(
        r#"(module
        (import "hal" "hal_device_call" (func $call (param i32 i32 i32 i32 i32 i32) (result i32)))
        (import "hal" "hal_restart" (func $restart (result i32)))
        (import "hal" "hal_chat_print" (func $print (param i32 i32) (result i32)))
        (import "hal" "hal_cpu_time_ns" (func $time (result i64)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{escaped_uuid}")
        (data (i32.const 64) "RESTART PASS")
        (func (export "_start")
            call $time i64.const 0 i64.ne if unreachable end
            i32.const 0 i32.const 32 i32.const 16 i32.const 4
            i32.const 1024 i32.const 2048 call $call if unreachable end
            i32.const 1024 i32.load8_u i32.eqz
            if
                i32.const 4100 i32.const 1 i32.store8
                i32.const 0 i32.const 48 i32.const 4096 i32.const 2052
                i32.const 0 i32.const 0 call $call if unreachable end
                call $restart drop unreachable
            end
            i32.const 64 i32.const 12 call $print if unreachable end))"#
    ))
    .unwrap();
    let fixture = Fixture::with_image(
        &bios,
        vec![DeviceConfig::Disk {
            uuid: Some(disk_id.to_string()),
            is_root: false,
            block_count: 512,
            block_size: 2048,
            remove_on_drop: true,
        }],
        |_| {},
    );
    fixture.expect_chat("RESTART PASS");
}

#[test]
fn pending_child_accepts_wake_and_unprivileged_restart_returns_denied() {
    let child = wat::parse_str(
        r#"(module
        (import "hal" "hal_restart" (func $restart (result i32)))
        (memory (export "memory") 1)
        (func (export "_start") call $restart i32.const -1 i32.ne if unreachable end))"#,
    )
    .unwrap();
    let escaped: String = child.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let bios = wat::parse_str(format!(
        r#"(module
        (import "hal" "hal_spawn_process" (func $spawn (param i32 i32) (result i64)))
        (import "hal" "hal_wake" (func $wake (param i64) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{escaped}")
        (func (export "_start")
            i32.const 0 i32.const {} call $spawn
            call $wake if unreachable end))"#,
        child.len()
    ))
    .unwrap();
    let fixture = Fixture::with_image(&bios, vec![], |_| {});
    let mut child_exited = false;
    let mut finished = false;
    for _ in 0..16 {
        let report = fixture.handle.as_ref().unwrap().synchronize().unwrap();
        for event in report.events {
            assert!(
                !event.description.starts_with("trapped:")
                    && !event.description.contains("crashed"),
                "{}",
                event.description
            );
            child_exited |= event.process_id == 2 && event.description == "exited";
        }
        if report.all_finished {
            finished = true;
            break;
        }
    }
    assert!(
        finished && child_exited,
        "woken child must instantiate, call denied restart, and exit normally"
    );
}

#[test]
#[ignore = "requires the mod checkout's bundled firmware"]
fn bundled_disk_self_test() {
    let fixture = Fixture::new(
        "disk-test-v1.wasm",
        vec![DeviceConfig::Disk {
            uuid: None,
            is_root: false,
            block_count: 512,
            block_size: 2048,
            remove_on_drop: true,
        }],
    );
    fixture.expect_chat("SELF-TEST COMPLETE: 1 passed, 0 failed.");
}

#[test]
#[ignore = "requires the mod checkout's bundled firmware"]
fn bundled_ttd_self_test() {
    let fixture = Fixture::new(
        "ttd-test-v1.wasm",
        vec![DeviceConfig::Ttd {
            uuid: None,
            headless: true,
        }],
    );
    fixture.expect_chat("TTD TEST COMPLETE: 1 passed, 0 failed.");
}

#[test]
#[ignore = "requires the mod checkout's bundled firmware"]
fn bundled_importer_self_test() {
    let fixture = Fixture::new(
        "importer-test-v1.wasm",
        vec![DeviceConfig::FileImporter {
            uuid: None,
            path: PathBuf::from("hello.txt"),
        }],
    );
    fixture.expect_chat("IMPORTER TEST COMPLETE: 1 passed, 0 failed.");
}

#[test]
#[ignore = "requires the mod checkout's bundled firmware"]
fn bundled_importer_to_ttd_self_test() {
    let fixture = Fixture::new(
        "importer-ttd-test-v1.wasm",
        vec![
            DeviceConfig::Ttd {
                uuid: None,
                headless: true,
            },
            DeviceConfig::FileImporter {
                uuid: None,
                path: PathBuf::from("hello.txt"),
            },
        ],
    );
    fixture.expect_chat("IMPORTER -> TTD COMPLETE: 1 passed, 0 failed.");
}

#[test]
#[ignore = "requires the mod checkout's bundled firmware"]
fn bundled_bootloader_imports_link() {
    // This artifact still expects the retired disk root flag. Match the mod's
    // own smoke check: verify import linking, without promising successful boot.
    let fixture = Fixture::new("bootloader-v1.wasm", vec![]);
    for _ in 0..20 {
        let report = fixture.handle.as_ref().unwrap().synchronize().unwrap();
        assert!(
            report
                .events
                .iter()
                .all(|event| !event.description.starts_with("trapped:"))
        );
    }
}

#[test]
fn disk_kernel_handoff_and_user_syscall() {
    use crate::device::Device;
    // Use a BIOS written for the current ABI. The production guest artifacts
    // still need their retired disk-root operations updated independently.
    // Initialization must happen after BIOS RAM has been released.
    let child = wat::parse_str(
        r#"(module
        (import "sys" "syscall" (func $call (param i32 i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "_start")
            i32.const 7 i32.const 0 i32.const 0 i32.const 0 i32.const 0 call $call
            if unreachable end))"#,
    )
    .unwrap();
    let escaped: String = child.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let kernel = wat::parse_str(format!(
        r#"(module
        (import "hal" "hal_chat_print" (func $print (param i32 i32) (result i32)))
        (import "hal" "hal_ram_used_pages" (func $pages (result i64)))
        (import "hal" "hal_spawn_process" (func $spawn (param i32 i32) (result i64)))
        (memory (export "memory") 1)
        (data (i32.const 0) "\80\00\00\00\40\00\00\00\00\01\00\00\40\00\00\00")
        (data (i32.const 512) "OS SYSCALL PASS")
        (data (i32.const 1024) "{escaped}")
        (func (export "kernel_get_io_buffers") (result i32) i32.const 0)
        (func (export "kernel_init")
            call $pages i64.const 1 i64.ne if unreachable end
            i32.const 1024 i32.const {} call $spawn i64.eqz if unreachable end)
        (func (export "kernel_syscall") (param i64 i32 i32 i32 i32) (result i32)
            local.get 1 if unreachable end
            local.get 2 i32.const 7 i32.ne if unreachable end
            i32.const 512 i32.const 15 call $print drop
            i32.const 0))"#,
        child.len()
    ))
    .unwrap();
    let disk_id = Uuid::new_v4();
    let escaped_uuid: String = disk_id
        .as_bytes()
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect();
    assert!(kernel.len() <= 2048, "the test BIOS loads one block");
    let bios = wat::parse_str(format!(
        r#"(module
        (import "hal" "hal_device_call" (func $read (param i32 i32 i32 i32 i32 i32) (result i32)))
        (import "hal" "hal_install_kernel" (func $install (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{escaped_uuid}")
        (data (i32.const 16) "\01\00\00\00")
        (func (export "_start")
            i32.const 0 i32.const 32 i32.const 16 i32.const 4
            i32.const 4096 i32.const 2048 call $read if unreachable end
            i32.const 4096 i32.const {} call $install if unreachable end))"#,
        kernel.len()
    ))
    .unwrap();
    let fixture = Fixture::with_image(
        &bios,
        vec![DeviceConfig::Disk {
            uuid: Some(disk_id.to_string()),
            is_root: false,
            block_count: 512,
            block_size: 2048,
            remove_on_drop: true,
        }],
        |root| {
            let mut disk =
                crate::device::disk::Disk::configured(root, disk_id, false, 512, 2048, false)
                    .unwrap();
            // TFAF: one root entry named kernel.x, table in block 0, data from block 1.
            let mut header = vec![0_u8; 2048];
            header[..4].copy_from_slice(&0x2cb8858e_u32.to_le_bytes());
            header[4..8].copy_from_slice(&0xfaf01000_u32.to_le_bytes());
            header[8..10].copy_from_slice(&2048_u16.to_le_bytes());
            header[10..12].copy_from_slice(&512_u16.to_le_bytes());
            header[12..14].copy_from_slice(&1_u16.to_le_bytes());
            header[14..16].copy_from_slice(&1_u16.to_le_bytes());
            header[16..20].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
            header[20..22].copy_from_slice(&1_u16.to_le_bytes());
            header[22..24].copy_from_slice(&8_u16.to_le_bytes());
            header[24..32].copy_from_slice(b"kernel.x");
            let mut blocks = vec![header];
            for chunk in kernel.chunks(2048) {
                let mut block = vec![0; 2048];
                block[..chunk.len()].copy_from_slice(chunk);
                blocks.push(block);
            }
            for (index, block) in blocks.iter().enumerate() {
                let mut args = (index as u32).to_le_bytes().to_vec();
                args.extend_from_slice(block);
                assert_eq!(disk.operate(0x30, &args, &mut []), 0);
            }
        },
    );
    fixture.expect_chat("OS SYSCALL PASS");
}

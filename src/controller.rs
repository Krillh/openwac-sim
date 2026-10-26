use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use log::info;
use tokio::runtime::Builder;
use tokio::task::JoinSet;
use uuid::Uuid;
use wasmtime::{Config, Engine, Module};

use crate::compile::CompileJob;
use crate::computer::{Computer, PendingSpawn};
use crate::device::{ComputerSpec, DeviceConfig};
use crate::guest::{GuestProcess, GuestProcessStatus};
use crate::hal::{ExitReason, ProcessRequest};
use crate::kernel::KERNEL_PID;
use crate::simulation::{
    ComputerId, ControllerCommand, ProcessEvent, ProcessId, SimulationConfig, TickReport, poll_once,
};

pub(crate) struct Controller;

impl Controller {
    pub(crate) fn start(
        config: SimulationConfig,
    ) -> Result<(JoinHandle<()>, Sender<ControllerCommand>), Box<dyn std::error::Error>> {
        config.validate().map_err(std::io::Error::other)?;
        let (commands, receiver) = mpsc::channel();
        let (initialized, result) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("sim_controller".into())
            .spawn(move || controller_thread(config, receiver, initialized))?;
        result.recv()?.map_err(std::io::Error::other)?;
        Ok((thread, commands))
    }
}

fn controller_thread(
    config: SimulationConfig,
    commands: Receiver<ControllerCommand>,
    initialized: Sender<Result<(), String>>,
) {
    let runtime_root = config.runtime_root.clone();
    let mut wasmtime_config = Config::new();
    wasmtime_config.consume_fuel(true);

    let setup = (|| -> Result<(Engine, Vec<Computer>, u64), String> {
        let engine = Engine::new(&wasmtime_config).map_err(|error| error.to_string())?;
        let mut computers = Vec::with_capacity(config.computers.len());
        for (index, spec) in config.computers.into_iter().enumerate() {
            let id = u64::try_from(index).map_err(|_| "too many configured computers")?;
            computers.push(build_computer(&engine, spec, id, &runtime_root)?);
        }
        let next_computer_id =
            u64::try_from(computers.len()).map_err(|_| "too many configured computers")?;
        Ok((engine, computers, next_computer_id))
    })();

    let (engine, mut computers, mut next_computer_id) = match setup {
        Ok(state) => state,
        Err(error) => {
            let _ = initialized.send(Err(error));
            return;
        }
    };
    let runtime = match Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = initialized.send(Err(error.to_string()));
            return;
        }
    };
    if initialized.send(Ok(())).is_err() {
        return;
    }

    let mut tick = 0_u64;
    while let Ok(command) = commands.recv() {
        match command {
            ControllerCommand::Synchronize(reply) => {
                tick = tick.saturating_add(1);
                let report = synchronize(&runtime, &engine, tick, &mut computers);
                if reply.send(report).is_err() {
                    break;
                }
            }
            ControllerCommand::CreateProcess { computer_id, reply } => {
                let result = create_process(&engine, &mut computers, computer_id);
                let _ = reply.send(result);
            }
            ControllerCommand::KillProcess { process_id, reply } => {
                let result = kill_process(&mut computers, process_id);
                let _ = reply.send(result);
            }
            ControllerCommand::SpawnComputer { spec, reply } => {
                let name = spec.resolved_name(next_computer_id);
                let duplicate_monitor = spec
                    .devices
                    .iter()
                    .filter_map(DeviceConfig::host_input_name)
                    .find(|monitor_name| {
                        computers.iter().any(|computer| {
                            computer
                                .shared
                                .devices
                                .find_host_device(monitor_name)
                                .is_some()
                        })
                    });
                let result = if computers.iter().any(|computer| computer.name == name) {
                    Err(format!("computer name '{name}' is duplicated"))
                } else if let Some(monitor_name) = duplicate_monitor {
                    Err(format!(
                        "serial monitor name '{monitor_name}' is duplicated"
                    ))
                } else {
                    build_computer(&engine, spec, next_computer_id, &runtime_root).and_then(
                        |computer| {
                            let id = computer.id;
                            let next_id = id
                                .checked_add(1)
                                .ok_or_else(|| "computer ID space exhausted".to_owned())?;
                            computers.push(computer);
                            next_computer_id = next_id;
                            Ok(id)
                        },
                    )
                };
                let _ = reply.send(result);
            }
            ControllerCommand::AddDevice {
                computer_id,
                device,
                reply,
            } => {
                let result = add_device(&mut computers, computer_id, device, &runtime_root);
                let _ = reply.send(result);
            }
            ControllerCommand::RemoveDevice {
                computer_id,
                device_id,
                reply,
            } => {
                let result = remove_device(&mut computers, computer_id, device_id);
                let _ = reply.send(result);
            }
            ControllerCommand::SendSerial {
                monitor_name,
                bytes,
                reply,
            } => {
                let result = send_serial(&computers, &monitor_name, &bytes);
                let _ = reply.send(result);
            }
            ControllerCommand::Shutdown => break,
        }
    }
}

fn synchronize(
    runtime: &tokio::runtime::Runtime,
    engine: &Engine,
    tick: u64,
    computers: &mut [Computer],
) -> Result<TickReport, String> {
    let mut events = Vec::new();
    for computer in computers.iter_mut() {
        computer.shared.clock.advance_to(tick);
        computer.shared.begin_tick();
    }
    for computer in computers.iter_mut() {
        let boot_active = computer
            .processes
            .iter()
            .any(|process| process.pid == computer.boot_pid && process.status.is_active());
        let maintenance_pending = computer.kernel.deferred_init_pending()
            || computer.kernel.is_maintenance_pending()
            || computer.shared.has_exits();
        let maintenance_fuel = computer.shared.cpu_budget.remaining_fuel();
        if !boot_active && maintenance_pending && maintenance_fuel > 0 {
            runtime.block_on(
                computer
                    .kernel
                    .deliver_exits(&computer.shared, maintenance_fuel),
            );
        }
        if let Some(message) = computer.kernel.take_boot_failure() {
            events.push(event(
                computer.id,
                computer.boot_pid,
                format!("computer crashed: boot initialization failed: {message}"),
            ));
            crash_computer_state(computer);
            continue;
        }
        apply_kernel_panic(computer, &mut events);
    }
    // Requests from the prior tick are admitted before scheduling so a child
    // can run on its first eligible tick.
    for computer in computers.iter_mut() {
        let boot_active = computer
            .process_mut(computer.boot_pid)
            .is_some_and(|process| process.status.is_active());
        if !computer.crashed && !boot_active && computer.kernel.processes_ready() {
            for pending in std::mem::take(&mut computer.pending_spawns) {
                if pending.cancelled {
                    continue;
                }
                let pid = pending.pid;
                match pending.compile.wait() {
                    Ok(child_module) => match GuestProcess::new(
                        engine,
                        &child_module,
                        computer.shared.clone(),
                        computer.kernel.clone(),
                        pid,
                        pending.privileged,
                    ) {
                        Ok(process) => {
                            computer.ready.push_back(pid);
                            computer.processes.push(process);
                            events.push(event(computer.id, pid, "spawned".into()));
                        }
                        Err(error) => {
                            computer.shared.unmark_pending_spawn(pid);
                            computer.shared.release_process_slot();
                            computer.shared.record_exit(pid, ExitReason::Trapped);
                            events.push(event(computer.id, pid, format!("spawn failed: {error}")));
                        }
                    },
                    Err(error) => {
                        computer.shared.unmark_pending_spawn(pid);
                        computer.shared.release_process_slot();
                        computer.shared.record_exit(pid, ExitReason::Trapped);
                        events.push(event(
                            computer.id,
                            pid,
                            format!("spawn failed: invalid Wasm module: {error}"),
                        ));
                    }
                }
            }
        }
    }
    let poll_results = runtime.block_on(async {
        let mut tasks = JoinSet::new();
        for (computer_index, computer) in computers.iter_mut().enumerate() {
            for process in &mut computer.processes {
                if process.status == GuestProcessStatus::WaitingForSync {
                    process.sync.release_for_tick();
                }
            }
            let computer_id = computer.id;
            let shared = Arc::clone(&computer.shared);
            let kernel = Arc::clone(&computer.kernel);
            let processes = std::mem::take(&mut computer.processes);
            let ready = std::mem::take(&mut computer.ready);
            let quantum = computer.shared.cpu_budget.quantum_fuel();
            let boot_pid = computer.boot_pid;
            tasks.spawn(async move {
                let (processes, ready, events, boot_failed) = schedule_computer(
                    computer_id,
                    shared,
                    kernel,
                    processes,
                    ready,
                    quantum,
                    boot_pid,
                )
                .await;
                (computer_index, processes, ready, events, boot_failed)
            });
        }

        let mut results = Vec::new();
        while let Some(result) = tasks.join_next().await {
            results
                .push(result.map_err(|error| {
                    format!("computer scheduling task failed to join: {error}")
                })?);
        }
        Ok::<_, String>(results)
    })?;

    for (computer_index, processes, ready, computer_events, boot_failed) in poll_results {
        computers[computer_index].processes = processes;
        computers[computer_index].ready = ready;
        events.extend(computer_events);
        if boot_failed {
            let computer = &mut computers[computer_index];
            events.push(event(
                computer.id,
                computer.boot_pid,
                "computer crashed: boot process trapped".into(),
            ));
            crash_computer_state(computer);
        }
    }

    for computer in computers.iter_mut() {
        if computer.shared.take_restart_request() {
            restart_computer(engine, computer, &mut events)?;
        }
        for message in computer.shared.take_chat() {
            log::info!("computer {} chat: {}", computer.id, message);
            events.push(event(computer.id, KERNEL_PID, format!("chat: {message}")));
        }
    }

    for computer in computers.iter_mut() {
        for request in computer.shared.take_process_requests()? {
            match request {
                ProcessRequest::Spawn {
                    pid,
                    wasm,
                    privileged,
                } => computer.pending_spawns.push(PendingSpawn {
                    pid,
                    privileged,
                    compile: CompileJob::start(engine, wasm),
                    cancelled: false,
                }),
                ProcessRequest::Kill(pid) if computer.cancel_pending_spawn(pid) => {
                    events.push(event(computer.id, pid, "killed before it started".into()));
                }
                ProcessRequest::Kill(pid) => match computer.process_mut(pid) {
                    Some(process) if process.status.is_active() => {
                        if process.kill() {
                            events.push(event(computer.id, pid, "killed".into()));
                        } else {
                            events.push(event(
                                computer.id,
                                pid,
                                "kill deferred until its kernel call returns".into(),
                            ));
                        }
                    }
                    _ => events.push(event(
                        computer.id,
                        pid,
                        "kill request ignored: process is not running".into(),
                    )),
                },
            }
        }
        let boot_was_killed = computer.processes.iter().any(|process| {
            process.pid == computer.boot_pid && process.status == GuestProcessStatus::Killed
        });
        if boot_was_killed {
            cancel_boot_work(computer);
        }
        apply_kernel_panic(computer, &mut events);
        report_kernel_panic(computer, &mut events);
    }

    async fn schedule_computer(
        computer_id: ComputerId,
        shared: Arc<crate::hal::ComputerShared>,
        kernel: Arc<crate::kernel::KernelSlot>,
        mut processes: Vec<GuestProcess>,
        mut ready: std::collections::VecDeque<ProcessId>,
        quantum_fuel: u64,
        boot_pid: ProcessId,
    ) -> (
        Vec<GuestProcess>,
        std::collections::VecDeque<ProcessId>,
        Vec<ProcessEvent>,
        bool,
    ) {
        let started = Instant::now();
        let mut events = Vec::new();
        let mut boot_failed = false;
        for process in &mut processes {
            let should_run = match &process.status {
                GuestProcessStatus::WaitingForSync | GuestProcessStatus::WaitingForBudget => {
                    process.status = GuestProcessStatus::Runnable;
                    true
                }
                GuestProcessStatus::WaitingForWake if process.is_woken() => {
                    process.status = GuestProcessStatus::Runnable;
                    true
                }
                GuestProcessStatus::Runnable => true,
                _ => false,
            };
            if should_run && !ready.contains(&process.pid) {
                ready.push_back(process.pid);
            }
        }

        while !ready.is_empty() && !shared.restart_requested() {
            // A process woken during this tick joins the FIFO tail.
            for process in &mut processes {
                if process.status == GuestProcessStatus::WaitingForWake && process.is_woken() {
                    process.status = GuestProcessStatus::Runnable;
                    if !ready.contains(&process.pid) {
                        ready.push_back(process.pid);
                    }
                }
            }
            let boot_active = processes
                .iter()
                .any(|process| process.pid == boot_pid && process.status.is_active());
            if boot_active {
                // BIOS owns the machine until it exits. Host-created processes
                // may be queued during boot, but cannot run ahead of it.
                let Some(boot_position) = ready.iter().position(|pid| *pid == boot_pid) else {
                    break;
                };
                ready.rotate_left(boot_position);
            }
            let Some(pid) = ready.pop_front() else { break };
            let Some(index) = processes.iter().position(|process| process.pid == pid) else {
                continue;
            };
            if !processes[index].status.is_active() {
                continue;
            }
            // A suspended syscall charge gets first use of the next tick's
            // budget. Its host function yields after charging, before guest
            // Wasm resumes and needs a normal quantum reservation.
            if !processes[index].is_waiting_for_budget() && !shared.cpu_budget.reserve_quantum() {
                ready.push_front(pid);
                break;
            }
            let mut process = processes.remove(index);
            match poll_once(process.future_mut()).await {
                std::task::Poll::Ready(Ok(())) => {
                    process.finish(GuestProcessStatus::Exited);
                    events.push(event(computer_id, process.pid, "exited".into()));
                    processes.push(process);
                }
                std::task::Poll::Ready(Err(error)) => {
                    process.finish(GuestProcessStatus::Trapped(error.clone()));
                    events.push(event(computer_id, process.pid, format!("trapped: {error}")));
                    boot_failed = process.pid == boot_pid;
                    processes.push(process);
                }
                std::task::Poll::Pending if process.apply_deferred_kill() => {
                    events.push(event(computer_id, process.pid, "killed".into()));
                    processes.push(process);
                }
                std::task::Poll::Pending if process.is_yielding() => {
                    process.clear_yield();
                    process.status = GuestProcessStatus::Runnable;
                    ready.push_back(process.pid);
                    processes.push(process);
                }
                std::task::Poll::Pending if process.is_waiting_for_wake() => {
                    process.status = GuestProcessStatus::WaitingForWake;
                    events.push(event(computer_id, process.pid, "blocked in syscall".into()));
                    processes.push(process);
                }
                std::task::Poll::Pending if process.is_waiting_for_sync() => {
                    process.status = GuestProcessStatus::WaitingForSync;
                    events.push(event(
                        computer_id,
                        process.pid,
                        "voluntarily yielded".into(),
                    ));
                    processes.push(process);
                }
                std::task::Poll::Pending if process.is_waiting_for_budget() => {
                    process.status = GuestProcessStatus::WaitingForBudget;
                    events.push(event(
                        computer_id,
                        process.pid,
                        "waiting for CPU budget".into(),
                    ));
                    processes.push(process);
                }
                std::task::Poll::Pending => {
                    process.status = GuestProcessStatus::Runnable;
                    ready.push_back(process.pid);
                    processes.push(process);
                }
            }
            let live_pids = processes
                .iter()
                .filter(|process| process.status.is_active())
                .map(|process| process.pid)
                .collect::<std::collections::HashSet<_>>();
            let live_kills = shared
                .process_control
                .take_live_kills(&live_pids)
                .expect("process request queue lock poisoned");
            for target_pid in live_kills {
                let Some(target) = processes
                    .iter_mut()
                    .find(|process| process.pid == target_pid && process.status.is_active())
                else {
                    continue;
                };
                ready.retain(|queued_pid| *queued_pid != target_pid);
                if target.kill() {
                    events.push(event(computer_id, target_pid, "killed".into()));
                } else {
                    events.push(event(
                        computer_id,
                        target_pid,
                        "kill deferred until its kernel call returns".into(),
                    ));
                }
            }
            if kernel.is_dead() {
                for process in &mut processes {
                    if !process.status.is_active()
                        || process.privileged
                        || process.is_kill_pending()
                    {
                        continue;
                    }
                    ready.retain(|queued_pid| *queued_pid != process.pid);
                    let description = if process.kill() {
                        "killed: kernel panic"
                    } else {
                        "kill deferred until its kernel call returns"
                    };
                    events.push(event(computer_id, process.pid, description.into()));
                }
            }
            if boot_failed {
                break;
            }
        }

        for pid in &ready {
            events.push(event(
                computer_id,
                *pid,
                format!("runnable next tick (CPU budget exhausted; quantum {quantum_fuel})"),
            ));
        }
        shared.set_cpu_time_nanos(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        (processes, ready, events, boot_failed)
    }

    let hardware_operations = computers
        .iter()
        .map(|computer| (computer.id, computer.shared.devices.operation_count()))
        .collect();
    if let Some(computer) = computers.first() {
        debug_assert_eq!(computer.shared.clock.tick(), tick);
    }
    let all_finished = computers.iter().all(|computer| {
        computer.crashed
            || (computer.active_processes().next().is_none()
                && !computer.has_pending_spawns()
                && !computer.kernel.is_maintenance_pending()
                && !computer.kernel.deferred_init_pending()
                && !(computer.shared.has_exits() && computer.kernel.needs_exit_delivery()))
    });
    Ok(TickReport {
        tick,
        events,
        hardware_operations,
        all_finished,
    })
}

fn crash_computer_state(computer: &mut Computer) {
    computer.crashed = true;
    computer.processes.clear();
    computer.ready.clear();
    for pending in computer.pending_spawns.drain(..) {
        if !pending.cancelled {
            computer.shared.unmark_pending_spawn(pending.pid);
            computer.shared.release_process_slot();
        }
    }
    computer.shared.reset_runtime_state();
    computer.kernel = Arc::new(crate::kernel::KernelSlot::default());
}

/// A killed BIOS abandons all process and kernel work it queued during boot.
/// The machine remains available, but no deferred boot child may start.
fn cancel_boot_work(computer: &mut Computer) {
    for pending in computer.pending_spawns.drain(..) {
        if !pending.cancelled {
            computer.shared.unmark_pending_spawn(pending.pid);
            computer.shared.release_process_slot();
        }
    }
    computer.kernel = Arc::new(crate::kernel::KernelSlot::default());
}

fn apply_kernel_panic(computer: &mut Computer, events: &mut Vec<ProcessEvent>) {
    if !computer.kernel.is_dead() {
        return;
    }
    for process in &mut computer.processes {
        if process.status.is_active() && !process.privileged && !process.is_kill_pending() {
            let description = if process.kill() {
                "killed: kernel panic"
            } else {
                "kill deferred until its kernel call returns"
            };
            events.push(event(computer.id, process.pid, description.into()));
        }
    }
    let mut retained = Vec::new();
    for pending in std::mem::take(&mut computer.pending_spawns) {
        if pending.privileged {
            retained.push(pending);
        } else if !pending.cancelled {
            computer.shared.unmark_pending_spawn(pending.pid);
            computer.shared.release_process_slot();
            computer.shared.record_exit(pending.pid, ExitReason::Killed);
            events.push(event(
                computer.id,
                pending.pid,
                "killed before it started: kernel panic".into(),
            ));
        }
    }
    computer.pending_spawns = retained;
}

fn report_kernel_panic(computer: &mut Computer, events: &mut Vec<ProcessEvent>) {
    if let Some(message) = computer.kernel.take_panic_report() {
        events.push(event(
            computer.id,
            KERNEL_PID,
            format!("kernel panic: {message}"),
        ));
    }
}

fn event(computer_id: ComputerId, process_id: ProcessId, description: String) -> ProcessEvent {
    ProcessEvent {
        computer_id,
        process_id,
        description,
    }
}

fn restart_computer(
    engine: &Engine,
    computer: &mut Computer,
    events: &mut Vec<ProcessEvent>,
) -> Result<(), String> {
    computer.processes.clear();
    computer.ready.clear();
    for pending in computer.pending_spawns.drain(..) {
        if !pending.cancelled {
            computer.shared.release_process_slot();
        }
    }
    computer.shared.reset_runtime_state();
    computer.kernel = Arc::new(crate::kernel::KernelSlot::default());
    computer.crashed = false;
    let pid = computer.shared.allocate_process_id()?;
    if !computer.shared.reserve_process_slot() {
        return Err("computer process limit reached while restarting".into());
    }
    match GuestProcess::new(
        engine,
        &computer.bootloader,
        computer.shared.clone(),
        computer.kernel.clone(),
        pid,
        true,
    ) {
        Ok(process) => {
            computer.boot_pid = pid;
            computer.ready.push_back(pid);
            computer.processes.push(process);
            events.push(event(computer.id, pid, "computer restarted".into()));
        }
        Err(error) => {
            computer.shared.release_process_slot();
            computer.crashed = true;
            events.push(event(computer.id, pid, format!("restart failed: {error}")));
        }
    }
    Ok(())
}

fn build_computer(
    engine: &Engine,
    spec: ComputerSpec,
    computer_id: ComputerId,
    runtime_root: &Path,
) -> Result<Computer, String> {
    spec.validate(computer_id)?;
    let name = spec.resolved_name(computer_id);
    let bootloader_path = bootloader_file_path(runtime_root, &spec.bootloader);
    let bootloader = load_module(engine, &bootloader_path)?;
    let cpu = spec.cpu.clone();
    let computer = Computer::new(computer_id, name, bootloader, cpu);
    for device_config in spec.devices {
        let device = device_config
            .build(runtime_root)
            .map_err(|error| error.to_string())?;
        computer.shared.devices.insert(device)?;
    }
    if !computer
        .shared
        .devices
        .device_infos()
        .iter()
        .any(|(interface, _)| *interface == 0x0001)
    {
        let image = read_bootloader_image(&bootloader_path)?;
        let bios_uuid =
            Uuid::from_u128(0x4f57_4143_0000_0000_0000_0000_0000_0000 | computer_id as u128);
        let bios = crate::device::eeprom::Eeprom::from_image(bios_uuid, image)?;
        computer.shared.devices.insert(Box::new(bios))?;
    }
    let devices = computer.shared.devices.device_infos();
    info!(
        "computer {} ({}) starting simulation with {} device(s)",
        computer.id,
        computer.name,
        devices.len()
    );
    for (interface_id, uuid) in devices {
        info!(
            "computer {} device: interface 0x{:04X}, UUID {}",
            computer.id, interface_id, uuid
        );
    }
    let pid = computer.shared.allocate_process_id()?;
    let mut computer = computer;
    if !computer.shared.reserve_process_slot() {
        return Err("computer process limit reached".into());
    }
    let boot_process = GuestProcess::new(
        engine,
        &computer.bootloader,
        computer.shared.clone(),
        computer.kernel.clone(),
        pid,
        true,
    )
    .map_err(|error| {
        computer.shared.release_process_slot();
        error.to_string()
    })?;
    computer.boot_pid = pid;
    computer.ready.push_back(pid);
    computer.processes.push(boot_process);
    Ok(computer)
}

fn bootloader_file_path(runtime_root: &Path, path: &Path) -> PathBuf {
    runtime_root.join("bootloaders").join(path)
}

fn load_module(engine: &Engine, path: &Path) -> Result<Module, String> {
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| format!("bootloader path {} has no file extension", path.display()))?;
    let source = std::fs::read(path)
        .map_err(|error| format!("failed to read bootloader {}: {error}", path.display()))?;
    let wasm = match extension.to_ascii_lowercase().as_str() {
        "wat" => wat::parse_bytes(&source)
            .map_err(|error| format!("invalid WAT bootloader {}: {error}", path.display()))?
            .into_owned(),
        "wasm" => source,
        _ => {
            return Err(format!(
                "unsupported bootloader extension for {} (expected .wat or .wasm)",
                path.display()
            ));
        }
    };
    crate::validation::validate_module(&wasm)?;
    Module::new(engine, &wasm)
        .map_err(|error| format!("failed to compile bootloader {}: {error}", path.display()))
}

fn read_bootloader_image(path: &Path) -> Result<Vec<u8>, String> {
    let source = std::fs::read(path)
        .map_err(|error| format!("failed to read bootloader {}: {error}", path.display()))?;
    match path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "wat" => wat::parse_bytes(&source)
            .map(|bytes| bytes.into_owned())
            .map_err(|error| format!("invalid WAT bootloader {}: {error}", path.display())),
        "wasm" => Ok(source),
        _ => Err(format!(
            "unsupported bootloader extension for {}",
            path.display()
        )),
    }
}

fn add_device(
    computers: &mut [Computer],
    computer_id: ComputerId,
    device_config: DeviceConfig,
    runtime_root: &Path,
) -> Result<Uuid, String> {
    device_config.validate()?;
    let computer = computers
        .iter()
        .find(|computer| computer.id == computer_id)
        .ok_or_else(|| format!("computer {computer_id} does not exist"))?;
    if let Some(name) = device_config.host_input_name()
        && computers
            .iter()
            .any(|computer| computer.shared.devices.find_host_device(name).is_some())
    {
        return Err(format!("serial monitor name '{name}' is duplicated"));
    }
    let device = device_config
        .build(runtime_root)
        .map_err(|error| error.to_string())?;
    computer.shared.devices.insert(device)
}

fn send_serial(computers: &[Computer], monitor_name: &str, bytes: &[u8]) -> Result<(), String> {
    let (computer, device_id) = computers
        .iter()
        .find_map(|computer| {
            computer
                .shared
                .devices
                .find_host_device(monitor_name)
                .map(|id| (computer, id))
        })
        .ok_or_else(|| format!("serial monitor '{monitor_name}' does not exist"))?;
    computer
        .shared
        .devices
        .with_device_mut(device_id, |device| device.receive_host_input(bytes))
        .map_err(|_| format!("serial monitor '{monitor_name}' is unavailable"))?
}

fn remove_device(
    computers: &mut [Computer],
    computer_id: ComputerId,
    device_id: Uuid,
) -> Result<(), String> {
    let computer = computers
        .iter()
        .find(|computer| computer.id == computer_id)
        .ok_or_else(|| format!("computer {computer_id} does not exist"))?;
    drop(computer.shared.devices.remove(device_id)?);
    Ok(())
}

fn create_process(
    engine: &Engine,
    computers: &mut [Computer],
    computer_id: ComputerId,
) -> Result<ProcessId, String> {
    let computer = computers
        .iter_mut()
        .find(|computer| computer.id == computer_id)
        .ok_or_else(|| format!("computer {computer_id} does not exist"))?;
    if computer.crashed {
        return Err(format!("computer {computer_id} is crashed"));
    }
    if !computer.shared.reserve_process_slot() {
        return Err(format!("computer {computer_id} process limit reached"));
    }
    let pid = match computer.shared.allocate_process_id() {
        Ok(pid) => pid,
        Err(error) => {
            computer.shared.release_process_slot();
            return Err(error);
        }
    };
    // Host-created processes are trusted, like the boot process.
    let process = GuestProcess::new(
        engine,
        &computer.bootloader,
        computer.shared.clone(),
        computer.kernel.clone(),
        pid,
        true,
    )
    .map_err(|error| {
        computer.shared.release_process_slot();
        error.to_string()
    })?;
    computer.ready.push_back(pid);
    computer.processes.push(process);
    Ok(pid)
}

fn kill_process(computers: &mut [Computer], pid: ProcessId) -> Result<(), String> {
    // Process IDs are allocated per computer. Refuse ambiguous host requests
    // instead of silently killing whichever computer happens to be first.
    let matches = computers
        .iter()
        .filter(|computer| {
            computer
                .pending_spawns
                .iter()
                .any(|spawn| spawn.pid == pid && !spawn.cancelled)
                || computer.processes.iter().any(|process| process.pid == pid)
        })
        .count();
    if matches > 1 {
        return Err(format!(
            "process {pid} exists on multiple computers; host kill is ambiguous"
        ));
    }
    for computer in computers {
        if computer.cancel_pending_spawn(pid) {
            return Ok(());
        }
        if let Some(process) = computer.process_mut(pid) {
            if process.status.is_active() {
                process.kill();
                return Ok(());
            }
            return Err(format!("process {pid} is no longer running"));
        }
    }
    Err(format!("process {pid} does not exist"))
}

#[cfg(test)]
mod tests {
    use super::{Controller, bootloader_file_path};
    use crate::device::{ComputerSpec, CpuConfig, DeviceConfig};
    use crate::simulation::{SimulationConfig, SimulationHandle};
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    fn write_test_bootloader(source: &str, extension: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("openwc-test-{}.{}", Uuid::new_v4(), extension));
        std::fs::write(&path, source).expect("test bootloader is written");
        path
    }

    fn test_config(computer_count: usize, bootloader: PathBuf) -> SimulationConfig {
        SimulationConfig {
            max_ticks: 4,
            tick_interval_ms: 50,
            runtime_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/simulation-test"),
            computers: (0..computer_count)
                .map(|_| ComputerSpec {
                    name: None,
                    bootloader: bootloader.clone(),
                    devices: Vec::new(),
                    cpu: CpuConfig::default(),
                })
                .collect(),
        }
    }

    const TEST_BOOTLOADER: &str = r#"
(import "hal" "hal_device_count" (func $hal_device_count (result i32)))
(func (export "_start")
  call $hal_device_count
  drop
)
"#;

    #[test]
    fn controller_can_create_and_kill_processes() {
        let bootloader = write_test_bootloader(TEST_BOOTLOADER, "wat");
        let config = test_config(2, bootloader.clone());
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let process_id = simulation.create_process(0).expect("process is created");
        let next_process_id = simulation
            .create_process(1)
            .expect("second process is created");
        // PIDs are scoped to a computer, so each computer starts at PID 1.
        assert_eq!(next_process_id, process_id);
        let report = simulation.synchronize().expect("tick completes");

        assert!(report.all_finished);
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn host_kill_rejects_process_ids_shared_by_multiple_computers() {
        let bootloader = write_test_bootloader(TEST_BOOTLOADER, "wat");
        let (thread, commands) =
            Controller::start(test_config(2, bootloader.clone())).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let error = simulation
            .kill_process(1)
            .expect_err("the same PID on two computers is ambiguous");
        assert!(error.to_string().contains("ambiguous"));

        simulation.shutdown().expect("simulation shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("bootloader is removed");
    }

    #[test]
    fn syscall_completes_when_one_quantum_uses_the_full_tick_budget() {
        let bootloader = write_test_bootloader(TEST_BOOTLOADER, "wat");
        let mut config = test_config(1, bootloader.clone());
        config.computers[0].cpu.fuel_per_tick = 256;
        config.computers[0].cpu.quantum_fuel = 256;
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let mut report = simulation.synchronize().expect("first tick completes");
        for _ in 0..5 {
            if report.all_finished {
                break;
            }
            report = simulation.synchronize().expect("tick completes");
        }
        assert!(
            report.all_finished,
            "the syscall continuation must complete"
        );
        assert!(
            report
                .events
                .iter()
                .any(|event| { event.process_id == 1 && event.description == "exited" })
        );
        assert!(
            report
                .events
                .iter()
                .all(|event| !event.description.starts_with("trapped:"))
        );

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("bootloader is removed");
    }

    #[test]
    fn bootloader_exclusively_uses_cpu_until_it_exits() {
        let kernel = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 0) "\00\01\00\00\00\01\00\00\00\02\00\00\00\01\00\00")
                (func (export "kernel_get_io_buffers") (result i32) i32.const 0)
                (func (export "kernel_syscall")
                    (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 0)
            )"#,
        )
        .expect("kernel WAT compiles");
        let escaped_kernel = kernel
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>();
        let boot_source = format!(
            r#"
                (import "hal" "hal_install_kernel"
                    (func $install (param i32 i32) (result i32)))
                (import "hal" "increment" (func $increment))
                (memory (export "memory") 1)
                (data (i32.const 0) "{escaped_kernel}")
                (func (export "_start")
                    i32.const 0
                    i32.const {}
                    call $install
                    drop
                    call $increment
                )
            "#,
            kernel.len()
        );
        let bootloader = write_test_bootloader(&boot_source, "wat");
        let (thread, commands) =
            Controller::start(test_config(1, bootloader.clone())).expect("controller starts");
        let simulation = SimulationHandle::new(commands);
        simulation
            .create_process(0)
            .expect("host process is queued");

        let first = simulation.synchronize().expect("first tick completes");
        assert_eq!(first.hardware_operations, vec![(0, 0)]);
        let second = simulation.synchronize().expect("second tick completes");
        assert_eq!(second.hardware_operations, vec![(0, 2)]);

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn boot_trap_crashes_the_computer_before_siblings_run() {
        let bootloader = write_test_bootloader(
            r#"
                (import "hal" "increment" (func $increment))
                (func (export "_start")
                    unreachable
                    call $increment
                )
            "#,
            "wat",
        );
        let (thread, commands) =
            Controller::start(test_config(1, bootloader.clone())).expect("controller starts");
        let simulation = SimulationHandle::new(commands);
        simulation
            .create_process(0)
            .expect("host process is queued");

        let mut report = simulation.synchronize().expect("tick completes");
        for _ in 0..8 {
            if report.all_finished {
                break;
            }
            report = simulation.synchronize().expect("tick completes");
        }
        assert!(
            report.all_finished,
            "a boot trap permanently stops the computer"
        );
        assert_eq!(report.hardware_operations, vec![(0, 0)]);
        assert!(
            report
                .events
                .iter()
                .any(|event| event.description.contains("computer crashed"))
        );

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn deferred_boot_kernel_init_failure_crashes_and_discards_queued_children() {
        let kernel = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 0) "\00\01\00\00\00\01\00\00\00\02\00\00\00\01\00\00")
                (func (export "kernel_get_io_buffers") (result i32) i32.const 0)
                (func (export "kernel_init") unreachable)
                (func (export "kernel_syscall")
                    (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 0)
            )"#,
        )
        .expect("kernel WAT compiles");
        let child =
            wat::parse_str("(module (func (export \"_start\")))").expect("child WAT compiles");
        let escaped = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("\\{byte:02x}"))
                .collect::<String>()
        };
        let boot_source = format!(
            r#"(module
                (import "hal" "hal_install_kernel"
                    (func $install (param i32 i32) (result i32)))
                (import "hal" "hal_spawn_process_privileged"
                    (func $spawn (param i32 i32) (result i64)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (data (i32.const 4096) "{}")
                (func (export "_start")
                    i32.const 0 i32.const {} call $install drop
                    i32.const 4096 i32.const {} call $spawn drop)
            )"#,
            escaped(&kernel),
            escaped(&child),
            kernel.len(),
            child.len(),
        );
        let bootloader = write_test_bootloader(&boot_source, "wat");
        let (thread, commands) =
            Controller::start(test_config(1, bootloader.clone())).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let mut saw_boot_crash = false;
        let mut saw_child_start = false;
        let mut last_report = None;
        for _ in 0..12 {
            let report = simulation.synchronize().expect("tick completes");
            saw_boot_crash |= report.events.iter().any(|event| {
                event.process_id == 1
                    && event
                        .description
                        .contains("computer crashed: boot initialization failed")
            });
            saw_child_start |= report
                .events
                .iter()
                .any(|event| event.process_id == 2 && event.description == "spawned");
            last_report = Some(report);
            if last_report
                .as_ref()
                .is_some_and(|report| report.all_finished)
            {
                break;
            }
        }
        let report = last_report.expect("at least one tick ran");
        assert!(saw_boot_crash, "deferred init failure crashes the computer");
        assert!(
            !saw_child_start,
            "queued children are discarded on boot failure"
        );
        assert!(report.all_finished, "crashed computer has no retained work");

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("bootloader is removed");
    }

    #[test]
    fn process_limit_counts_the_boot_process() {
        let bootloader = write_test_bootloader(TEST_BOOTLOADER, "wat");
        let mut config = test_config(1, bootloader.clone());
        config.computers[0].cpu.max_processes = 1;
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);
        assert!(
            simulation
                .create_process(0)
                .unwrap_err()
                .to_string()
                .contains("process limit")
        );

        simulation.synchronize().expect("tick completes");
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn guest_hal_spawns_and_runs_arbitrary_wasm() {
        let child_wasm = wat::parse_str(
            r#"(module
                (import "hal" "increment" (func $increment))
                (func (export "_start") call $increment)
            )"#,
        )
        .expect("child module compiles");
        let escaped_child: String = child_wasm
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect();
        let parent_wat = format!(
            r#"
(import "hal" "hal_spawn_process_privileged" (func $hal_spawn_process (param i32 i32) (result i64)))
(memory (export "memory") 1)
(data (i32.const 0) "{escaped_child}")
(func (export "_start")
  i32.const 0
  i32.const {}
  call $hal_spawn_process
  drop
)
"#,
            child_wasm.len()
        );
        let bootloader = write_test_bootloader(&parent_wat, "wat");
        let config = test_config(1, bootloader.clone());
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let mut saw_spawned = false;
        let mut saw_child_exit = false;
        let mut final_hardware_operations = Vec::new();
        let mut completed = false;
        for _ in 0..16 {
            let report = simulation.synchronize().expect("tick completes");
            saw_spawned |= report
                .events
                .iter()
                .any(|event| event.process_id == 2 && event.description == "spawned");
            saw_child_exit |= report
                .events
                .iter()
                .any(|event| event.process_id == 2 && event.description == "exited");
            final_hardware_operations = report.hardware_operations;
            if report.all_finished {
                completed = true;
                break;
            }
        }
        assert!(saw_spawned, "the controller should accept the child module");
        assert!(saw_child_exit, "the child module should execute and exit");
        assert!(
            completed,
            "both bootloader and spawned process should finish"
        );
        assert_eq!(final_hardware_operations, vec![(0, 1)]);

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn live_kill_takes_effect_before_the_target_gets_its_quantum() {
        let killer = wat::parse_str(
            r#"(module
                (import "hal" "hal_kill_process" (func $kill (param i64) (result i32)))
                (func (export "_start") i64.const 3 call $kill drop)
            )"#,
        )
        .expect("killer module compiles");
        let victim = wat::parse_str(
            r#"(module
                (import "hal" "increment" (func $increment))
                (func (export "_start") call $increment)
            )"#,
        )
        .expect("victim module compiles");
        let escaped = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("\\{byte:02x}"))
                .collect::<String>()
        };
        let boot_source = format!(
            r#"(module
                (import "hal" "hal_spawn_process_privileged"
                    (func $spawn (param i32 i32) (result i64)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (data (i32.const 4096) "{}")
                (func (export "_start")
                    i32.const 0 i32.const {} call $spawn drop
                    i32.const 4096 i32.const {} call $spawn drop)
            )"#,
            escaped(&killer),
            escaped(&victim),
            killer.len(),
            victim.len(),
        );
        let bootloader = write_test_bootloader(&boot_source, "wat");
        let (thread, commands) =
            Controller::start(test_config(1, bootloader.clone())).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let mut reports = Vec::new();
        for _ in 0..8 {
            let report = simulation.synchronize().expect("tick completes");
            let done = report.all_finished;
            reports.push(report);
            if done {
                break;
            }
        }
        assert!(reports.last().is_some_and(|report| report.all_finished));
        assert!(reports.iter().any(|report| {
            report
                .events
                .iter()
                .any(|event| event.process_id == 3 && event.description == "killed")
        }));
        assert_eq!(reports.last().unwrap().hardware_operations, vec![(0, 0)]);
        assert!(
            reports.iter().any(|report| report
                .events
                .iter()
                .any(|event| { event.process_id == 2 && event.description == "exited" })),
            "the killer should complete its HAL call"
        );

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("bootloader is removed");
    }

    #[test]
    fn processes_on_one_computer_share_a_round_robin_cpu_budget() {
        let calls = "call $increment\n".repeat(80);
        let source = format!(
            r#"(module
                (import "hal" "increment" (func $increment))
                (func (export "_start")
                    {calls}
                )
            )"#
        );
        let bootloader = write_test_bootloader(&source, "wat");
        let mut config = test_config(1, bootloader.clone());
        config.computers[0].cpu = CpuConfig {
            fuel_per_tick: 256,
            quantum_fuel: 64,
            ..CpuConfig::default()
        };
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);
        simulation
            .create_process(0)
            .expect("first user process is created");
        simulation
            .create_process(0)
            .expect("second user process is created");

        let mut report = simulation.synchronize().expect("first tick completes");
        for _ in 0..15 {
            if report.all_finished {
                break;
            }
            report = simulation.synchronize().expect("tick completes");
        }

        assert!(report.all_finished, "all scheduled processes should exit");
        assert_eq!(report.hardware_operations, vec![(0, 240)]);
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn unprivileged_child_cannot_import_privileged_hal() {
        let child_wasm = wat::parse_str(
            r#"(module
                (import "hal" "hal_device_call"
                    (func $device_call (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "_start"))
            )"#,
        )
        .expect("unprivileged child module compiles");
        let escaped_child: String = child_wasm
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect();
        let parent_wat = format!(
            r#"(module
                (import "hal" "hal_spawn_process"
                    (func $spawn (param i32 i32) (result i64)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{escaped_child}")
                (func (export "_start")
                    i32.const 0
                    i32.const {}
                    call $spawn
                    drop
                )
            )"#,
            child_wasm.len()
        );
        let bootloader = write_test_bootloader(&parent_wat, "wat");
        let (thread, commands) =
            Controller::start(test_config(1, bootloader.clone())).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let mut child_trapped = false;
        for _ in 0..8 {
            let report = simulation.synchronize().expect("tick completes");
            child_trapped |= report.events.iter().any(|event| {
                event.process_id == 2
                    && event.description.starts_with("trapped:")
                    && event.description.contains("hal_device_call")
            });
            if report.all_finished {
                break;
            }
        }
        assert!(
            child_trapped,
            "an unprivileged child must fail to link a privileged HAL import"
        );
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn syscall_waits_for_next_tick_and_returns_without_guest_retry() {
        let disk_id = Uuid::new_v4();
        let escaped_uuid = disk_id
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>();
        let device_calls = "i32.const 0
                    i32.const 17
                    i32.const 16
                    i32.const 0
                    i32.const 32
                    i32.const 4
                    call $device_call
                    i32.const 0
                    i32.ne
                    if unreachable end
                    i32.const 32
                    i32.load
                    i32.const 2
                    i32.ne
                    if unreachable end
                    "
        .repeat(4);
        let child_source = format!(
            r#"(module
                (import "hal" "hal_device_call"
                    (func $device_call (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{escaped_uuid}")
                (func (export "_start")
                    {device_calls}
                )
            )"#
        );
        let child_wasm = wat::parse_str(child_source).expect("child WAT compiles");
        let escaped_child = child_wasm
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>();
        let parent_source = format!(
            r#"(module
                (import "hal" "hal_spawn_process_privileged"
                    (func $spawn (param i32 i32) (result i64)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{escaped_child}")
                (func (export "_start")
                    i32.const 0
                    i32.const {}
                    call $spawn
                    drop
                    i32.const 0
                    i32.const {}
                    call $spawn
                    drop
                )
            )"#,
            child_wasm.len(),
            child_wasm.len()
        );
        let bootloader = write_test_bootloader(&parent_source, "wat");
        let mut config = test_config(1, bootloader.clone());
        config.computers[0].cpu = CpuConfig {
            fuel_per_tick: 48,
            quantum_fuel: 16,
            ..CpuConfig::default()
        };
        config.computers[0].devices.push(DeviceConfig::Disk {
            uuid: Some(disk_id.to_string()),
            is_root: true,
            block_count: 2,
            block_size: 8,
            remove_on_drop: true,
        });
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let first_tick = simulation.synchronize().expect("first tick completes");
        assert!(
            !first_tick
                .events
                .iter()
                .any(|event| event.description == "spawned"),
            "children compile for a tick before they are created"
        );
        let mut second_tick = simulation.synchronize().expect("second tick completes");
        for _ in 0..4 {
            if second_tick
                .events
                .iter()
                .any(|event| event.description == "spawned")
            {
                break;
            }
            second_tick = simulation.synchronize().expect("tick completes");
        }
        assert!(
            second_tick
                .events
                .iter()
                .any(|event| event.description == "spawned"),
            "the privileged bootloader should create a privileged child after boot completes"
        );

        let mut saw_waiting_syscall = false;
        let mut final_report = second_tick;
        for _ in 0..8 {
            saw_waiting_syscall |= final_report
                .events
                .iter()
                .any(|event| event.description == "waiting for CPU budget");
            if final_report.all_finished {
                break;
            }
            final_report = simulation.synchronize().expect("tick completes");
        }
        saw_waiting_syscall |= final_report
            .events
            .iter()
            .any(|event| event.description == "waiting for CPU budget");
        assert!(
            saw_waiting_syscall,
            "a child syscall should wait after the first child consumes the tick budget"
        );
        assert!(
            final_report.all_finished,
            "the original syscall continuation should resume and finish"
        );
        // Two children, four device calls each.
        assert_eq!(final_report.hardware_operations, vec![(0, 8)]);
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn computers_and_devices_can_be_added_at_runtime() {
        let bootloader = write_test_bootloader(TEST_BOOTLOADER, "wat");
        let config = SimulationConfig {
            max_ticks: 4,
            tick_interval_ms: 50,
            runtime_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/simulation-test"),
            computers: Vec::new(),
        };
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let computer_id = simulation
            .spawn_computer(ComputerSpec {
                name: None,
                bootloader: bootloader.clone(),
                devices: Vec::new(),
                cpu: CpuConfig::default(),
            })
            .expect("computer spawns");
        assert_eq!(computer_id, 0);

        let device_id = Uuid::new_v4();
        let device_config = DeviceConfig::Disk {
            uuid: Some(device_id.to_string()),
            is_root: false,
            block_count: 2,
            block_size: 8,
            remove_on_drop: true,
        };
        assert_eq!(
            simulation
                .add_device(computer_id, device_config)
                .expect("device is added"),
            device_id
        );
        simulation
            .remove_device(computer_id, device_id)
            .expect("device is removed");
        assert!(simulation.remove_device(computer_id, device_id).is_err());

        let report = simulation.synchronize().expect("simulation ticks");
        assert!(report.all_finished);
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");

        assert!(
            !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/simulation-test/data")
                .join("disks")
                .join(device_id.to_string())
                .exists()
        );
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn named_serial_monitors_route_input_to_distinct_devices_on_one_computer() {
        let north_id = Uuid::new_v4();
        let south_id = Uuid::new_v4();
        let escaped = |uuid: Uuid| {
            uuid.as_bytes()
                .iter()
                .map(|byte| format!("\\{byte:02x}"))
                .collect::<String>()
        };
        let source = format!(
            r#"(module
                (import "hal" "hal_device_call"
                    (func $call (param i32 i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{}")
                (data (i32.const 16) "{}")
                (func (export "_start")
                    i32.const 0 i32.const 1 i32.const 0 i32.const 0
                    i32.const 64 i32.const 16 call $call
                    i32.const 0 i32.ne if unreachable end
                    i32.const 64 i32.load8_u i32.const 110 i32.ne if unreachable end

                    i32.const 16 i32.const 1 i32.const 0 i32.const 0
                    i32.const 80 i32.const 16 call $call
                    i32.const 0 i32.ne if unreachable end
                    i32.const 80 i32.load8_u i32.const 115 i32.ne if unreachable end
                )
            )"#,
            escaped(north_id),
            escaped(south_id),
        );
        let bootloader = write_test_bootloader(&source, "wat");
        let mut config = test_config(0, bootloader.clone());
        config.computers.push(ComputerSpec {
            name: Some("lab".into()),
            bootloader: bootloader.clone(),
            devices: vec![
                DeviceConfig::SerialMonitor {
                    name: "north".into(),
                    uuid: Some(north_id.to_string()),
                },
                DeviceConfig::SerialMonitor {
                    name: "south".into(),
                    uuid: Some(south_id.to_string()),
                },
            ],
            cpu: CpuConfig::default(),
        });

        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);
        simulation
            .send_serial("south", b"south\n")
            .expect("south accepts input");
        simulation
            .send_serial("north", b"north\n")
            .expect("north accepts input");
        assert!(simulation.send_serial("unknown", b"message\n").is_err());
        assert!(
            simulation
                .add_device(
                    0,
                    DeviceConfig::SerialMonitor {
                        name: "north".into(),
                        uuid: None,
                    }
                )
                .is_err()
        );

        let mut report = simulation.synchronize().expect("tick completes");
        for _ in 0..8 {
            if report.all_finished {
                break;
            }
            report = simulation.synchronize().expect("tick completes");
        }
        assert!(
            report.all_finished,
            "guest should consume both input streams: {:?}",
            report
                .events
                .iter()
                .map(|event| &event.description)
                .collect::<Vec<_>>()
        );
        assert!(
            report
                .events
                .iter()
                .any(|event| event.description == "exited")
        );
        assert!(
            !report
                .events
                .iter()
                .any(|event| event.description.starts_with("trapped:"))
        );
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn wasm_bootloader_file_is_accepted_without_import_injection() {
        let wat_path = write_test_bootloader(TEST_BOOTLOADER, "wat");
        let wasm_path = wat_path.with_extension("wasm");
        let wasm = wat::parse_file(&wat_path).expect("test WAT parses");
        std::fs::write(&wasm_path, wasm).expect("test Wasm file is written");
        let config = test_config(1, wasm_path.clone());
        let (thread, commands) = Controller::start(config).expect("Wasm bootloader loads");
        let simulation = SimulationHandle::new(commands);

        let report = simulation.synchronize().expect("tick completes");
        assert!(report.all_finished);

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(wat_path).expect("test WAT is removed");
        std::fs::remove_file(wasm_path).expect("test Wasm is removed");
    }

    #[test]
    fn hal_device_call_selects_device_by_uuid() {
        let device_uuid = Uuid::new_v4();
        let escaped_uuid = device_uuid
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>();
        let source = format!(
            r#"
(import "hal" "hal_device_call"
  (func $device_call (param i32 i32 i32 i32 i32 i32) (result i32)))
(memory (export "memory") 1)
(data (i32.const 0) "{escaped_uuid}")
(func (export "_start")
  i32.const 0
  i32.const 0x11
  i32.const 0
  i32.const 0
  i32.const 32
  i32.const 4
  call $device_call
  i32.const 0
  i32.ne
  if
    unreachable
  end
  i32.const 32
  i32.load
  i32.const 2
  i32.ne
  if
    unreachable
  end
)
"#
        );
        let bootloader = write_test_bootloader(&source, "wat");
        let mut config = test_config(0, bootloader.clone());
        config.computers.push(ComputerSpec {
            name: None,
            bootloader: bootloader.clone(),
            devices: vec![DeviceConfig::Disk {
                uuid: Some(device_uuid.to_string()),
                is_root: false,
                block_count: 2,
                block_size: 8,
                remove_on_drop: true,
            }],
            cpu: CpuConfig::default(),
        });

        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);
        let mut completed = false;
        for _ in 0..8 {
            let report = simulation.synchronize().expect("simulation tick completes");
            assert!(
                report.hardware_operations == vec![(0, 0)]
                    || report.hardware_operations == vec![(0, 1)],
                "guest should call the selected device exactly once"
            );
            if report
                .events
                .iter()
                .any(|event| event.process_id == 1 && event.description == "exited")
            {
                assert_eq!(report.hardware_operations, vec![(0, 1)]);
                completed = true;
                break;
            }
            assert!(
                report
                    .events
                    .iter()
                    .all(|event| !event.description.starts_with("trapped:")),
                "guest should not trap while calling the device by UUID: {:?}",
                report
                    .events
                    .iter()
                    .map(|event| &event.description)
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            completed,
            "guest should exit after calling the device by UUID"
        );

        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
    }

    #[test]
    fn bootloader_path_is_relative_to_example_root() {
        let root = Path::new("examples/basic");
        let path = Path::new("bootloader.wat");
        assert_eq!(
            bootloader_file_path(root, path),
            Path::new("examples/basic/bootloaders/bootloader.wat")
        );
        assert_eq!(
            bootloader_file_path(root, Path::new("thing/bootloader.wat")),
            Path::new("examples/basic/bootloaders/thing/bootloader.wat")
        );
    }
}

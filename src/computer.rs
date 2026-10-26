use std::collections::VecDeque;
use std::sync::Arc;

use crate::compile::CompileJob;
use crate::device::CpuConfig;
use crate::guest::GuestProcess;
use crate::hal::{ComputerShared, ExitReason};
use crate::kernel::KernelSlot;
use crate::simulation::{ComputerId, ProcessId};
use wasmtime::Module;

/// A spawn request whose module is compiling on a worker thread. The process
/// is created at the next tick's request processing.
pub(crate) struct PendingSpawn {
    pub(crate) pid: ProcessId,
    pub(crate) privileged: bool,
    pub(crate) compile: CompileJob,
    pub(crate) cancelled: bool,
}

pub(crate) struct Computer {
    pub(crate) id: ComputerId,
    pub(crate) name: String,
    pub(crate) shared: Arc<ComputerShared>,
    pub(crate) kernel: Arc<KernelSlot>,
    pub(crate) processes: Vec<GuestProcess>,
    pub(crate) pending_spawns: Vec<PendingSpawn>,
    pub(crate) ready: VecDeque<ProcessId>,
    pub(crate) bootloader: Module,
    pub(crate) boot_pid: ProcessId,
    pub(crate) crashed: bool,
}

impl Computer {
    pub(crate) fn new(id: ComputerId, name: String, bootloader: Module, cpu: CpuConfig) -> Self {
        Self {
            id,
            name,
            shared: Arc::new(ComputerShared::new(
                cpu.fuel_per_tick,
                cpu.quantum_fuel,
                cpu.max_processes as usize,
                usize::try_from(cpu.max_memory_bytes).unwrap_or(usize::MAX),
            )),
            kernel: Arc::new(KernelSlot::default()),
            processes: Vec::new(),
            pending_spawns: Vec::new(),
            ready: VecDeque::new(),
            bootloader,
            boot_pid: 0,
            crashed: false,
        }
    }

    pub(crate) fn process_mut(&mut self, pid: ProcessId) -> Option<&mut GuestProcess> {
        self.processes.iter_mut().find(|process| process.pid == pid)
    }

    pub(crate) fn active_processes(&self) -> impl Iterator<Item = &GuestProcess> {
        self.processes
            .iter()
            .filter(|process| process.status.is_active())
    }

    /// Whether a spawned child is still waiting to be created.
    pub(crate) fn has_pending_spawns(&self) -> bool {
        self.pending_spawns.iter().any(|spawn| !spawn.cancelled)
    }

    /// Cancels a child that has not started yet. It never runs, and the
    /// kernel is told it was killed. Returns false if there is no such child.
    pub(crate) fn cancel_pending_spawn(&mut self, pid: ProcessId) -> bool {
        let Some(spawn) = self
            .pending_spawns
            .iter_mut()
            .find(|spawn| spawn.pid == pid && !spawn.cancelled)
        else {
            return false;
        };
        spawn.cancelled = true;
        self.shared.unmark_pending_spawn(pid);
        self.shared.release_process_slot();
        self.shared.record_exit(pid, ExitReason::Killed);
        true
    }
}

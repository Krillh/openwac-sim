use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wasmtime::{Engine, Linker, Module, Store, TypedFunc};

use crate::hal::{ComputerShared, ExitReason, SyncGate, WakeToken};
use crate::hal_functions::define_hal_functions;
use crate::kernel::KernelSlot;
use crate::memory::MemoryLimiter;
use crate::simulation::ProcessId;
use crate::syscall::define_sys_functions;

const PROCESS_FUEL_LIMIT: u64 = u64::MAX;

type ProcessFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Raised by a host function to end the calling process normally.
#[derive(Debug)]
pub(crate) struct ProcessExit;

impl std::fmt::Display for ProcessExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("process exited")
    }
}

impl std::error::Error for ProcessExit {}

pub(crate) struct ProcessCtx {
    pub(crate) computer: Arc<ComputerShared>,
    /// `None` for the kernel's own store, which cannot call itself.
    pub(crate) kernel: Option<Arc<KernelSlot>>,
    pub(crate) pid: ProcessId,
    pub(crate) privileged: bool,
    pub(crate) sync: SyncGate,
    pub(crate) waiting_for_sync: Arc<AtomicBool>,
    pub(crate) waiting_for_budget: Arc<AtomicBool>,
    pub(crate) waiting_for_wake: Arc<AtomicBool>,
    pub(crate) wake: Arc<WakeToken>,
    pub(crate) in_kernel_call: Arc<AtomicBool>,
    pub(crate) yield_requested: Arc<AtomicBool>,
    pub(crate) memory: MemoryLimiter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuestProcessStatus {
    Runnable,
    WaitingForSync,
    WaitingForBudget,
    WaitingForWake,
    Exited,
    Killed,
    Trapped(String),
}

impl GuestProcessStatus {
    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Runnable | Self::WaitingForSync | Self::WaitingForBudget | Self::WaitingForWake
        )
    }
}

pub(crate) struct GuestProcess {
    pub(crate) pid: ProcessId,
    pub(crate) status: GuestProcessStatus,
    pub(crate) privileged: bool,
    future: ProcessFuture,
    pub(crate) sync: SyncGate,
    shared: Arc<ComputerShared>,
    waiting_for_sync: Arc<AtomicBool>,
    waiting_for_budget: Arc<AtomicBool>,
    waiting_for_wake: Arc<AtomicBool>,
    wake: Arc<WakeToken>,
    in_kernel_call: Arc<AtomicBool>,
    yield_requested: Arc<AtomicBool>,
    kill_pending: bool,
    slot_reserved: bool,
}

impl GuestProcess {
    pub(crate) fn new(
        engine: &Engine,
        module: &Module,
        computer: Arc<ComputerShared>,
        kernel: Arc<KernelSlot>,
        pid: ProcessId,
        privileged: bool,
    ) -> Result<Self, wasmtime::Error> {
        let sync = SyncGate::new();
        let waiting_for_sync = Arc::new(AtomicBool::new(false));
        let waiting_for_budget = Arc::new(AtomicBool::new(false));
        let waiting_for_wake = Arc::new(AtomicBool::new(false));
        let in_kernel_call = Arc::new(AtomicBool::new(false));
        let yield_requested = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(WakeToken::default());
        let context = ProcessCtx {
            computer: Arc::clone(&computer),
            kernel: Some(kernel),
            pid,
            privileged,
            sync: sync.clone(),
            waiting_for_sync: Arc::clone(&waiting_for_sync),
            waiting_for_budget: Arc::clone(&waiting_for_budget),
            waiting_for_wake: Arc::clone(&waiting_for_wake),
            wake: Arc::clone(&wake),
            in_kernel_call: Arc::clone(&in_kernel_call),
            yield_requested: Arc::clone(&yield_requested),
            memory: computer.new_memory_limiter(),
        };

        let mut store = Store::new(engine, context);
        store.limiter(|context| &mut context.memory);
        store.set_fuel(PROCESS_FUEL_LIMIT)?;
        let quantum_fuel = store.data().computer.cpu_budget.quantum_fuel();
        store.fuel_async_yield_interval(Some(quantum_fuel))?;

        let mut linker = Linker::new(engine);
        define_hal_functions(&mut linker, privileged)?;
        define_sys_functions(&mut linker)?;

        let module = module.clone();
        let future = Box::pin(async move {
            let instance = linker
                .instantiate_async(&mut store, &module)
                .await
                .map_err(|error| error.to_string())?;
            let entry: TypedFunc<(), ()> = instance
                .get_typed_func(&mut store, "_start")
                .map_err(|error| error.to_string())?;
            match entry.call_async(&mut store, ()).await {
                Ok(()) => Ok(()),
                Err(error) if error.downcast_ref::<ProcessExit>().is_some() => Ok(()),
                Err(error) => Err(error.to_string()),
            }
        });

        computer.register_wake_token(pid, Arc::clone(&wake));
        Ok(Self {
            pid,
            status: GuestProcessStatus::Runnable,
            privileged,
            future,
            sync,
            shared: computer,
            waiting_for_sync,
            waiting_for_budget,
            waiting_for_wake,
            wake,
            in_kernel_call,
            yield_requested,
            kill_pending: false,
            slot_reserved: true,
        })
    }

    pub(crate) fn future_mut(
        &mut self,
    ) -> Pin<&mut (dyn Future<Output = Result<(), String>> + Send)> {
        self.future.as_mut()
    }

    pub(crate) fn is_waiting_for_sync(&self) -> bool {
        self.waiting_for_sync.load(Ordering::Acquire)
    }

    pub(crate) fn is_waiting_for_budget(&self) -> bool {
        self.waiting_for_budget.load(Ordering::Acquire)
    }

    pub(crate) fn is_waiting_for_wake(&self) -> bool {
        self.waiting_for_wake.load(Ordering::Acquire)
    }

    pub(crate) fn is_woken(&self) -> bool {
        self.wake.is_pending()
    }

    pub(crate) fn is_yielding(&self) -> bool {
        self.yield_requested.load(Ordering::Acquire)
    }

    pub(crate) fn clear_yield(&self) {
        self.yield_requested.store(false, Ordering::Release);
    }

    /// Records the end of the process and queues the kernel's exit notification.
    pub(crate) fn finish(&mut self, status: GuestProcessStatus) {
        if self.status.is_active() {
            let reason = match &status {
                GuestProcessStatus::Exited => ExitReason::Exited,
                GuestProcessStatus::Trapped(_) => ExitReason::Trapped,
                _ => ExitReason::Killed,
            };
            self.shared.record_exit(self.pid, reason);
            if self.slot_reserved {
                self.shared.release_process_slot();
                self.slot_reserved = false;
            }
        }
        self.status = status;
        self.waiting_for_sync.store(false, Ordering::Release);
        self.waiting_for_budget.store(false, Ordering::Release);
        self.waiting_for_wake.store(false, Ordering::Release);
        self.shared.unregister_wake_token(self.pid);
        // Keep the terminal process record for status reporting, but release
        // its Wasmtime store now. In particular, retaining a completed BIOS
        // store would keep its linear memory charged and prevent the kernel
        // from using that RAM after boot.
        self.future = Box::pin(async { Ok(()) });
    }

    /// Kills the process. A process inside a kernel call cannot be dropped
    /// mid-call without corrupting the kernel, so the kill is deferred until
    /// the call returns; this returns `false` in that case.
    pub(crate) fn kill(&mut self) -> bool {
        if self.in_kernel_call.load(Ordering::Acquire) {
            self.kill_pending = true;
            return false;
        }
        self.kill_pending = false;
        self.finish(GuestProcessStatus::Killed);
        self.future = Box::pin(async { Ok(()) });
        true
    }

    /// Applies a deferred kill once the process has left its kernel call.
    pub(crate) fn apply_deferred_kill(&mut self) -> bool {
        self.kill_pending && self.status.is_active() && self.kill()
    }

    pub(crate) fn is_kill_pending(&self) -> bool {
        self.kill_pending
    }
}

impl Drop for GuestProcess {
    fn drop(&mut self) {
        if self.slot_reserved {
            self.shared.release_process_slot();
            self.slot_reserved = false;
        }
        self.shared.unregister_wake_token(self.pid);
    }
}

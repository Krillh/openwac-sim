//! The kernel is a passive Wasm instance with its own store. It is installed
//! once by a privileged process and afterwards only runs when a process makes
//! a syscall (or when the host reports that a process ended), always on the
//! caller's task.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use log::warn;
use tokio::sync::Mutex as AsyncMutex;
use wasmtime::{Engine, Linker, Memory, Store, TypedFunc};

use crate::compile::CompileJob;
use crate::device::device_status;
use crate::guest::{ProcessCtx, ProcessExit};
use crate::hal::{ComputerShared, ExitReason, SyncGate, WakeToken};
use crate::hal_functions::define_kernel_hal_functions;
use crate::simulation::{ProcessId, poll_once};

/// Process ID the kernel reports for its own events.
pub(crate) const KERNEL_PID: ProcessId = 0;

/// Kernel result: the call cannot finish yet; park the caller until
/// `hal_wake` and run the same call again.
pub(crate) const SYSCALL_BLOCK: i32 = -100;
/// Kernel result: end the calling process as exited.
pub(crate) const SYSCALL_TERMINATE_CALLER: i32 = -101;
/// Bit 0 of the `flags` argument passed to `kernel_syscall`.
pub(crate) const CALLER_PRIVILEGED: i32 = 1;

/// The kernel has no fuel cap; consumption is measured and charged to the caller.
const KERNEL_FUEL: u64 = u64::MAX;
const IO_DESCRIPTOR_SIZE: usize = 16;
const MAX_KERNEL_INPUT_CAPACITY: usize = 256 * 1024;
const MAX_KERNEL_OUTPUT_CAPACITY: usize = 16 * 1024;

type SyscallEntry = TypedFunc<(i64, i32, i32, i32, i32), i32>;
type ExitHook = TypedFunc<(i64, i32), ()>;

#[derive(Default)]
struct KernelIo {
    in_ptr: usize,
    in_cap: usize,
    out_ptr: usize,
    out_cap: usize,
}

pub(crate) struct KernelService {
    store: Store<ProcessCtx>,
    memory: Memory,
    syscall: SyscallEntry,
    exit_hook: Option<ExitHook>,
    init: Option<TypedFunc<(), ()>>,
    io_descriptor: TypedFunc<(), i32>,
    io_prepare: Option<TypedFunc<(i32, i32), i32>>,
    lazy_init_pending: bool,
    prepared_input_capacity: usize,
    prepared_output_capacity: usize,
    quantum_fuel: u64,
    io: KernelIo,
}

/// The wait flags of the process whose time the kernel is running on.
pub(crate) struct CallerBinding {
    pub(crate) sync: SyncGate,
    pub(crate) waiting_for_sync: Arc<AtomicBool>,
    pub(crate) waiting_for_budget: Arc<AtomicBool>,
    pub(crate) waiting_for_wake: Arc<AtomicBool>,
    pub(crate) in_kernel_call: Arc<AtomicBool>,
}

pub(crate) enum KernelOutcome {
    /// `result` is the value returned to the caller; `response` holds the
    /// bytes to copy into the caller's output buffer.
    Done {
        result: i32,
        response: Vec<u8>,
    },
    Block,
    Terminate,
}

pub(crate) struct KernelCall {
    pub(crate) outcome: Result<KernelOutcome, i32>,
}

#[derive(Default)]
pub(crate) struct KernelSlot {
    service: AsyncMutex<Option<KernelService>>,
    maintenance: Mutex<Option<MaintenanceFuture>>,
    /// Set from the start of an install until it fails or the kernel dies;
    /// it makes a second install fail while the first is still compiling.
    installing: AtomicBool,
    /// Remains set after a successful install so the service cannot be replaced.
    installed: AtomicBool,
    dead: AtomicBool,
    panic_report: Mutex<Option<String>>,
    boot_failure: Mutex<Option<String>>,
}

type MaintenanceFuture = Pin<Box<dyn Future<Output = (KernelService, Result<(), String>)> + Send>>;

/// An install that has started compiling. Dropping it before `finish_install`
/// succeeds (for example because the installer was killed) releases the claim
/// so the kernel can be installed again.
pub(crate) struct InstallJob {
    slot: Arc<KernelSlot>,
    compile: Option<CompileJob>,
    defer_init: bool,
    completed: bool,
}

impl Drop for InstallJob {
    fn drop(&mut self) {
        if !self.completed {
            self.slot.installing.store(false, Ordering::Release);
        }
    }
}

struct InKernelCall<'a>(&'a AtomicBool);

impl<'a> InKernelCall<'a> {
    fn enter(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}

impl Drop for InKernelCall<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl KernelSlot {
    pub(crate) fn is_maintenance_pending(&self) -> bool {
        self.maintenance
            .lock()
            .expect("kernel maintenance lock poisoned")
            .is_some()
    }

    /// Whether deferred boot-time initialization is pending or running.
    pub(crate) fn deferred_init_pending(&self) -> bool {
        if self.is_maintenance_pending() {
            return true;
        }
        self.service
            .try_lock()
            .ok()
            .and_then(|service| service.as_ref().map(|service| service.lazy_init_pending))
            .unwrap_or(false)
    }

    /// Whether queued process exits can trigger deferred initialization or a
    /// kernel callback. This lets the controller finish promptly when no
    /// kernel work is pending, while still draining work after the last guest exits.
    pub(crate) fn needs_exit_delivery(&self) -> bool {
        if self.is_maintenance_pending() {
            return true;
        }
        let Ok(service) = self.service.try_lock() else {
            return true;
        };
        service
            .as_ref()
            .is_some_and(|service| service.lazy_init_pending || service.exit_hook.is_some())
    }

    /// Child processes wait for deferred BIOS kernel initialization to finish.
    pub(crate) fn processes_ready(&self) -> bool {
        if self.installing.load(Ordering::Acquire) || self.is_maintenance_pending() {
            return false;
        }
        // A kernel panic disables syscalls, but it must not prevent already
        // queued privileged processes from starting.
        if self.is_dead() {
            return true;
        }
        let Ok(service) = self.service.try_lock() else {
            return false;
        };
        service
            .as_ref()
            .is_none_or(|service| !service.lazy_init_pending)
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    pub(crate) fn take_panic_report(&self) -> Option<String> {
        self.panic_report
            .lock()
            .expect("kernel panic report lock poisoned")
            .take()
    }

    /// Takes a deferred boot-time kernel initialization failure. The runtime
    /// treats this as a failed boot rather than a recoverable kernel panic.
    pub(crate) fn take_boot_failure(&self) -> Option<String> {
        self.boot_failure
            .lock()
            .expect("kernel boot failure lock poisoned")
            .take()
    }

    fn record_boot_failure(&self, message: String) {
        self.dead.store(true, Ordering::Release);
        let mut report = self
            .boot_failure
            .lock()
            .expect("kernel boot failure lock poisoned");
        report.get_or_insert(message);
    }

    fn mark_panicked(&self, message: String) {
        warn!("kernel panic: {message}");
        self.dead.store(true, Ordering::Release);
        let mut report = self
            .panic_report
            .lock()
            .expect("kernel panic report lock poisoned");
        report.get_or_insert(message);
    }

    /// Claims the install and starts compiling the kernel on a worker thread.
    /// The caller should wait a tick before `finish_install` so the compile
    /// overlaps idle time.
    pub(crate) fn begin_install(
        self: &Arc<Self>,
        engine: &Engine,
        wasm: Vec<u8>,
        defer_init: bool,
    ) -> Result<InstallJob, i32> {
        if self.is_dead()
            || self.installed.load(Ordering::Acquire)
            || self
                .installing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(device_status::KERNEL_ALREADY_INSTALLED);
        }
        Ok(InstallJob {
            slot: Arc::clone(self),
            compile: Some(CompileJob::start(engine, wasm)),
            defer_init,
            completed: false,
        })
    }

    /// Finishes an install: waits for the compile, instantiates the kernel
    /// and runs `kernel_init`. Returns a status code if installation fails.
    pub(crate) async fn finish_install(
        &self,
        mut job: InstallJob,
        engine: &Engine,
        computer: Arc<ComputerShared>,
    ) -> Result<(), i32> {
        let mut guard = self.service.lock().await;
        let invalid = |what: &str, error: &dyn std::fmt::Display| {
            warn!("kernel install failed: {what}: {error}");
            device_status::KERNEL_INVALID
        };

        let compile = job
            .compile
            .take()
            .ok_or_else(|| invalid("compile", &"no compile job"))?;
        let module = compile.wait().map_err(|error| invalid("compile", &error))?;
        let quantum_fuel = computer.cpu_budget.quantum_fuel();
        let mut store = Store::new(engine, kernel_context(Arc::clone(&computer)));
        store
            .set_fuel(KERNEL_FUEL)
            .map_err(|error| invalid("fuel", &error))?;
        store
            .fuel_async_yield_interval(Some(computer.cpu_budget.quantum_fuel()))
            .map_err(|error| invalid("fuel yield interval", &error))?;
        let mut linker = Linker::new(engine);
        define_kernel_hal_functions(&mut linker).map_err(|error| invalid("linker", &error))?;
        store.limiter(|context| &mut context.memory);
        let instance = linker
            .instantiate_async(&mut store, &module)
            .await
            .map_err(|error| invalid("instantiate", &error))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| invalid("exports", &"missing memory"))?;
        let syscall: SyscallEntry = instance
            .get_typed_func(&mut store, "kernel_syscall")
            .map_err(|error| invalid("exports", &error))?;
        let io_descriptor: TypedFunc<(), i32> = instance
            .get_typed_func(&mut store, "kernel_get_io_buffers")
            .map_err(|error| invalid("exports", &error))?;
        let io_prepare: Option<TypedFunc<(i32, i32), i32>> = instance
            .get_typed_func(&mut store, "kernel_prepare_io_buffers")
            .ok();
        let init: Option<TypedFunc<(), ()>> =
            instance.get_typed_func(&mut store, "kernel_init").ok();
        let exit_hook: Option<ExitHook> = instance
            .get_typed_func(&mut store, "kernel_process_exited")
            .ok();

        let lazy_init_pending = job.defer_init;
        let exit_notifications_enabled = !lazy_init_pending && exit_hook.is_some();
        let io = {
            // Minecraft validates the initial descriptor before install
            // returns even when it defers kernel_init until BIOS exit.
            if !lazy_init_pending {
                if let Some(init) = &init {
                    init.call_async(&mut store, ())
                        .await
                        .map_err(|error| invalid("kernel_init", &error))?;
                }
            }
            let descriptor_ptr = io_descriptor
                .call_async(&mut store, ())
                .await
                .map_err(|error| invalid("kernel_get_io_buffers", &error))?;
            read_io_descriptor(&memory, &store, descriptor_ptr).ok_or_else(|| {
                invalid(
                    "kernel_get_io_buffers",
                    &"descriptor is outside kernel memory",
                )
            })?
        };
        let prepared_input_capacity = io.in_cap.min(MAX_KERNEL_INPUT_CAPACITY);
        let prepared_output_capacity = io.out_cap.min(MAX_KERNEL_OUTPUT_CAPACITY);

        *guard = Some(KernelService {
            store,
            memory,
            syscall,
            exit_hook,
            init,
            io_descriptor,
            io_prepare,
            lazy_init_pending,
            prepared_input_capacity,
            prepared_output_capacity,
            quantum_fuel,
            io,
        });
        self.installed.store(true, Ordering::Release);
        self.installing.store(false, Ordering::Release);
        computer.set_exit_notifications(exit_notifications_enabled);
        job.completed = true;
        Ok(())
    }

    /// Runs one syscall inside the kernel on the caller's task.
    pub(crate) async fn call(
        &self,
        binding: &CallerBinding,
        pid: ProcessId,
        privileged: bool,
        number: i32,
        request: &[u8],
        out_cap: usize,
    ) -> KernelCall {
        let failed = |code| KernelCall { outcome: Err(code) };
        if self.is_dead() {
            return failed(device_status::KERNEL_PANICKED);
        }
        let mut guard = self.service.lock().await;
        if self.is_dead() {
            return failed(device_status::KERNEL_PANICKED);
        }
        let Some(service) = guard.as_mut() else {
            return failed(device_status::KERNEL_NOT_INSTALLED);
        };
        if service.lazy_init_pending {
            return failed(device_status::KERNEL_NOT_INSTALLED);
        }
        if request.len() > MAX_KERNEL_INPUT_CAPACITY
            || (service.io_prepare.is_none() && request.len() > service.io.in_cap)
        {
            return failed(device_status::ARGUMENT_OVERRUN);
        }
        let out_cap = if service.io_prepare.is_some() {
            out_cap.min(MAX_KERNEL_OUTPUT_CAPACITY)
        } else {
            out_cap.min(service.io.out_cap)
        };

        service.bind_caller(binding);
        let _in_call = InKernelCall::enter(&binding.in_kernel_call);
        let result = service.run(pid, privileged, number, request, out_cap).await;
        match result {
            Ok(call) => call,
            Err(message) => {
                service.store.data().computer.set_exit_notifications(false);
                *guard = None;
                self.mark_panicked(message);
                failed(device_status::KERNEL_PANICKED)
            }
        }
    }

    /// Tells the kernel about processes that ended. This runs outside any
    /// process's time, so it is not charged and must not block. If a caller
    /// is suspended inside the kernel, the notification waits for a later tick.
    pub(crate) async fn deliver_exits(
        &self,
        computer: &Arc<ComputerShared>,
        maintenance_fuel: u64,
    ) {
        if maintenance_fuel == 0 {
            return;
        }
        // Deferred BIOS initialization owns the kernel store while it runs.
        // Poll its future only once per scheduler turn so an async fuel yield
        // returns control to the controller and resumes on a later tick.
        let pending = self
            .maintenance
            .lock()
            .expect("kernel maintenance lock poisoned")
            .take();
        if let Some(mut future) = pending {
            match poll_once(future.as_mut()).await {
                Poll::Pending => {
                    *self
                        .maintenance
                        .lock()
                        .expect("kernel maintenance lock poisoned") = Some(future);
                }
                Poll::Ready((service, result)) => {
                    match result {
                        Ok(()) => {
                            computer.set_exit_notifications(service.exit_hook.is_some());
                            *self.service.lock().await = Some(service);
                        }
                        Err(message) if message == ProcessExit.to_string() => {
                            *self.service.lock().await = Some(service);
                        }
                        Err(message) if service.lazy_init_pending => {
                            // The service owns a failed kernel_init and cannot
                            // be reused. Dropping it releases its Wasm store.
                            computer.set_exit_notifications(false);
                            self.record_boot_failure(message);
                        }
                        Err(message) => {
                            computer.set_exit_notifications(false);
                            *self.service.lock().await = Some(service);
                            self.mark_panicked(message);
                        }
                    }
                }
            }
            return;
        }

        let mut exits = computer.take_exits();
        if self.is_dead() {
            return;
        }
        let Ok(mut guard) = self.service.try_lock() else {
            computer.restore_exits(exits);
            return;
        };
        let Some(mut service) = guard.take() else {
            return;
        };
        if service.lazy_init_pending {
            let future = async move {
                let result = service.initialize_lazy_runtime().await;
                (service, result)
            };
            *self
                .maintenance
                .lock()
                .expect("kernel maintenance lock poisoned") = Some(Box::pin(future));
            computer.restore_exits(exits);
            return;
        }

        if exits.is_empty() {
            *guard = Some(service);
            return;
        }

        let Some(hook) = service.exit_hook.clone() else {
            *guard = Some(service);
            return;
        };
        let (pid, reason) = exits.remove(0);
        computer.restore_exits(exits);
        service.bind_caller(&CallerBinding {
            sync: SyncGate::new(),
            waiting_for_sync: Arc::new(AtomicBool::new(false)),
            waiting_for_budget: Arc::new(AtomicBool::new(false)),
            waiting_for_wake: Arc::new(AtomicBool::new(false)),
            in_kernel_call: Arc::new(AtomicBool::new(false)),
        });
        let result = service
            .run_exit_hook(&hook, pid, reason, maintenance_fuel)
            .await;
        match result {
            Ok(()) => *guard = Some(service),
            Err(message) if message == ProcessExit.to_string() => *guard = Some(service),
            Err(message) => {
                computer.set_exit_notifications(false);
                self.mark_panicked(message);
                // A panicked kernel cannot be reused. Dropping the service
                // releases its store and RAM reservation.
            }
        }
    }
}

impl KernelService {
    async fn initialize_lazy_runtime(&mut self) -> Result<(), String> {
        if !self.lazy_init_pending {
            return Ok(());
        }
        if let Some(init) = &self.init {
            init.call_async(&mut self.store, ())
                .await
                .map_err(|error| error.to_string())?;
        }
        let descriptor_ptr = self
            .io_descriptor
            .call_async(&mut self.store, ())
            .await
            .map_err(|error| error.to_string())?;
        self.io = read_io_descriptor(&self.memory, &self.store, descriptor_ptr)
            .ok_or_else(|| "kernel I/O descriptor is outside kernel memory".to_owned())?;
        self.prepared_input_capacity = self.io.in_cap.min(MAX_KERNEL_INPUT_CAPACITY);
        self.prepared_output_capacity = self.io.out_cap.min(MAX_KERNEL_OUTPUT_CAPACITY);
        self.lazy_init_pending = false;
        Ok(())
    }

    fn bind_caller(&mut self, binding: &CallerBinding) {
        let context = self.store.data_mut();
        context.sync = binding.sync.clone();
        context.waiting_for_sync = Arc::clone(&binding.waiting_for_sync);
        context.waiting_for_budget = Arc::clone(&binding.waiting_for_budget);
        context.waiting_for_wake = Arc::clone(&binding.waiting_for_wake);
    }

    async fn run(
        &mut self,
        pid: ProcessId,
        privileged: bool,
        number: i32,
        request: &[u8],
        out_cap: usize,
    ) -> Result<KernelCall, String> {
        self.store
            .set_fuel(KERNEL_FUEL)
            .map_err(|error| error.to_string())?;
        self.store
            .fuel_async_yield_interval(Some(self.quantum_fuel))
            .map_err(|error| error.to_string())?;
        let in_len = i32::try_from(request.len()).map_err(|error| error.to_string())?;
        if let Some(prepare) = &self.io_prepare {
            let requested_input = self.prepared_input_capacity.max(request.len());
            let requested_output = self
                .prepared_output_capacity
                .max(out_cap.min(MAX_KERNEL_OUTPUT_CAPACITY));
            if requested_input > self.prepared_input_capacity
                || requested_output > self.prepared_output_capacity
            {
                let input = i32::try_from(requested_input).map_err(|error| error.to_string())?;
                let output = i32::try_from(requested_output).map_err(|error| error.to_string())?;
                let preparation = prepare
                    .call_async(&mut self.store, (input, output))
                    .await
                    .map_err(|error| error.to_string())?;
                if preparation < 0 {
                    return Ok(KernelCall {
                        outcome: Ok(KernelOutcome::Done {
                            result: preparation,
                            response: Vec::new(),
                        }),
                    });
                }
                if preparation != 0 {
                    return Err(format!(
                        "invalid kernel I/O preparation status {preparation}"
                    ));
                }

                let previous_input = self.io.in_cap;
                let previous_output = self.io.out_cap;
                let descriptor_ptr = self
                    .io_descriptor
                    .call_async(&mut self.store, ())
                    .await
                    .map_err(|error| error.to_string())?;
                self.io = read_io_descriptor(&self.memory, &self.store, descriptor_ptr)
                    .ok_or_else(|| "kernel I/O descriptor is outside kernel memory".to_owned())?;
                if self.io.in_cap < previous_input.max(requested_input)
                    || self.io.out_cap < previous_output.max(requested_output)
                {
                    return Err(
                        "kernel I/O buffers shrank or did not reach requested capacities".into(),
                    );
                }
                self.prepared_input_capacity = requested_input;
                self.prepared_output_capacity = requested_output;
            }
        }

        if request.len() > self.io.in_cap {
            return Ok(KernelCall {
                outcome: Ok(KernelOutcome::Done {
                    result: device_status::ARGUMENT_OVERRUN,
                    response: Vec::new(),
                }),
            });
        }
        self.memory
            .write(&mut self.store, self.io.in_ptr, request)
            .map_err(|error| format!("kernel input buffer is invalid: {error}"))?;
        let flags = if privileged { CALLER_PRIVILEGED } else { 0 };
        let out_len = out_cap.min(self.io.out_cap) as i32;
        let code = match self
            .syscall
            .call_async(
                &mut self.store,
                (
                    i64::try_from(pid).unwrap_or(i64::MAX),
                    flags,
                    number,
                    in_len,
                    out_len,
                ),
            )
            .await
        {
            Ok(code) => code,
            Err(error) if error.downcast_ref::<ProcessExit>().is_some() => {
                return Ok(KernelCall {
                    outcome: Ok(KernelOutcome::Terminate),
                });
            }
            Err(error) => return Err(error.to_string()),
        };
        let outcome = match code {
            SYSCALL_BLOCK => Ok(KernelOutcome::Block),
            SYSCALL_TERMINATE_CALLER => Ok(KernelOutcome::Terminate),
            code if code < 0 => Ok(KernelOutcome::Done {
                result: code,
                response: Vec::new(),
            }),
            code => {
                let length = code as usize;
                if length > out_cap {
                    Err(device_status::RESPONSE_OVERRUN)
                } else {
                    let mut response = vec![0; length];
                    self.memory
                        .read(&self.store, self.io.out_ptr, &mut response)
                        .map_err(|error| format!("kernel output buffer is invalid: {error}"))?;
                    Ok(KernelOutcome::Done {
                        result: code,
                        response,
                    })
                }
            }
        };
        Ok(KernelCall { outcome })
    }

    async fn run_exit_hook(
        &mut self,
        hook: &ExitHook,
        pid: ProcessId,
        reason: ExitReason,
        fuel_limit: u64,
    ) -> Result<(), String> {
        self.store
            .set_fuel(fuel_limit)
            .map_err(|error| error.to_string())?;
        self.store
            .fuel_async_yield_interval(None)
            .map_err(|error| error.to_string())?;
        let result = {
            let call = hook.call_async(
                &mut self.store,
                (i64::try_from(pid).unwrap_or(i64::MAX), reason as i32),
            );
            tokio::pin!(call);
            match poll_once(call.as_mut()).await {
                Poll::Ready(Ok(())) => Ok(()),
                Poll::Ready(Err(error)) => Err(error.to_string()),
                Poll::Pending => Err("kernel_process_exited blocked".to_owned()),
            }
        };
        self.store
            .fuel_async_yield_interval(Some(self.quantum_fuel))
            .map_err(|error| error.to_string())?;
        result
    }
}

fn kernel_context(computer: Arc<ComputerShared>) -> ProcessCtx {
    let memory = computer.new_memory_limiter();
    ProcessCtx {
        computer,
        kernel: None,
        pid: KERNEL_PID,
        privileged: true,
        yield_requested: Arc::new(AtomicBool::new(false)),
        memory,
        sync: SyncGate::new(),
        waiting_for_sync: Arc::new(AtomicBool::new(false)),
        waiting_for_budget: Arc::new(AtomicBool::new(false)),
        waiting_for_wake: Arc::new(AtomicBool::new(false)),
        wake: Arc::new(WakeToken::default()),
        in_kernel_call: Arc::new(AtomicBool::new(false)),
    }
}

/// The descriptor is four little-endian `u32`s: input pointer, input
/// capacity, output pointer, output capacity.
fn read_io_descriptor(
    memory: &Memory,
    store: &Store<ProcessCtx>,
    pointer: i32,
) -> Option<KernelIo> {
    let pointer = usize::try_from(pointer).ok()?;
    let mut bytes = [0; IO_DESCRIPTOR_SIZE];
    memory.read(store, pointer, &mut bytes).ok()?;
    let field = |index: usize| {
        let word: [u8; 4] = bytes[index * 4..index * 4 + 4].try_into().ok()?;
        usize::try_from(u32::from_le_bytes(word)).ok()
    };
    let io = KernelIo {
        in_ptr: field(0)?,
        in_cap: field(1)?,
        out_ptr: field(2)?,
        out_cap: field(3)?,
    };
    let memory_size = memory.data_size(store);
    let fits = |pointer: usize, capacity: usize| {
        pointer
            .checked_add(capacity)
            .is_some_and(|end| end <= memory_size)
    };
    (fits(io.in_ptr, io.in_cap) && fits(io.out_ptr, io.out_cap)).then_some(io)
}

#[cfg(test)]
mod tests {
    use super::KernelSlot;

    #[test]
    fn kernel_panic_does_not_block_privileged_process_admission() {
        let slot = KernelSlot::default();
        slot.mark_panicked("test panic".to_owned());

        assert!(slot.processes_ready());
    }
}

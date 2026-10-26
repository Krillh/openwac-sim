use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;

use log::debug;
use uuid::Uuid;
use wasmtime::{Caller, Extern, Linker};

use crate::device::device_status::{self, MEMORY_INVALID};
use crate::guest::{ProcessCtx, ProcessExit};
use crate::hal::{ComputerShared, ProcessRequest};

const MAX_HAL_BUFFER_SIZE: usize = 16 * 1024 * 1024;
const MAX_WASM_IMAGE_SIZE: usize = 1024 * 1024;
const MAX_DEVICE_CALL_WORK_BYTES: usize = 256 * 1024;
const DEVICE_CALL_BASE_COST: u64 = 8;
const DEVICE_CALL_COST_PER_256_BYTES: u64 = 1;
const PROCESS_SPAWN_BASE_COST: u64 = 8;
const PROCESS_SPAWN_COST_PER_16_KIB: u64 = 1;
const KERNEL_INSTALL_BASE_COST: u64 = 8;
const KERNEL_INSTALL_COST_PER_16_KIB: u64 = 1;
const WAKE_COST: u64 = 1;

/// HAL functions that unprivileged processes may import. Every other HAL
/// function is privileged. A function belongs here only if it cannot affect
/// other processes or devices and its cost is bounded.
const USER_HAL_FUNCTIONS: &[&str] = &["hal_yield", "hal_restart"];

fn hal_function_allowed(name: &str, privileged: bool) -> bool {
    privileged || USER_HAL_FUNCTIONS.contains(&name)
}

/// Registers a `hal` function unless the process may not import it.
macro_rules! register {
    ($linker:expr, $privileged:expr, $name:literal, $function:expr $(,)?) => {
        if hal_function_allowed($name, $privileged) {
            $linker.func_wrap_async("hal", $name, $function).map(|_| ())
        } else {
            Ok(())
        }
    };
}

pub(crate) fn define_hal_functions(
    linker: &mut Linker<ProcessCtx>,
    privileged: bool,
) -> Result<(), wasmtime::Error> {
    define_hal_functions_for_store(linker, privileged, false)
}

/// HAL imports for the kernel's private store. It shares the privileged HAL,
/// but cannot recursively install another kernel.
pub(crate) fn define_kernel_hal_functions(
    linker: &mut Linker<ProcessCtx>,
) -> Result<(), wasmtime::Error> {
    define_hal_functions_for_store(linker, true, true)
}

fn define_hal_functions_for_store(
    linker: &mut Linker<ProcessCtx>,
    privileged: bool,
    kernel_store: bool,
) -> Result<(), wasmtime::Error> {
    register!(
        linker,
        privileged,
        "increment",
        |caller: Caller<'_, ProcessCtx>, (): ()| {
            let computer = Arc::clone(&caller.data().computer);
            let waiting = Arc::clone(&caller.data().waiting_for_budget);
            Box::new(async move {
                let _ = charge_syscall(computer.clone(), waiting, 1).await;
                computer.devices.record_operation();
            })
        },
    )?;
    #[cfg(debug_assertions)]
    register!(
        linker,
        privileged,
        "hal_DEBUG_PRINT",
        |mut caller: Caller<'_, ProcessCtx>, (buffer_ptr, buffer_size): (i32, i32)| {
            Box::new(async move {
                use std::io::Write;

                let (Some(buffer_ptr), Some(buffer_size)) =
                    (nonnegative(buffer_ptr), nonnegative(buffer_size))
                else {
                    return -1;
                };
                if buffer_size > MAX_DEVICE_CALL_WORK_BYTES {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                let Some(memory) = guest_memory(&mut caller) else {
                    return -2;
                };
                if !valid_memory_range(&memory, &caller, buffer_ptr, buffer_size) {
                    return -2;
                }
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                let cost = 1 + u64::try_from(buffer_size.div_ceil(256)).unwrap_or(u64::MAX);
                if !charge_syscall(computer, waiting, cost).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                let mut buffer = vec![0; buffer_size];
                if memory.read(&caller, buffer_ptr, &mut buffer).is_err() {
                    return -2;
                }

                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                if stdout
                    .write_all(&buffer)
                    .and_then(|()| stdout.write_all(b"\n"))
                    .is_ok()
                {
                    0
                } else {
                    -3
                }
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_yield",
        |caller: Caller<'_, ProcessCtx>, (): ()| {
            let yielding = Arc::clone(&caller.data().yield_requested);
            Box::new(async move {
                yielding.store(true, Ordering::Release);
                let mut yielded = false;
                std::future::poll_fn(move |_| {
                    if !yielded {
                        yielded = true;
                        Poll::Pending
                    } else {
                        yielding.store(false, Ordering::Release);
                        Poll::Ready(())
                    }
                })
                .await;
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_restart",
        |caller: Caller<'_, ProcessCtx>, (): ()| {
            let computer = Arc::clone(&caller.data().computer);
            let waiting = Arc::clone(&caller.data().waiting_for_budget);
            Box::new(async move {
                if !charge_syscall(computer.clone(), waiting, 1).await {
                    return Ok(-1);
                }
                if !caller.data().privileged {
                    return Ok(-1);
                }
                computer.request_restart();
                Err(wasmtime::Error::new(ProcessExit))
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_cpu_time_ns",
        |caller: Caller<'_, ProcessCtx>, (): ()| {
            Box::new(async move {
                if !charge_syscall(
                    Arc::clone(&caller.data().computer),
                    Arc::clone(&caller.data().waiting_for_budget),
                    1,
                )
                .await
                {
                    return 0_i64;
                }
                i64::try_from(caller.data().computer.cpu_time_nanos()).unwrap_or(i64::MAX)
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_ram_used_pages",
        |caller: Caller<'_, ProcessCtx>, (): ()| {
            Box::new(async move {
                if !charge_syscall(
                    Arc::clone(&caller.data().computer),
                    Arc::clone(&caller.data().waiting_for_budget),
                    1,
                )
                .await
                {
                    return 0_i64;
                }
                let pages = caller
                    .data()
                    .computer
                    .memory_used_bytes()
                    .div_ceil(64 * 1024);
                i64::try_from(pages).unwrap_or(i64::MAX)
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_chat_print",
        |mut caller: Caller<'_, ProcessCtx>, (buffer_ptr, buffer_size): (i32, i32)| {
            Box::new(async move {
                let Some(buffer_size) = nonnegative(buffer_size) else {
                    return device_status::INVALID_ARGUMENT;
                };
                if buffer_size > 2048 {
                    return device_status::INVALID_ARGUMENT;
                }
                let Some(buffer_ptr) = nonnegative(buffer_ptr) else {
                    return device_status::POINTER_INVALID;
                };
                let Some(memory) = guest_memory(&mut caller) else {
                    return device_status::MEMORY_INVALID;
                };
                if !valid_memory_range(&memory, &caller, buffer_ptr, buffer_size) {
                    return device_status::POINTER_INVALID;
                }
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                let cost = 1 + u64::try_from(buffer_size.div_ceil(256)).unwrap_or(u64::MAX);
                if !charge_syscall(computer.clone(), waiting, cost).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                let mut bytes = vec![0; buffer_size];
                if memory.read(&caller, buffer_ptr, &mut bytes).is_err() {
                    return device_status::POINTER_INVALID;
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if computer.record_chat(text) { 0 } else { -11 }
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_device_count",
        |caller: Caller<'_, ProcessCtx>, (): ()| {
            let computer = Arc::clone(&caller.data().computer);
            let waiting = Arc::clone(&caller.data().waiting_for_budget);
            Box::new(async move {
                if !charge_syscall(computer.clone(), waiting, 1).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                debug!("hal_device_count");
                i32::try_from(computer.devices.count()).unwrap_or(i32::MAX)
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_device_info",
        |mut caller: Caller<'_, ProcessCtx>,
         (index, output_ptr, output_capacity): (i32, i32, i32)| {
            Box::new(async move {
                let Some(index) = nonnegative(index) else {
                    return device_status::INVALID_ARGUMENT;
                };
                let Some(output_ptr) = nonnegative(output_ptr) else {
                    return device_status::INVALID_ARGUMENT;
                };
                let Some((interface_id, uuid)) = caller.data().computer.devices.device_info(index)
                else {
                    return device_status::INVALID_ARGUMENT;
                };
                if output_capacity < 20 {
                    return device_status::RESPONSE_OVERRUN;
                }
                let Some(memory) = guest_memory(&mut caller) else {
                    return device_status::MEMORY_INVALID;
                };
                if !valid_memory_range(&memory, &caller, output_ptr, 20) {
                    return MEMORY_INVALID;
                }
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                if !charge_syscall(computer, waiting, 1).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                debug!("hal_device_info #{index}");
                let mut info = [0; 20];
                info[..4].copy_from_slice(&interface_id.to_le_bytes());
                info[4..].copy_from_slice(uuid.as_bytes());
                match memory.write(&mut caller, output_ptr, &info) {
                    Ok(()) => 0,
                    Err(_) => MEMORY_INVALID,
                }
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_device_call",
        |mut caller: Caller<'_, ProcessCtx>,
         (uuid_ptr, operation, arguments_ptr, arguments_len, response_ptr, response_capacity): (
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
        )| {
            Box::new(async move {
                if !caller.data().privileged {
                    return device_status::PERMISSION_DENIED;
                }
                let (Some(arguments_len), Some(response_len)) =
                    (nonnegative(arguments_len), nonnegative(response_capacity))
                else {
                    return device_status::INVALID_ARGUMENT;
                };
                let (Some(uuid_ptr), Some(arguments_ptr), Some(response_ptr)) = (
                    nonnegative(uuid_ptr),
                    nonnegative(arguments_ptr),
                    nonnegative(response_ptr),
                ) else {
                    return device_status::POINTER_INVALID;
                };
                let operation = operation as u32;
                let work_bytes = match arguments_len.checked_add(response_len) {
                    Some(bytes) if bytes <= MAX_DEVICE_CALL_WORK_BYTES => bytes,
                    _ => return device_status::SYSCALL_TOO_EXPENSIVE,
                };
                if arguments_len > MAX_HAL_BUFFER_SIZE || response_len > MAX_HAL_BUFFER_SIZE {
                    return device_status::INVALID_ARGUMENT;
                }
                let cost = DEVICE_CALL_BASE_COST
                    + u64::try_from(work_bytes.div_ceil(256)).unwrap_or(u64::MAX)
                        * DEVICE_CALL_COST_PER_256_BYTES;
                let Some(memory) = guest_memory(&mut caller) else {
                    return device_status::MEMORY_INVALID;
                };
                if !valid_memory_range(&memory, &caller, uuid_ptr, 16)
                    || !valid_memory_range(&memory, &caller, arguments_ptr, arguments_len)
                    || !valid_memory_range(&memory, &caller, response_ptr, response_len)
                {
                    return device_status::POINTER_INVALID;
                }
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                if !charge_syscall(computer.clone(), waiting, cost).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                let mut uuid_bytes = [0; 16];
                if memory.read(&caller, uuid_ptr, &mut uuid_bytes).is_err() {
                    return device_status::POINTER_INVALID;
                }
                let mut arguments = vec![0; arguments_len];
                if memory.read(&caller, arguments_ptr, &mut arguments).is_err() {
                    return device_status::POINTER_INVALID;
                }
                debug!("hal_device_call");
                debug!("uuid: {}", Uuid::from_bytes(uuid_bytes).as_hyphenated());
                debug!("operation: 0x{operation:08X}");
                let mut response = vec![0; response_len];
                let result = match computer.devices.operate(
                    uuid::Uuid::from_bytes(uuid_bytes),
                    operation,
                    &arguments,
                    &mut response,
                ) {
                    Ok(result) => result,
                    Err(()) => {
                        debug!("device operation failed: DEVICE_NOT_AVAILABLE");
                        return device_status::DEVICE_NOT_AVAILABLE;
                    }
                };
                debug!("device operation result: {result}");
                if result < 0 {
                    return result;
                }
                let bytes_written = result as usize;
                if bytes_written > response.len() {
                    return device_status::RESPONSE_OVERRUN;
                }
                if memory
                    .write(&mut caller, response_ptr, &response[..bytes_written])
                    .is_err()
                {
                    return device_status::MEMORY_INVALID;
                }
                // Device implementations return their copied byte count to
                // this host adapter; the public HAL ABI reports success as 0.
                0
            })
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_poll_event",
        |mut caller: Caller<'_, ProcessCtx>, (output_ptr, output_capacity): (i32, i32)| {
            Box::new(async move {
                let (Some(output_ptr), Some(output_capacity)) =
                    (nonnegative(output_ptr), nonnegative(output_capacity))
                else {
                    return -2;
                };
                if output_capacity > MAX_HAL_BUFFER_SIZE
                    || output_capacity > MAX_DEVICE_CALL_WORK_BYTES
                {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                let Some(memory) = guest_memory(&mut caller) else {
                    return -3;
                };
                if !valid_memory_range(&memory, &caller, output_ptr, output_capacity) {
                    return -3;
                }
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                let cost = 1 + u64::try_from(output_capacity.div_ceil(256)).unwrap_or(u64::MAX);
                if !charge_syscall(computer.clone(), waiting, cost).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                let event = match computer
                    .mailbox
                    .receive_with_capacity(caller.data().pid, output_capacity)
                {
                    Ok(Some(event)) => event,
                    Ok(None) => return -1,
                    Err(()) => return -2,
                };
                if memory.write(&mut caller, output_ptr, &event).is_err() {
                    return -3;
                }
                i32::try_from(event.len()).unwrap_or(i32::MAX)
            })
        },
    )?;
    register!(linker, privileged, "hal_spawn_process", |caller: Caller<
        '_,
        ProcessCtx,
    >,
                                                        (
        wasm_ptr,
        wasm_len,
    ): (
        i32,
        i32
    )| {
        Box::new(spawn_process(caller, wasm_ptr, wasm_len, false))
    },)?;
    register!(
        linker,
        privileged,
        "hal_spawn_process_privileged",
        |caller: Caller<'_, ProcessCtx>, (wasm_ptr, wasm_len): (i32, i32)| {
            Box::new(spawn_process(caller, wasm_ptr, wasm_len, true))
        },
    )?;
    register!(
        linker,
        privileged,
        "hal_kill_process",
        |caller: Caller<'_, ProcessCtx>, (pid,): (i64,)| {
            Box::new(async move {
                if !caller.data().privileged {
                    return device_status::PERMISSION_DENIED;
                }
                if pid <= 0 {
                    return -1;
                }
                let pid = pid as u64;
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                if !charge_syscall(computer.clone(), waiting, 2).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                match computer.process_control.enqueue(ProcessRequest::Kill(pid)) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            })
        },
    )?;
    if !kernel_store {
        register!(
            linker,
            privileged,
            "hal_install_kernel",
            |caller: Caller<'_, ProcessCtx>, (wasm_ptr, wasm_len): (i32, i32)| {
                Box::new(install_kernel(caller, wasm_ptr, wasm_len))
            },
        )?;
    }
    register!(
        linker,
        privileged,
        "hal_wake",
        |caller: Caller<'_, ProcessCtx>, (pid,): (i64,)| {
            Box::new(async move {
                if !caller.data().privileged {
                    return device_status::PERMISSION_DENIED;
                }
                let computer = Arc::clone(&caller.data().computer);
                let waiting = Arc::clone(&caller.data().waiting_for_budget);
                if !charge_syscall(Arc::clone(&computer), waiting, WAKE_COST).await {
                    return device_status::SYSCALL_TOO_EXPENSIVE;
                }
                if pid > 0 && computer.wake_process(pid as u64) {
                    0
                } else {
                    device_status::INVALID_ARGUMENT
                }
            })
        },
    )?;
    Ok(())
}

async fn install_kernel(
    mut caller: Caller<'_, ProcessCtx>,
    wasm_ptr: i32,
    wasm_len: i32,
) -> Result<i32, wasmtime::Error> {
    if !caller.data().privileged {
        return Ok(device_status::PERMISSION_DENIED);
    }
    // The kernel's own store has no slot, so it cannot reinstall itself.
    let Some(kernel) = caller.data().kernel.clone() else {
        return Ok(device_status::PERMISSION_DENIED);
    };
    let Some(wasm_len) = nonnegative(wasm_len) else {
        return Ok(device_status::INVALID_ARGUMENT);
    };
    if wasm_len < 8 || wasm_len > MAX_WASM_IMAGE_SIZE {
        return Ok(device_status::INVALID_ARGUMENT);
    }
    let Some(wasm_ptr) = nonnegative(wasm_ptr) else {
        return Ok(device_status::POINTER_INVALID);
    };
    let Some(memory) = guest_memory(&mut caller) else {
        return Ok(device_status::MEMORY_INVALID);
    };
    if !valid_memory_range(&memory, &caller, wasm_ptr, wasm_len) {
        return Ok(device_status::POINTER_INVALID);
    }
    let cost = KERNEL_INSTALL_BASE_COST
        + u64::try_from(wasm_len.div_ceil(16 * 1024)).unwrap_or(u64::MAX)
            * KERNEL_INSTALL_COST_PER_16_KIB;
    let computer = Arc::clone(&caller.data().computer);
    let waiting = Arc::clone(&caller.data().waiting_for_budget);
    if !charge_syscall(Arc::clone(&computer), Arc::clone(&waiting), cost).await {
        return Ok(device_status::SYSCALL_TOO_EXPENSIVE);
    }
    let mut wasm = vec![0; wasm_len];
    if memory.read(&caller, wasm_ptr, &mut wasm).is_err() {
        return Ok(device_status::POINTER_INVALID);
    }
    let engine = caller.engine().clone();
    let defer_init = caller.data().pid == 1;
    let job = match kernel.begin_install(&engine, wasm, defer_init) {
        Ok(job) => job,
        Err(code) => return Ok(code),
    };
    // Wait a tick while the kernel compiles on a worker thread; compilation
    // overlaps the idle time between ticks instead of lengthening this one.
    let next_tick = caller.data().sync.wait_for_next_tick();
    let waiting_for_sync = Arc::clone(&caller.data().waiting_for_sync);
    waiting_for_sync.store(true, Ordering::Release);
    next_tick.await;
    waiting_for_sync.store(false, Ordering::Release);

    let result = match kernel
        .finish_install(job, &engine, Arc::clone(&computer))
        .await
    {
        Ok(()) => 0,
        Err(code) => code,
    };
    if computer.restart_requested() {
        Err(wasmtime::Error::new(ProcessExit))
    } else {
        Ok(result)
    }
}

async fn spawn_process(
    mut caller: Caller<'_, ProcessCtx>,
    wasm_ptr: i32,
    wasm_len: i32,
    privileged: bool,
) -> i64 {
    if privileged && !caller.data().privileged {
        return i64::from(device_status::PERMISSION_DENIED);
    }
    let (Some(wasm_ptr), Some(wasm_len)) = (nonnegative(wasm_ptr), nonnegative(wasm_len)) else {
        return 0;
    };
    if wasm_len < 8 || wasm_len > MAX_WASM_IMAGE_SIZE {
        return 0;
    }
    let Some(memory) = guest_memory(&mut caller) else {
        return 0;
    };
    if !valid_memory_range(&memory, &caller, wasm_ptr, wasm_len) {
        return 0;
    }
    let cost = PROCESS_SPAWN_BASE_COST
        + u64::try_from(wasm_len.div_ceil(16 * 1024)).unwrap_or(u64::MAX)
            * PROCESS_SPAWN_COST_PER_16_KIB;
    let computer = Arc::clone(&caller.data().computer);
    let waiting = Arc::clone(&caller.data().waiting_for_budget);
    if !charge_syscall(computer.clone(), waiting, cost).await {
        return i64::from(device_status::SYSCALL_TOO_EXPENSIVE);
    }
    let mut wasm = vec![0; wasm_len];
    if memory.read(&caller, wasm_ptr, &mut wasm).is_err() {
        return 0;
    }
    if !computer.reserve_process_slot() {
        return 0;
    }
    let Ok(pid) = computer.allocate_process_id() else {
        computer.release_process_slot();
        return 0;
    };
    computer.mark_pending_spawn(pid);
    if computer
        .process_control
        .enqueue(ProcessRequest::Spawn {
            pid,
            wasm,
            privileged,
        })
        .is_err()
    {
        computer.unmark_pending_spawn(pid);
        computer.release_process_slot();
        return 0;
    }
    pid as i64
}

pub(crate) async fn charge_syscall(
    computer: Arc<ComputerShared>,
    waiting: Arc<std::sync::atomic::AtomicBool>,
    cost: u64,
) -> bool {
    waiting.store(true, Ordering::Release);
    let charged = match computer.cpu_budget.charge_syscall(cost).await {
        Ok(waited_for_tick) => {
            waiting.store(false, Ordering::Release);
            if waited_for_tick {
                // Hand control back to the scheduler before the host call
                // resumes guest Wasm after consuming a later tick's budget.
                tokio::task::yield_now().await;
            }
            true
        }
        Err(()) => false,
    };
    waiting.store(false, Ordering::Release);
    charged
}

pub(crate) fn nonnegative(value: i32) -> Option<usize> {
    usize::try_from(value).ok()
}

pub(crate) fn guest_memory(caller: &mut Caller<'_, ProcessCtx>) -> Option<wasmtime::Memory> {
    caller.get_export("memory").and_then(Extern::into_memory)
}

pub(crate) fn valid_memory_range(
    memory: &wasmtime::Memory,
    caller: &Caller<'_, ProcessCtx>,
    pointer: usize,
    length: usize,
) -> bool {
    pointer
        .checked_add(length)
        .is_some_and(|end| end <= memory.data_size(caller))
}

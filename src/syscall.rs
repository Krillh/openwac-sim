//! The `sys` import module: the only door from user processes into the kernel.

use std::sync::Arc;

use wasmtime::{Caller, Linker};

use crate::device::device_status;
use crate::guest::{ProcessCtx, ProcessExit};
use crate::hal_functions::{charge_syscall, guest_memory, nonnegative, valid_memory_range};
use crate::kernel::{CallerBinding, KernelOutcome};

/// Combined request and response size allowed in one syscall.
const MAX_SYSCALL_WORK_BYTES: usize = 256 * 1024;
const SYSCALL_BASE_COST: u64 = 8;
const SYSCALL_COST_PER_256_BYTES: u64 = 1;

pub(crate) fn define_sys_functions(linker: &mut Linker<ProcessCtx>) -> Result<(), wasmtime::Error> {
    linker.func_wrap_async(
        "sys",
        "syscall",
        |mut caller: Caller<'_, ProcessCtx>,
         (number, in_ptr, in_len, out_ptr, out_cap): (i32, i32, i32, i32, i32)| {
            Box::new(
                async move { syscall(&mut caller, number, in_ptr, in_len, out_ptr, out_cap).await },
            )
        },
    )?;
    Ok(())
}

async fn syscall(
    caller: &mut Caller<'_, ProcessCtx>,
    number: i32,
    in_ptr: i32,
    in_len: i32,
    out_ptr: i32,
    out_cap: i32,
) -> Result<i32, wasmtime::Error> {
    let (Some(in_ptr), Some(in_len), Some(out_ptr), Some(out_cap)) = (
        nonnegative(in_ptr),
        nonnegative(in_len),
        nonnegative(out_ptr),
        nonnegative(out_cap),
    ) else {
        return Ok(device_status::INVALID_ARGUMENT);
    };
    let Some(kernel) = caller.data().kernel.clone() else {
        return Ok(device_status::PERMISSION_DENIED);
    };
    let work_bytes = match in_len.checked_add(out_cap) {
        Some(bytes) if bytes <= MAX_SYSCALL_WORK_BYTES => bytes,
        _ => return Ok(device_status::SYSCALL_TOO_EXPENSIVE),
    };
    let Some(memory) = guest_memory(caller) else {
        return Ok(device_status::MEMORY_INVALID);
    };
    if !valid_memory_range(&memory, caller, in_ptr, in_len)
        || !valid_memory_range(&memory, caller, out_ptr, out_cap)
    {
        return Ok(device_status::POINTER_INVALID);
    }

    let computer = Arc::clone(&caller.data().computer);
    let binding = CallerBinding {
        sync: caller.data().sync.clone(),
        waiting_for_sync: Arc::clone(&caller.data().waiting_for_sync),
        waiting_for_budget: Arc::clone(&caller.data().waiting_for_budget),
        waiting_for_wake: Arc::clone(&caller.data().waiting_for_wake),
        in_kernel_call: Arc::clone(&caller.data().in_kernel_call),
    };
    let wake = Arc::clone(&caller.data().wake);
    let (pid, privileged) = (caller.data().pid, caller.data().privileged);

    let copy_cost = SYSCALL_BASE_COST
        + u64::try_from(work_bytes.div_ceil(256)).unwrap_or(u64::MAX) * SYSCALL_COST_PER_256_BYTES;
    if !charge_syscall(
        Arc::clone(&computer),
        Arc::clone(&binding.waiting_for_budget),
        copy_cost,
    )
    .await
    {
        return Ok(device_status::SYSCALL_TOO_EXPENSIVE);
    }

    let mut request = vec![0; in_len];
    if memory.read(&*caller, in_ptr, &mut request).is_err() {
        return Ok(device_status::POINTER_INVALID);
    }

    // A blocked call is re-run from the saved request once woken. The caller's
    // memory cannot change while it is parked inside this host call.
    loop {
        let call = kernel
            .call(&binding, pid, privileged, number, &request, out_cap)
            .await;
        match call.outcome {
            Err(code) => return Ok(code),
            Ok(KernelOutcome::Done { result, response }) => {
                if !response.is_empty() && memory.write(&mut *caller, out_ptr, &response).is_err() {
                    return Ok(device_status::MEMORY_INVALID);
                }
                return Ok(result);
            }
            Ok(KernelOutcome::Terminate) => return Err(wasmtime::Error::new(ProcessExit)),
            Ok(KernelOutcome::Block) => {
                binding
                    .waiting_for_wake
                    .store(true, std::sync::atomic::Ordering::Release);
                wake.wait().await;
                binding
                    .waiting_for_wake
                    .store(false, std::sync::atomic::Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use crate::controller::Controller;
    use crate::device::{ComputerSpec, CpuConfig};
    use crate::simulation::{SimulationConfig, SimulationHandle};

    const KERNEL_DISPATCH: &str = r#"
(func (export "kernel_syscall")
    (param $pid i64) (param $flags i32) (param $number i32) (param $in_len i32) (param $out_cap i32)
    (result i32)
  (local $i i32)
  ;; 1: echo the request
  (if (i32.eq (local.get $number) (i32.const 1)) (then
    (memory.copy (i32.const 512) (i32.const 256) (local.get $in_len))
    (return (local.get $in_len))))
  ;; 2: block until the flag is set, then return it
  (if (i32.eq (local.get $number) (i32.const 2)) (then
    (if (i32.eqz (i32.load (i32.const 1024))) (then (return (i32.const -100))))
    (i32.store (i32.const 512) (i32.load (i32.const 1024)))
    (return (i32.const 4))))
  ;; 3: set the flag and wake the process whose ID is in the request
  (if (i32.eq (local.get $number) (i32.const 3)) (then
    (i32.store (i32.const 1024) (i32.const 7))
    (drop (call $wake (i64.load (i32.const 256))))
    (return (i32.const 0))))
  ;; 4: terminate the caller
  (if (i32.eq (local.get $number) (i32.const 4)) (then (return (i32.const -101))))
  ;; 5: trap
  (if (i32.eq (local.get $number) (i32.const 5)) (then unreachable))
  ;; 6: run 100 hardware operations
  (if (i32.eq (local.get $number) (i32.const 6)) (then
    (block $done (loop $again
      (br_if $done (i32.ge_u (local.get $i) (i32.const 100)))
      call $increment
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      br $again))
    (return (i32.const 0))))
  ;; 7: report the privilege flags
  (if (i32.eq (local.get $number) (i32.const 7)) (then
    (i32.store (i32.const 512) (local.get $flags))
    (return (i32.const 4))))
  ;; 8: report the caller's process ID
  (if (i32.eq (local.get $number) (i32.const 8)) (then
    (i32.store (i32.const 512) (i32.wrap_i64 (local.get $pid)))
    (return (i32.const 4))))
  ;; 9: spin forever; it must still yield at the kernel's CPU quantum.
  (if (i32.eq (local.get $number) (i32.const 9)) (then
    (loop $forever (br $forever))))
  (i32.const -2))
"#;

    /// Counts a hardware operation whenever a process ends normally.
    const EXIT_HOOK: &str = r#"
(func (export "kernel_process_exited") (param i64 i32)
  (if (i32.eqz (local.get 1)) (then call $increment)))
"#;

    fn kernel_wasm(with_exit_hook: bool) -> Vec<u8> {
        let hook = if with_exit_hook { EXIT_HOOK } else { "" };
        wat::parse_str(format!(
            r#"(module
  (import "hal" "hal_wake" (func $wake (param i64) (result i32)))
  (import "hal" "increment" (func $increment))
  (memory (export "memory") 1)
  ;; input buffer at 256 (256 bytes); output buffer at 512 (256 bytes)
  (data (i32.const 0) "\00\01\00\00\00\01\00\00\00\02\00\00\00\01\00\00")
  (func (export "kernel_get_io_buffers") (result i32) i32.const 0)
  {hook}
  {KERNEL_DISPATCH}
)"#
        ))
        .expect("kernel WAT compiles")
    }

    fn preparation_kernel(
        initial_input: usize,
        initial_output: usize,
        preparation_status: i32,
        grow_buffers: bool,
    ) -> Vec<u8> {
        let grow = if grow_buffers {
            "i32.const 4 local.get $input i32.store\n\
             i32.const 12 local.get $output i32.store\n"
        } else {
            ""
        };
        wat::parse_str(format!(
            r#"(module
  (import "hal" "increment" (func $increment))
  (memory (export "memory") 1)
  (data (i32.const 0) "\40\00\00\00\{input:02x}\00\00\00\80\00\00\00\{output:02x}\00\00\00")
  (func (export "kernel_get_io_buffers") (result i32) i32.const 0)
  (func (export "kernel_prepare_io_buffers")
    (param $input i32) (param $output i32) (result i32)
    call $increment
    {grow}
    i32.const {status})
  (func (export "kernel_syscall") (param i64 i32 i32 i32 i32) (result i32)
    i32.const 0))"#,
            input = initial_input,
            output = initial_output,
            status = preparation_status,
            grow = grow,
        ))
        .expect("kernel preparation WAT compiles")
    }

    fn user_wasm(body: &str, data: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
  (import "hal" "hal_yield" (func $yield))
  (import "sys" "syscall" (func $syscall (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  {data}
  (func (export "_start") {body})
)"#
        ))
        .expect("user WAT compiles")
    }

    fn escape(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
    }

    /// A budget large enough that fuel never delays a process, for tests that
    /// assert on exact ticks.
    fn roomy_cpu() -> CpuConfig {
        CpuConfig {
            fuel_per_tick: 1_000_000,
            quantum_fuel: 256,
            ..CpuConfig::default()
        }
    }

    /// Builds a privileged boot program whose memory holds each blob at a
    /// known offset; `body` receives the (offset, length) of every blob.
    fn boot_module(blobs: &[&[u8]], body: impl Fn(&[(usize, usize)]) -> String) -> String {
        let mut data = String::new();
        let mut spans = Vec::new();
        for (index, blob) in blobs.iter().enumerate() {
            let offset = 2048 + index * 8192;
            assert!(blob.len() < 8192, "test blob is too large");
            data.push_str(&format!(
                "(data (i32.const {offset}) \"{}\")\n",
                escape(blob)
            ));
            spans.push((offset, blob.len()));
        }
        format!(
            r#"(module
  (import "hal" "hal_install_kernel" (func $install (param i32 i32) (result i32)))
  (import "hal" "hal_spawn_process" (func $spawn (param i32 i32) (result i64)))
  (import "hal" "hal_spawn_process_privileged"
    (func $spawn_privileged (param i32 i32) (result i64)))
  (import "hal" "hal_kill_process" (func $kill (param i64) (result i32)))
  (import "hal" "hal_yield" (func $yield))
  (import "sys" "syscall" (func $syscall (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 4)
  {data}
  (func (export "_start") {})
)"#,
            body(&spans)
        )
    }

    fn install(span: (usize, usize), expected: i32) -> String {
        format!(
            "i32.const {} i32.const {} call $install i32.const {expected} i32.ne if unreachable end\n",
            span.0, span.1
        )
    }

    fn spawn(span: (usize, usize)) -> String {
        format!(
            "i32.const {} i32.const {} call $spawn drop\n",
            span.0, span.1
        )
    }

    fn spawn_privileged(span: (usize, usize)) -> String {
        format!(
            "i32.const {} i32.const {} call $spawn_privileged drop\n",
            span.0, span.1
        )
    }

    /// Asserts a syscall's result, or traps.
    fn expect_syscall(
        number: i32,
        in_ptr: i32,
        in_len: i32,
        out_ptr: i32,
        out_cap: i32,
        expected: i32,
    ) -> String {
        format!(
            "i32.const {number} i32.const {in_ptr} i32.const {in_len} i32.const {out_ptr} \
             i32.const {out_cap} call $syscall i32.const {expected} i32.ne if unreachable end\n"
        )
    }

    fn expect_i32_at(address: i32, expected: i32) -> String {
        format!("i32.const {address} i32.load i32.const {expected} i32.ne if unreachable end\n")
    }

    struct Run {
        events: Vec<(u64, String)>,
        /// The tick each entry of `events` was reported in.
        ticks: Vec<u64>,
        hardware_operations: u64,
        finished: bool,
    }

    impl Run {
        fn saw(&self, pid: u64, description: &str) -> bool {
            self.tick_of(pid, description).is_some()
        }

        /// The first tick in which `pid` reported `description`.
        fn tick_of(&self, pid: u64, description: &str) -> Option<u64> {
            self.events
                .iter()
                .zip(&self.ticks)
                .find(|((event_pid, text), _)| *event_pid == pid && text == description)
                .map(|(_, tick)| *tick)
        }

        fn assert_no_traps(&self) {
            let traps: Vec<_> = self
                .events
                .iter()
                .filter(|(_, text)| text.starts_with("trapped:"))
                .collect();
            assert!(
                traps.is_empty(),
                "unexpected traps: {traps:?}; all events: {:?}",
                self.events
            );
        }
    }

    fn run(boot_source: &str, cpu: CpuConfig, max_ticks: usize) -> Run {
        run_with(boot_source, cpu, max_ticks, |_, _| {})
    }

    fn run_with(
        boot_source: &str,
        cpu: CpuConfig,
        max_ticks: usize,
        mut between_ticks: impl FnMut(&SimulationHandle, &[(u64, String)]),
    ) -> Run {
        let bootloader =
            std::env::temp_dir().join(format!("openwac-syscall-test-{}.wat", Uuid::new_v4()));
        std::fs::write(&bootloader, boot_source).expect("test bootloader is written");
        let config = SimulationConfig {
            max_ticks: max_ticks as u64,
            tick_interval_ms: 50,
            runtime_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/simulation-test"),
            computers: vec![ComputerSpec {
                name: None,
                bootloader: bootloader.clone(),
                devices: Vec::new(),
                cpu,
            }],
        };
        let (thread, commands) = Controller::start(config).expect("controller starts");
        let simulation = SimulationHandle::new(commands);

        let mut result = Run {
            events: Vec::new(),
            ticks: Vec::new(),
            hardware_operations: 0,
            finished: false,
        };
        for _ in 0..max_ticks {
            let report = simulation.synchronize().expect("tick completes");
            let tick_events: Vec<_> = report
                .events
                .iter()
                .map(|event| (event.process_id, event.description.clone()))
                .collect();
            result.ticks.extend(tick_events.iter().map(|_| report.tick));
            result.events.extend(tick_events.iter().cloned());
            result.hardware_operations = report
                .hardware_operations
                .iter()
                .map(|(_, operations)| operations)
                .sum();
            between_ticks(&simulation, &tick_events);
            if report.all_finished {
                result.finished = true;
                break;
            }
        }
        simulation.shutdown().expect("controller shuts down");
        thread.join().expect("controller thread joins");
        std::fs::remove_file(bootloader).expect("test bootloader is removed");
        result
    }

    #[test]
    fn syscall_round_trips_buffers_and_identifies_the_caller() {
        let child = user_wasm(
            &[
                expect_syscall(1, 0, 5, 16, 16, 5),
                expect_i32_at(16, i32::from_le_bytes(*b"hell")),
                expect_syscall(7, 0, 0, 32, 4, 4),
                expect_i32_at(32, 0),
                expect_syscall(8, 0, 0, 32, 4, 4),
                expect_i32_at(32, 2),
            ]
            .concat(),
            r#"(data (i32.const 0) "hello")"#,
        );
        let privileged_child = user_wasm(
            &[expect_syscall(7, 0, 0, 32, 4, 4), expect_i32_at(32, 1)].concat(),
            "",
        );
        let kernel = kernel_wasm(false);
        let source = boot_module(&[&kernel, &child, &privileged_child], |spans| {
            [
                install(spans[0], 0),
                spawn(spans[1]),
                spawn_privileged(spans[2]),
            ]
            .concat()
        });

        let run = run(&source, CpuConfig::default(), 16);
        run.assert_no_traps();
        assert!(
            run.saw(2, "exited"),
            "the unprivileged caller should finish; events: {:?}",
            run.events
        );
        assert!(run.saw(3, "exited"), "the privileged caller should finish");
        assert!(run.finished);
    }

    #[test]
    fn syscall_without_an_installed_kernel_is_an_error() {
        let source = boot_module(&[], |_| expect_syscall(1, 0, 0, 0, 0, -13));
        let run = run(&source, CpuConfig::default(), 4);
        run.assert_no_traps();
        assert!(run.saw(1, "exited"));
    }

    #[test]
    fn kernel_install_rejects_invalid_images_and_second_installs() {
        let kernel = kernel_wasm(false);
        let child = user_wasm(&expect_syscall(1, 0, 0, 0, 0, 0), "");
        let source = boot_module(&[b"not wasm", &kernel, &child], |spans| {
            [
                install(spans[0], -16),
                install(spans[1], 0),
                install(spans[1], -15),
                spawn(spans[2]),
            ]
            .concat()
        });
        let run = run(&source, CpuConfig::default(), 16);
        run.assert_no_traps();
        assert!(run.saw(1, "exited"));
    }

    #[test]
    fn blocked_syscall_is_restarted_after_a_wake() {
        let waiter = user_wasm(
            &[expect_syscall(2, 0, 0, 16, 4, 4), expect_i32_at(16, 7)].concat(),
            "",
        );
        let waker = user_wasm(
            &expect_syscall(3, 0, 8, 0, 0, 0),
            r#"(data (i32.const 0) "\02\00\00\00\00\00\00\00")"#,
        );
        let kernel = kernel_wasm(false);
        let source = boot_module(&[&kernel, &waiter, &waker], |spans| {
            [install(spans[0], 0), spawn(spans[1]), spawn(spans[2])].concat()
        });

        let run = run(&source, CpuConfig::default(), 16);
        run.assert_no_traps();
        assert!(run.saw(2, "blocked in syscall"));
        assert!(run.saw(2, "exited"), "the waiter should resume and finish");
        assert!(run.saw(3, "exited"));
        assert!(run.finished);
    }

    #[test]
    fn kernel_io_preparation_matches_growth_and_status_contract() {
        let cases = [
            // The requested capacities fit the descriptor, so prepare is not
            // called and no hardware operation is recorded.
            ("already prepared", 2, 4, 0, false, 0, false, 0),
            // Negative preparation statuses pass through to the caller.
            ("negative status", 1, 1, -27, false, -27, false, 1),
            // Positive statuses and descriptors that fail to grow are traps.
            ("positive status", 1, 1, 1, false, -14, true, 1),
            ("shrunk descriptor", 1, 1, 0, false, -14, true, 1),
            ("successful growth", 1, 1, 0, true, 0, false, 1),
        ];

        for (name, input, output, preparation_status, grow, expected, panics, operations) in cases {
            let kernel = preparation_kernel(input, output, preparation_status, grow);
            let child = user_wasm(&expect_syscall(1, 0, 2, 16, 4, expected), "");
            let source = boot_module(&[&kernel, &child], |spans| {
                [install(spans[0], 0), spawn(spans[1])].concat()
            });
            let run = run(&source, CpuConfig::default(), 16);
            run.assert_no_traps();
            assert!(run.saw(2, "exited"), "{name}: child should return");
            assert_eq!(run.hardware_operations, operations, "{name}: prepare calls");
            assert_eq!(
                run.events
                    .iter()
                    .any(|(pid, text)| *pid == 0 && text.starts_with("kernel panic:")),
                panics,
                "{name}: panic state; events: {:?}",
                run.events
            );
            assert!(run.finished, "{name}: simulation should finish");
        }
    }

    #[test]
    fn terminate_result_ends_the_caller_and_notifies_the_kernel() {
        let child = user_wasm(
            &format!(
                "{} unreachable",
                "i32.const 4 i32.const 0 i32.const 0 i32.const 0 i32.const 0 call $syscall drop"
            ),
            "",
        );
        let kernel = kernel_wasm(true);
        let source = boot_module(&[&kernel, &child], |spans| {
            [install(spans[0], 0), spawn(spans[1])].concat()
        });

        let run = run(&source, CpuConfig::default(), 16);
        run.assert_no_traps();
        assert!(run.saw(2, "exited"));
        // The kernel was still PENDING at boot exit, then observed the child.
        assert_eq!(run.hardware_operations, 1);
    }

    #[test]
    fn exit_hook_restores_kernel_fuel_quantum_for_later_syscalls() {
        let exiting_child = user_wasm("", "");
        let spinning_child = user_wasm(&expect_syscall(9, 0, 0, 0, 0, 0), "");
        let kernel = kernel_wasm(true);
        let source = boot_module(&[&kernel, &exiting_child, &spinning_child], |spans| {
            [install(spans[0], 0), spawn(spans[1]), spawn(spans[2])].concat()
        });
        let cpu = CpuConfig {
            fuel_per_tick: 512,
            quantum_fuel: 256,
            ..CpuConfig::default()
        };

        let run = run(&source, cpu, 12);
        run.assert_no_traps();
        assert!(
            run.saw(1, "exited"),
            "boot must finish before the child starts"
        );
        assert!(
            run.events
                .iter()
                .any(|(pid, text)| *pid == 3 && text.starts_with("runnable next tick")),
            "an infinite kernel syscall must yield across scheduler ticks; events: {:?}",
            run.events
        );
        assert_eq!(
            run.hardware_operations, 1,
            "the boot exit hook should run once before the child syscall"
        );
    }

    #[test]
    fn kernel_trap_is_a_panic_that_kills_unprivileged_processes() {
        let trigger = user_wasm(&expect_syscall(5, 0, 0, 0, 0, -14), "");
        let spinner = user_wasm("loop call $yield br 0 end", "");
        let inspector = wat::parse_str(
            r#"(module
  (import "hal" "hal_ram_used_pages" (func $pages (result i64)))
  (memory (export "memory") 1)
  (func (export "_start")
    (if (i64.ne (call $pages) (i64.const 1)) (then unreachable))))"#,
        )
        .expect("privileged memory inspector WAT compiles");
        let kernel = kernel_wasm(false);
        let source = boot_module(&[&kernel, &trigger, &spinner, &inspector], |spans| {
            [
                install(spans[0], 0),
                spawn(spans[1]),
                spawn(spans[2]),
                spawn_privileged(spans[3]),
            ]
            .concat()
        });

        let run = run(&source, CpuConfig::default(), 16);
        run.assert_no_traps();
        assert!(
            run.events
                .iter()
                .any(|(pid, text)| *pid == 0 && text.starts_with("kernel panic:"))
        );
        assert!(run.saw(3, "killed: kernel panic"));
        assert!(
            run.saw(4, "exited"),
            "a surviving privileged process must not retain the dead kernel's RAM"
        );
        assert!(run.saw(1, "exited"), "privileged processes survive a panic");
        assert!(run.finished);
    }

    #[test]
    fn kill_is_deferred_until_the_kernel_call_returns() {
        let child = user_wasm(
            &format!(
                "{} loop call $yield br 0 end",
                "i32.const 6 i32.const 0 i32.const 0 i32.const 0 i32.const 0 call $syscall drop"
            ),
            "",
        );
        let kernel = kernel_wasm(false);
        let source = boot_module(&[&kernel, &child], |spans| {
            [install(spans[0], 0), spawn(spans[1])].concat()
        });
        let cpu = CpuConfig {
            fuel_per_tick: 48,
            quantum_fuel: 16,
            ..CpuConfig::default()
        };

        let mut kill_requested = false;
        let run = run_with(&source, cpu, 400, |simulation, events| {
            if !kill_requested
                && events
                    .iter()
                    .any(|(pid, text)| *pid == 2 && text.starts_with("runnable next tick"))
            {
                kill_requested = true;
                simulation.kill_process(2).expect("kill is accepted");
            }
        });
        run.assert_no_traps();
        assert!(
            kill_requested,
            "the kernel call should span several ticks; events: {:?}",
            run.events
        );
        assert!(run.saw(2, "killed"));
        assert_eq!(
            run.hardware_operations, 100,
            "the kernel call must run to completion before the kill applies"
        );
        assert!(run.finished);
    }

    #[test]
    fn kernel_install_completes_a_tick_after_the_call() {
        let kernel = kernel_wasm(false);
        let child = user_wasm(&expect_syscall(1, 0, 0, 0, 0, 0), "");
        let source = boot_module(&[&kernel, &child], |spans| {
            // The install returns 0 only after the compile tick has passed.
            [install(spans[0], 0), spawn(spans[1])].concat()
        });

        let run = run(&source, roomy_cpu(), 8);
        run.assert_no_traps();
        let yielded = run
            .tick_of(1, "voluntarily yielded")
            .expect("install must yield through a compile tick");
        let exited = run.tick_of(1, "exited").expect("boot process exits");
        assert!(exited > yielded, "install should resume after yielding");
        assert!(run.finished);
    }

    #[test]
    fn boot_kernel_init_is_deferred_until_boot_exit_and_precedes_exit_hooks() {
        // The boot process owns a Wasm memory while it runs. The installed
        // kernel records initialization and its exit hook verifies that
        // initialization completed before the boot exit is delivered.
        let kernel = wat::parse_str(
            r#"(module
  (import "hal" "increment" (func $increment))
  (import "hal" "hal_ram_used_pages" (func $pages (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "\00\01\00\00\00\01\00\00\00\02\00\00\00\01\00\00")
  (func (export "kernel_get_io_buffers") (result i32) i32.const 0)
  (func (export "kernel_init")
    (if (i64.ne (call $pages) (i64.const 1)) (then unreachable))
    (i32.store (i32.const 1024) (i32.const 1))
    call $increment)
  (func (export "kernel_process_exited") (param i64 i32)
    (if (i32.eqz (i32.load (i32.const 1024))) (then unreachable))
    call $increment)
  (func (export "kernel_syscall") (param i64 i32 i32 i32 i32) (result i32)
    i32.const 0))"#,
        )
        .expect("deferred-init kernel WAT compiles");
        let child = user_wasm(&expect_syscall(1, 0, 0, 0, 0, 0), "");
        let source = boot_module(&[&kernel, &child], |spans| {
            [
                install(spans[0], 0),
                // The BIOS remains PENDING until it exits, so this call must
                // report KERNEL_NOT_INSTALLED without running kernel_init.
                expect_syscall(1, 0, 0, 0, 0, -13),
                spawn(spans[1]),
            ]
            .concat()
        });

        let run = run(&source, roomy_cpu(), 8);
        run.assert_no_traps();
        assert!(run.saw(1, "exited"), "boot should exit normally");
        assert!(run.saw(2, "exited"), "the child should run after init");
        assert_eq!(
            run.hardware_operations, 2,
            "deferred init and the boot exit hook should each run once"
        );
        assert!(run.finished);
    }

    #[test]
    fn spawned_child_starts_a_tick_after_the_request() {
        let child = user_wasm("", "");
        let source = boot_module(&[&child], |spans| spawn(spans[0]));

        let run = run(&source, roomy_cpu(), 8);
        run.assert_no_traps();
        // The child starts after the tick in which the boot process requested it.
        assert_eq!(run.tick_of(1, "exited"), Some(1));
        assert_eq!(run.tick_of(2, "spawned"), Some(2));
        assert_eq!(run.tick_of(2, "exited"), Some(2));
        assert!(
            run.finished,
            "the simulation is not finished while a child is compiling"
        );
    }

    #[test]
    fn kill_cancels_a_child_that_is_still_compiling() {
        let child = user_wasm("", "");
        let source = boot_module(&[&child], |spans| {
            format!(
                "i32.const {} i32.const {} call $spawn call $kill drop\n",
                spans[0].0, spans[0].1
            )
        });

        let run = run(&source, CpuConfig::default(), 8);
        run.assert_no_traps();
        assert!(run.saw(2, "killed before it started"));
        assert!(!run.saw(2, "spawned"), "a cancelled child must never start");
        assert!(run.finished);
    }
}

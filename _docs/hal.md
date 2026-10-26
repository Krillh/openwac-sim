# WebAssembly HAL

Guest modules import host functions from the Wasm module `hal`. The runtime
does not add imports to a module: declare each function the guest calls using
the exact name and signature shown here.

## Quick reference

| Import | Wasm signature | Access | Result |
| --- | --- | --- | --- |
| `increment` | `() -> ()` | Privileged | Records one hardware operation. |
| `hal_yield` | `() -> ()` | Any process | Suspends until the next simulation tick. |
| `hal_device_count` | `() -> i32` | Privileged | Number of attached devices. |
| `hal_device_info` | `(i32, i32, i32) -> i32` | Privileged | Writes interface ID and UUID to guest memory. |
| `hal_device_call` | `(i32, i32, i32, i32, i32, i32) -> i32` | Privileged | Calls a device operation by UUID. |
| `hal_poll_event` | `(i32, i32) -> i32` | Privileged | Removes the next fitting message from this process's mailbox. |
| `hal_spawn_process` | `(i32, i32) -> i64` | Privileged | Queues an unprivileged child process. |
| `hal_spawn_process_privileged` | `(i32, i32) -> i64` | Privileged | Queues a privileged child process. |
| `hal_kill_process` | `(i64) -> i32` | Privileged | Queues a process kill. |
| `hal_install_kernel` | `(i32, i32) -> i32` | Privileged | Installs the calling computer's syscall kernel. |
| `hal_wake` | `(i64) -> i32` | Privileged | Wakes a process blocked in a syscall. |
| `hal_DEBUG_PRINT` | `(i32, i32) -> i32` | Debug host build only; privileged | Writes guest bytes and a newline to host stdout. |

For buffer calls, `i32` pointer and length values are byte offsets and byte
counts in the guest's exported linear memory named `memory`. Values must be
nonnegative and the full range must fit in memory. HAL buffers are limited to
16 MiB; some operations have a smaller work bound. `i64` is used for process
IDs. The UUID byte sequence is the standard 16-byte UUID representation.

The caller must be privileged to import every function except `hal_yield`.
The boot program and privileged children run with that tier. Unprivileged
modules that import a privileged function fail to instantiate because that
import is not linked. Unprivileged programs make kernel requests through the
`sys` module; see [System calls](./syscalls.md).

## Imports used by the installed kernel

The current kernel imports these privileged HAL functions from its separate
Wasm store:

| Import | Kernel use |
| --- | --- |
| [`hal_device_count`](#hal_device_count) | Enumerate attached devices. |
| [`hal_device_info`](#hal_device_info) | Get a device's interface ID and UUID. |
| [`hal_device_call`](#hal_device_call) | Read and write disks and communicate with display and keyboard devices. |
| [`hal_spawn_process`](#hal_spawn_process-and-hal_spawn_process_privileged) | Spawn an unprivileged process. |
| [`hal_spawn_process_privileged`](#hal_spawn_process-and-hal_spawn_process_privileged) | Spawn a privileged process. |
| [`hal_kill_process`](#hal_kill_process) | Request process termination. |
| [`hal_wake`](#hal_wake) | Resume a process waiting for external I/O. |

`hal_install_kernel` is used by the privileged boot program to install the
kernel. The installed kernel does not import it and cannot replace itself.
The kernel's imports are declared in
[`_llm/kernel/src/syscalls.zig`](../../_llm/kernel/src/syscalls.zig); device
calls are wrapped by the kernel's display and keyboard modules.

## Device discovery and access

### `hal_device_count`

```wat
(import "hal" "hal_device_count" (func (result i32)))
```

Returns the number of devices currently attached to the computer. Device
indices can change when devices are added or removed.

### `hal_device_info`

```wat
(import "hal" "hal_device_info"
  (func (param i32 i32 i32) (result i32)))
```

Parameters are `index`, `output_ptr`, and `output_capacity`. On success the
host writes this 20-byte record and returns `0`:

| Offset | Size | Contents |
| ---: | ---: | --- |
| 0 | 4 | Interface ID, little-endian `u32` |
| 4 | 16 | Device UUID bytes |

Devices are enumerated in ascending UUID order. Use the UUID from this record
for later calls; do not retain an index as a device identity. Returns `-3`
for a negative index/pointer or an unknown index, `-5` if capacity is below
20, and `-9` if the memory export or access is invalid.

### `hal_device_call`

```wat
(import "hal" "hal_device_call"
  (func (param i32 i32 i32 i32 i32 i32) (result i32)))
```

Parameters are `uuid_ptr`, `operation`, `arguments_ptr`, `arguments_len`,
`response_ptr`, and `response_capacity`. The UUID pointer addresses 16 bytes;
the operation is interpreted as an unsigned 32-bit operation number. The host
copies the argument buffer, calls the device, and copies response bytes back
only when the device returns a positive byte count. The result is therefore
negative on failure, zero for success without response bytes, or a positive
response length. Argument consumption is not reflected in the result.

The call requires privilege. Argument and response buffers are each limited
to 16 MiB, and their combined size may not exceed 256 KiB. The host charges 8
units plus one per 256 combined bytes, rounded up. A call waits for a later
tick if its charge does not fit the remaining budget; it returns `-11` if its
charge cannot fit alongside one scheduler quantum. Device operation numbers
and device-specific results are in [Device interfaces](./device_interfaces.md).

Common HAL errors include `-3` for invalid arguments, `-7` if the UUID is not
available, `-8` for an invalid pointer/range, `-9` for missing or inaccessible
guest memory, and `-11` for an operation too expensive to fit in one tick.
Negative device results pass through unchanged, so their meanings depend on
the selected interface.

## Scheduling and messages

### `hal_yield`

```wat
(import "hal" "hal_yield" (func))
```

Suspends the caller until the next simulation tick, then resumes after the
call. Yielding gives up the current turn. The process can run during the next
tick if the scheduler has budget for it.

### `hal_poll_event`

```wat
(import "hal" "hal_poll_event"
  (func (param i32 i32) (result i32)))
```

Parameters are `output_ptr` and `output_capacity`. This is a nonblocking FIFO
poll of the caller's mailbox. A fitting message is copied and removed; the
result is its byte length. Message bytes are opaque to the HAL. Returns `-1`
when empty, `-2` for negative arguments or when the next message is too large
to fit, `-3` for missing/invalid guest memory, and `-11` when capacity
exceeds the 256 KiB operation bound. A message that does not fit stays queued.
An empty message returns `0`, distinct from an empty mailbox. The charge is
one unit plus one per 256 bytes of capacity, rounded up.

## Process control

### `hal_spawn_process` and `hal_spawn_process_privileged`

```wat
(import "hal" "hal_spawn_process" (func (param i32 i32) (result i64)))
(import "hal" "hal_spawn_process_privileged"
  (func (param i32 i32) (result i64)))
```

Both take a pointer and byte length for a Wasm binary in guest memory. The
ordinary form creates an unprivileged child; the privileged form creates a
privileged child and is available only to privileged callers. The module
starts at its exported `_start` function and must declare all imports it uses.
Pass compiled Wasm bytes, not WAT source text.

A positive result is the allocated process ID and means the request was
queued, not that compilation or startup succeeded. `0` indicates invalid
length or memory, empty input, exhausted process-ID space, or a request queue
failure. Privileged spawn by an unprivileged caller returns `-12`. A valid
request costs 8 units plus one per 16 KiB of Wasm, rounded up; an impossible
charge returns `-11`. Spawn requests are processed by the simulator after the
call, so the child does not execute immediately.

### `hal_kill_process`

```wat
(import "hal" "hal_kill_process" (func (param i64) (result i32)))
```

Queues a kill request for the positive process ID. Returns `0` when queued,
`-1` for process ID zero or a queue-lock failure, `-12` without privilege,
and `-11` if the charge cannot fit in one tick. The operation costs 2 units.
The target may finish a kernel call before the pending kill takes effect.

### `hal_wake`

```wat
(import "hal" "hal_wake" (func (param i64) (result i32)))
```

Wakes a process parked in a blocking syscall. The wake is sticky: if it arrives
before the process blocks, the next block completes immediately. Spurious
wakes are harmless. Returns `0` for a process on this computer, `-3` for a
nonpositive or unknown ID, `-12` without privilege, or `-11` if the one-unit
charge cannot fit in a tick.

### `hal_install_kernel`

```wat
(import "hal" "hal_install_kernel" (func (param i32 i32) (result i32)))
```

Takes a pointer and length for a Wasm kernel binary. The host compiles and
instantiates it. Legacy kernels have their `kernel_get_io_buffers` descriptor
read and optional `kernel_init` called during installation. Kernels that export
`kernel_prepare_io_buffers` defer initialization and I/O-buffer allocation
until their first syscall or process-exit hook; their stack must already be
valid at instantiation. Installation suspends the caller until the next tick while
compilation runs. A successful install returns `0`. It returns `-12` without
privilege, `-15` if a kernel is already installed or has panicked, `-16` if
compilation/instantiation/required exports or eager initialization fail, `-3` for
invalid length, `-8` or `-9` for memory errors, and `-11` for an impossible
charge. The charge is 8 units plus one per 16 KiB of binary, rounded up, plus
fuel consumed by eager `kernel_init`. Lazy startup is charged to the first
syscall or process-exit hook. See [System calls](./syscalls.md) for the kernel ABI.

## Debug output

### `hal_DEBUG_PRINT`

```wat
(import "hal" "hal_DEBUG_PRINT" (func (param i32 i32) (result i32)))
```

This import is registered only when the host is built with debug assertions
and is available only to privileged modules. It writes the requested guest
bytes to host stdout, followed by a newline. The payload is limited to 256
KiB and costs one unit plus one per 256 bytes, rounded up. It returns `0` on
success, `-11` when too large or too expensive, `-1` for a negative pointer
or length, `-2` for unavailable or invalid guest memory, and `-3` for a host
stdout write error.

## Import declarations in C

The companion C header [hal.h](./hal.h) provides the declarations, constants,
and import-name macros. Its fixed-width integer signatures map to the Wasm
signatures above when targeting `wasm32`. `HAL_EXPORT_START`
declares the `_start` export; it does not declare a HAL import. The `DEBUG`
macro controls whether the header declares `hal_DEBUG_PRINT`, independently
of whether the host was built with debug assertions.

## CPU budget

Each computer has a configurable fuel budget per tick and per-process
quantum. Defaults are 4096 and 256. The scheduler reserves a whole quantum
before polling each process; leftover fuel smaller than a quantum is unused.
Host work such as copying buffers and operating devices is charged separately
from Wasm instruction fuel. A valid call may wait for a later tick when the
current remaining budget is too small. If its full charge cannot fit beside
one quantum, it returns `-11`. These values are simulation fuel units, not
measurements of physical CPU time.

# Device interfaces

Each attached device has a UUID and an interface ID. Discover devices with
[`hal_device_count` and `hal_device_info`](./hal.md#device-discovery-and-access),
then call one by UUID with `hal_device_call`. Interface IDs identify the
operation and byte-layout contract; they are not device addresses.

## Quick reference

All multibyte integers are little-endian. An operation receives an opaque
argument byte slice and a caller-sized response buffer. It returns a negative
status on failure, `0` for success without response bytes, or the positive
number of response bytes written. A positive result does not report how many
argument bytes were consumed.

| Interface ID | Device | Operation IDs |
| --- | --- | --- |
| `0x0100` | Persistent disk | `00` root query, `01` set root, `10` block size, `11` block count, `20` read block, `30` write block |
| `0x0200` | Host file importer | `00` file size, `01` read 256-byte block, `02` file name, `03` file-name length |
| `0x0300` | Serial monitor | `00` write, `01` read, `02` peek, `03` available bytes |
| `0x0400` | Tiny Text Display | `00` geometry, `10` read cells, `20` write cells, `21` fill, `22` copy, `30` set cursor, `31` get cursor |

## Common operation rules

- The `operation` parameter is a 32-bit unsigned operation ID.
- Device-specific operation errors are the shared status values listed at the
  end of this document. HAL-level failures are described in [HAL](./hal.md).
- `hal_device_call` is privileged. A device's interface contract does not
  grant guest code access on its own.
- Buffers are binary and may contain zero bytes. Text operations state when
  they expect ASCII; strings are not implicitly NUL-terminated.

## `0x0100` Persistent disk

This device exposes a fixed-size, writable block device. The default geometry
is 32 blocks of 256 bytes each. Simulation configuration can choose a
different nonzero block count and block size. Its backing file is stored under
the simulation runtime root at `data/disks/<uuid>`.

| Op | Name | Arguments | Response | Behavior |
| --- | --- | --- | --- | --- |
| `0x00` | `IS_ROOT` | None | 1 byte | `0` or `1` for the current root flag. |
| `0x01` | `SET_ROOT` | First byte: `0` or `1` | None | Changes the root flag. |
| `0x10` | `BLOCK_SIZE` | None | `u32` | Configured bytes per block. |
| `0x11` | `BLOCK_COUNT` | None | `u32` | Configured number of blocks. |
| `0x20` | `READ_BLOCK` | `block:u32` | One full block | Reads the selected block. |
| `0x30` | `WRITE_BLOCK` | `block:u32`, then one full block | None | Replaces the selected block. |

For `READ_BLOCK`, the response capacity must be at least the configured block
size. For `WRITE_BLOCK`, the argument consists of the four-byte block number
followed by exactly one full block. `IS_ROOT` writes its one-byte response;
`SET_ROOT` reads the first argument byte. Block indices outside
`0..BLOCK_COUNT` return `OUT_OF_BOUNDS`. Short block indices return
`ARGUMENT_OVERRUN`; an undersized write buffer returns `ARGUMENT_OVERRUN`,
while an oversized write buffer returns `INVALID_ARGUMENT`.

Example configuration:

```toml
[[computers.devices]]
type = "disk"
is_root = true
# uuid is optional; omitted UUIDs are generated.
block_count = 32
block_size = 256
remove_on_drop = false
```

`remove_on_drop = true` removes the backing file when the device is dropped.

## `0x0200` Host file importer

This read-only device exposes one regular file selected by host configuration.
The configured relative path resolves below the runtime root's
`external-files/` directory. Absolute paths, parent traversal, and resolved
paths outside that directory are rejected. The final path component must be
an ASCII file name. The device snapshots the file size when it is created and
reads data in 256-byte blocks; the final block can be shorter.

| Op | Name | Arguments | Response | Behavior |
| --- | --- | --- | --- | --- |
| `0x00` | `FILE_SIZE` | None | `file_size:u64` | Size captured at device creation. |
| `0x01` | `READ_BLOCK` | `block:u32` | Up to 256 bytes | Reads the requested block; final block may be partial. |
| `0x02` | `FILE_NAME` | None | ASCII bytes | Configured basename, without terminator. |
| `0x03` | `FILE_NAME_LENGTH` | None | `u32` | Basename byte length. |

The `READ_BLOCK` response capacity must fit the bytes in that block. A block
outside the file returns `OUT_OF_BOUNDS`; a short block index returns
`ARGUMENT_OVERRUN`. If the source is truncated or unreadable after creation,
the operation returns `DEVICE_BROKEN`. Insufficient response space returns
`RESPONSE_OVERRUN`.

Example configuration (with `gcd.wat` at `<runtime-root>/external-files/gcd.wat`):

```toml
[[computers.devices]]
type = "file_importer"
path = "gcd.wat"
# uuid is optional; omitted UUIDs are generated.
```

## `0x0300` Serial monitor

The serial monitor connects guest operations to host console I/O. Each device
has a configured host-side `name`. Host input is queued as raw bytes in a FIFO
with a maximum capacity of 64 KiB. A host input submission that would exceed
the capacity is rejected whole; it does not append a partial submission.

| Op | Name | Arguments | Response | Behavior |
| --- | --- | --- | --- | --- |
| `0x00` | `WRITE` | Bytes to write | None | Writes bytes to host stdout and flushes. |
| `0x01` | `READ` | None | Up to response capacity bytes | Copies and consumes queued input. |
| `0x02` | `PEEK` | None | Up to response capacity bytes | Copies queued input without consuming it. |
| `0x03` | `AVAILABLE` | None | `u32` | Number of queued bytes. |

`READ` and `PEEK` require a nonempty response buffer, even when there is no
input; with capacity they return `0` when the FIFO is empty. `AVAILABLE`
requires at least four response bytes. `WRITE` accepts raw bytes and returns
`0` when host output succeeds. Input argument bytes to `READ`, `PEEK`, and
`AVAILABLE` are rejected. Output errors return `DEVICE_BROKEN`.

Example configuration:

```toml
[[computers.devices]]
type = "serial_monitor"
name = "terminal"
# uuid is optional; omitted UUIDs are generated.
```

The configured name is used by the host console to address the device; guests
still select it by UUID through the HAL.

## `0x0400` Tiny Text Display

The TTD has 60 columns by 30 rows. Each cell contains one literal CP437 byte;
the host renders it using the bundled `Codepage-437-fixed.png` atlas. Adding a
`type = "ttd"` device opens a window, initially showing the 256 CP437 glyphs in
the top-left 32-by-8 cells. Guest writes update that same window. Closing the
window makes subsequent operations return `DEVICE_NOT_AVAILABLE`.

| Op | Name | Arguments | Response |
| --- | --- | --- | --- |
| `0x00` | GEOMETRY | Empty | `columns:u32, rows:u32` (8 bytes: `60,30`). |
| `0x10` | READ_CELLS | `x:u32, y:u32, width:u32, height:u32` | `width * height` bytes, row-major. |
| `0x20` | WRITE_CELLS | Rectangle, then exactly `width * height` bytes | Empty. |
| `0x21` | FILL | Rectangle, then `character:u8` | Empty. |
| `0x22` | COPY | Source rectangle, then `destination_x:u32, destination_y:u32` | Empty. |
| `0x30` | SET_CURSOR | `x:u32, y:u32, visible:u8, blink:u8` | Empty. |
| `0x31` | GET_CURSOR | Empty | `x:u32, y:u32, visible:u8, blink:u8` (10 bytes). |

Rectangles must have positive dimensions and fit entirely within the display;
no clipping occurs. COPY snapshots its source, so overlapping copies are safe.
The cursor is hidden at `(0,0)` initially and never changes cell contents.

```toml
[[computers.devices]]
type = "ttd"
```

## Shared device status codes

| Code | Name | Meaning |
| ---: | --- | --- |
| `0` | `OK` | Operation succeeded without response bytes. |
| `-1` | `UNKNOWN_ERROR` | Generic/unknown failure. |
| `-2` | `INVALID_OPERATION` | Operation ID is not supported by this interface. |
| `-3` | `INVALID_ARGUMENT` | Argument value or shape is invalid. |
| `-4` | `ARGUMENT_OVERRUN` | Argument buffer is shorter than required. |
| `-5` | `RESPONSE_OVERRUN` | Response buffer is too short. |
| `-6` | `DEVICE_BROKEN` | Device I/O failed or device state is unusable. |
| `-7` | `DEVICE_NOT_AVAILABLE` | Device UUID is not attached/available (HAL result). |
| `-8` | `POINTER_INVALID` | HAL guest pointer or range is invalid. |
| `-9` | `MEMORY_INVALID` | HAL guest memory is missing or inaccessible. |
| `-10` | `OUT_OF_BOUNDS` | Requested block/index is outside the device. |
| `-11` | `SYSCALL_TOO_EXPENSIVE` | HAL operation cannot fit the per-tick work budget. |
| `-12` | `PERMISSION_DENIED` | Caller lacks privilege. |
| `-13` | `KERNEL_NOT_INSTALLED` | No syscall kernel is installed. |
| `-14` | `KERNEL_PANICKED` | Installed syscall kernel has panicked. |
| `-15` | `KERNEL_ALREADY_INSTALLED` | Kernel is installed, installing, or unavailable after panic. |
| `-16` | `KERNEL_INVALID` | Kernel could not be compiled, instantiated, or initialized. |

The runtime defines these values in `device_status`. A specific device may
use only a subset. Device methods return their status directly through
`hal_device_call`; the shared numbers are not a separate transport layer.

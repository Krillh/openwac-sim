# OpenWAC simulator

A Rust/Wasmtime development runtime for the OpenWAC Minecraft mod. Guest-visible
HAL, device protocols, boot handoff, and process lifecycle follow the mod. The
simulator also supports WAT boot firmware, serial consoles, headless displays,
and running without wall-clock pacing.

Build and run from this directory:

```powershell
cargo build
cargo run -- --unpaced run examples/hello/simulation.toml
cargo run -- run examples/full/config.toml
cargo run -- --unpaced run examples/full/config.toml
```

Paths in a configuration are relative to its containing directory. Boot firmware
is loaded from `bootloaders/`, imported files from `external-files/`, explicit
additional EEPROM images from `firmware/`, and persistent disks from `data/disks/`.
The boot firmware is automatically exposed as an EEPROM unless an EEPROM is
explicitly configured. WAT is accepted for development boot firmware; spawned
processes and kernels must contain compiled Wasm bytes.

```toml
max_ticks = 10000
tick_interval_ms = 50

[[computers]]
name = "dev"
bootloader = "boot.wasm"

[computers.cpu]
fuel_per_tick = 65536
quantum_fuel = 256
max_processes = 16
max_memory_bytes = 1048576

[[computers.devices]]
type = "disk"
uuid = "00000000-0000-0000-0000-000000000002"
block_count = 512
block_size = 2048

[[computers.devices]]
type = "ttd"
headless = true

[[computers.devices]]
type = "keyboard"
name = "keyboard"

[[computers.devices]]
type = "peripheral_port"
```

The example CPU/RAM limits match the mod's tier-one hardware. Omitted CPU
settings use the platform-neutral machine defaults (4096 fuel, quantum 256,
8 processes, 64 MiB RAM). Memory includes all process and kernel linear memories;
a shared 256 MiB limit also applies across computers.

Spawn requests made in tick N become eligible in tick N+1. The BIOS runs alone;
children wait until it exits successfully and deferred kernel initialization
finishes. A BIOS trap crashes its computer. `hal_yield` rotates the ready queue
and may resume within the same tick. Wasmtime and Chicory have different fuel
costs, and simulator execution currently accounts in polling quanta; identical
instruction-level timing is not guaranteed.

Device calls return status 0 on success and place data in the output buffer.
Disk root-flag operations are retired. The legacy `is_root` configuration key
is accepted but ignored. New disk images use the mod's OWDS header; existing
raw simulator images must be converted explicitly before use. Opening an image
with mismatched identity or geometry fails rather than resizing it.

A TTD begins with space-filled cells. `headless = true` suppresses its window
while preserving device behavior. Keyboard host input currently emits text and
reset events. `send keyboard hello` sends text using the simulator console; the
same command addresses configured serial monitors. Minecraft's physical wiring,
player capture sessions, and power leases are outside the simulator's world model.
Chat HAL output appears in the log and tick events. The CPU-time metric measures
the last completed simulated computer turn; RAM usage is reported in 64 KiB pages.
The command exits with an error if a guest traps, startup fails, or a computer
or kernel crashes; those diagnostics are visible at the default log level.

Run the simulator checks:

```powershell
cargo test --offline
```

In this multi-project workspace, additionally run the mod's shipped diagnostic
firmware and bootloader against the simulator:

```powershell
cargo test --offline mod_checks -- --ignored --nocapture
```

Those optional integration checks use the sibling `openwac` checkout. Set
`OPENWAC_MOD_ROOT` to use another mod checkout. They exercise disk persistence,
TTD read/write/copy/cursor behavior, importer output, and packaged bootloader
import linking. The regular suite separately verifies disk-to-kernel handoff
and a user syscall with a synthetic BIOS written for the current ABI. Test
media is generated under `target/mod-checks/`.

The bundled bootloader and local `_wip` guest artifacts still call the retired
disk root-flag operations. Update those guest projects before attempting a
complete OS boot against the current mod or simulator; the simulator does not
silently re-enable the retired operations.

The `_docs/` directory contains historical simulator notes. The current mod's
HAL and device implementations are the compatibility authority.

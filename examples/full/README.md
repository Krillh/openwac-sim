# Mod-compatible boot setup

Run this configuration from the simulator directory:

```powershell
cargo run -- run examples/full/config.toml
cargo run -- --unpaced run examples/full/config.toml
```

Place BIOS firmware built for the current mod contract in `bootloaders/boot.wasm`.
The packaged `bootloader-v1.wasm` and local `_wip` bootloader, kernel, and OS
still use retired disk root-flag operations and need guest-side updates before
a complete boot is possible. The simulator follows the mod and rejects those
operations. Populate compatible disk images according to the updated guest's
boot contract. Kernel and OS media preparation belongs to that guest project.

The two disks have fixed UUIDs so prepared media can be reused. Their backing
images live in this example's `data/disks/` directory. Each image contains a
32-byte OWDS host header followed by guest filesystem blocks. Raw filesystem
images must be wrapped in the matching header before use. Legacy simulator
images and disk root-flag operations are not the current mod contract.

The TTD window starts blank. Change `headless` to true for automated runs.
`send keyboard hello` injects text using the host console. The optional serial
monitor remains a simulator development extension.

File importers expose `external-files/kernel.x`, `external-files/os.x`, and
`external-files/sh.x` for guest tools that explicitly use importers. Their
presence does not copy anything onto a disk automatically. Importer source
errors are reported when the guest accesses the device.

The `_wip` guest artifacts in this workspace may target older interfaces;
select artifacts built for the current mod before interpreting a boot failure
as a simulator regression. The optional `mod_checks` integration tests execute
the mod's shipped firmware with generated test media independently of these files.

# Disk mounts

The kernel implements disk mounts as an in-memory namespace over TBDF volumes.
The on-disk TBDF format has no mount record. Mounts therefore must be recreated
by the OS after the kernel starts. The Rust supervisor creates persistent
mountpoint directories under `mnt/` on the root volume and mounts each
attached non-root disk at `mnt/<lowercase-uuid>/`. The directories persist;
the mount relationships do not. It does not format disks or modify imported
file contents.

## Namespace rules

- Each attached disk has a separate TBDF volume. Root entry ID `0` is local to
  its volume; IDs from different disks are not interchangeable.
- The root disk is the attached disk whose `IS_ROOT` operation returns `1`.
  The kernel mounts its TBDF volume lazily on first filesystem access.
- A mount maps an existing directory in one volume to the root of another
  attached disk. The mounted root's children appear at the mountpoint and hide
  the mountpoint directory's former children.
- A mountpoint must be a non-root directory entry; the global and mounted
  volume roots cannot themselves be mountpoints.
- Mounts can be nested below directories in mounted volumes. The root disk
  cannot be mounted again, and one disk cannot be mounted at multiple points.
- `..` at a mounted volume's root returns to the directory containing that
  mountpoint. `..` at the global root stays at the global root.
- Unmount fails while the mount has child mounts or open file handles. The
  root disk cannot be unmounted.
- `FS_MOUNT` flag bit 0 selects read-only mode. Other flag bits are invalid.
- Mount entries and resident TBDF metadata are volatile kernel state. TBDF
  streams file payload blocks from the backing disk instead of retaining a full
  disk snapshot. A new kernel install starts with no device table or mounts.

## Paths

Filesystem calls use a sequence of NUL-terminated path components, such as
`dir\0subdir\0file\0`. An empty byte string names the global root. Components
are printable ASCII names; empty components are invalid. `.` stays in the
current directory and `..` follows namespace parent links, including across
mounts. The whole path is limited to 256 bytes and every nonempty path must
end in NUL.

## Syscall wire formats

The syscall numbers and response records are defined in
[`syscalls.md`](./syscalls.md):

- `FS_MOUNT`: disk UUID (16 bytes), flags (`u32`), then mountpoint path.
- `FS_UNMOUNT`: mountpoint path.
- `FS_CHILDREN`: ordinal (`u32`) followed by directory path; returns one
  child record.
- `FS_SEEK`: path; returns the resolved entry record.

An entry record is `id:u16`, `parent:u16`, `is_directory:u8`,
`readonly:u8`, `size:u32`, followed by name bytes. The response length is 10
plus the name length. Directory size is TBDF's direct-child count; file size
is its byte length.

The kernel uses `_llm/tbdf` for block allocation and per-volume operations.
Its filesystem layer owns device selection, mount traversal, cross-volume
parent handling, and file handles. Mounted images are limited to eight volumes
and 8 MiB total disk bytes. `UPDATE_FILESYSTEM` discards resident metadata and
closes file handles so subsequent calls reload the current disk state.

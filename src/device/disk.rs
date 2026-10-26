use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::device::{Device, device_status};

const DEFAULT_BLOCK_COUNT: u32 = 512;
const DEFAULT_BLOCK_SIZE: u32 = 2048;
const BLOCK_INDEX_SIZE: usize = size_of::<u32>();
const HEADER_SIZE: u64 = 32;
const MAGIC: u32 = 0x4f57_4453;
const VERSION: u32 = 1;

#[allow(dead_code)]
pub struct Disk {
    uuid: Uuid,
    block_count: u32,
    block_size: u32,
    path: PathBuf,
    file: Option<File>,
    remove_on_drop: bool,
}

impl Disk {
    #[allow(dead_code)]
    pub fn new(runtime_root: &Path, _is_root: bool) -> io::Result<Self> {
        let uuid = Uuid::new_v4();
        let path = runtime_root
            .join("data")
            .join("disks")
            .join(uuid.to_string());
        Self::create(uuid, path, DEFAULT_BLOCK_COUNT, DEFAULT_BLOCK_SIZE, false)
    }

    fn create(
        uuid: Uuid,
        path: PathBuf,
        block_count: u32,
        block_size: u32,
        remove_on_drop: bool,
    ) -> io::Result<Self> {
        std::fs::create_dir_all(path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "disk path has no parent")
        })?)?;
        let payload_length = (block_count as u64)
            .checked_mul(block_size as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "disk size overflow"))?;
        if payload_length > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disk exceeds 64 MiB",
            ));
        }
        let file = open_image(uuid, &path, block_size, block_count)?;

        Ok(Self {
            uuid,
            block_count,
            block_size,
            path,
            file: Some(file),
            remove_on_drop,
        })
    }

    fn ensure_file(&mut self) -> Result<(), i32> {
        if self.file.is_some() {
            return Ok(());
        }
        let file = open_image(self.uuid, &self.path, self.block_size, self.block_count)
            .map_err(|_| device_status::DEVICE_BROKEN)?;
        self.file = Some(file);
        Ok(())
    }

    pub(crate) fn configured(
        runtime_root: &Path,
        uuid: Uuid,
        _is_root: bool,
        block_count: u32,
        block_size: u32,
        remove_on_drop: bool,
    ) -> io::Result<Self> {
        if block_count == 0 || block_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disk block count and size must be non-zero",
            ));
        }
        let payload_length = u64::from(block_count)
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "disk size overflow"))?;
        if payload_length > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disk exceeds 64 MiB",
            ));
        }
        let path = runtime_root
            .join("data")
            .join("disks")
            .join(uuid.to_string());
        Ok(Self {
            uuid,
            block_count,
            block_size,
            path,
            file: None,
            remove_on_drop,
        })
    }

    #[allow(dead_code)]
    pub fn load_new_disk(_uuid: Uuid) -> Result<Self, ()> {
        // Loading requires the disk metadata format to be defined.
        todo!()
    }

    /// Deletes this disk and its backing file.
    #[allow(dead_code)]
    pub fn delete(mut self) -> io::Result<()> {
        self.remove_on_drop = false;
        self.file.take();
        std::fs::remove_file(&self.path)
    }

    fn block_offset(&self, block: u32) -> Result<u64, i32> {
        if block >= self.block_count {
            return Err(device_status::OUT_OF_BOUNDS);
        }
        HEADER_SIZE
            .checked_add(
                (block as u64)
                    .checked_mul(self.block_size as u64)
                    .ok_or(device_status::OUT_OF_BOUNDS)?,
            )
            .ok_or(device_status::OUT_OF_BOUNDS)
    }

    fn read_block(&mut self, block: u32, buffer: &mut [u8]) -> Result<usize, i32> {
        let offset = self.block_offset(block)?;
        let block_size =
            usize::try_from(self.block_size).map_err(|_| device_status::DEVICE_BROKEN)?;
        if buffer.len() < block_size {
            return Err(device_status::RESPONSE_OVERRUN);
        }
        self.ensure_file()?;
        let file = self.file.as_ref().ok_or(device_status::DEVICE_BROKEN)?;
        validate_image(file, self.uuid, self.block_size, self.block_count)
            .map_err(|_| device_status::DEVICE_BROKEN)?;
        read_exact_at(file, &mut buffer[..block_size], offset)?;
        Ok(block_size)
    }

    fn write_block(&mut self, block: u32, buffer: &[u8]) -> Result<(), i32> {
        let offset = self.block_offset(block)?;
        let block_size =
            usize::try_from(self.block_size).map_err(|_| device_status::DEVICE_BROKEN)?;
        if buffer.len() != block_size {
            return Err(if buffer.len() < block_size {
                device_status::ARGUMENT_OVERRUN
            } else {
                device_status::INVALID_ARGUMENT
            });
        }
        self.ensure_file()?;
        let file = self.file.as_ref().ok_or(device_status::DEVICE_BROKEN)?;
        validate_image(file, self.uuid, self.block_size, self.block_count)
            .map_err(|_| device_status::DEVICE_BROKEN)?;
        write_all_at(file, buffer, offset)?;
        file.sync_all().map_err(|_| device_status::DEVICE_BROKEN)?;
        Ok(())
    }
}

fn open_image(uuid: Uuid, path: &Path, block_size: u32, block_count: u32) -> io::Result<File> {
    std::fs::create_dir_all(
        path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "disk path has no parent")
        })?,
    )?;
    let payload_length = (block_count as u64)
        .checked_mul(block_size as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "disk size overflow"))?;
    if payload_length > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "disk exceeds 64 MiB",
        ));
    }
    let length = HEADER_SIZE + payload_length;
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => {
            file.set_len(length)?;
            let header = disk_header(uuid, block_size, block_count);
            write_all_at(&file, &header, 0).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "failed to initialize disk image header",
                )
            })?;
            file.sync_all()?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            validate_image(&file, uuid, block_size, block_count)?;
            Ok(file)
        }
        Err(error) => Err(error),
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        if !self.remove_on_drop {
            return;
        }
        self.file.take();
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            log::warn!(
                "failed to remove disk backing file {} on drop: {error}",
                self.path.display()
            );
        }
    }
}

impl Device for Disk {
    fn uuid(&self) -> Uuid {
        self.uuid
    }

    fn interface_id(&self) -> u32 {
        0x0100
    }

    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        match operation {
            op::BLOCK_SIZE | op::BLOCK_COUNT => {
                if !arguments.is_empty() {
                    return device_status::INVALID_ARGUMENT;
                }
                write_u32(
                    response,
                    if operation == op::BLOCK_SIZE {
                        self.block_size
                    } else {
                        self.block_count
                    },
                )
            }
            op::READ_BLOCK => {
                let block = match parse_block_index(arguments) {
                    Ok(block) => block,
                    Err(error) => return error,
                };
                // DiskDevice validates the index against the medium before checking
                // operation-specific trailing bytes. Keep that status precedence.
                if block >= self.block_count {
                    return device_status::OUT_OF_BOUNDS;
                }
                if arguments.len() != BLOCK_INDEX_SIZE {
                    return device_status::INVALID_ARGUMENT;
                }
                self.read_block(block, response)
                    .map_or_else(|error| error, |written| written as i32)
            }
            op::WRITE_BLOCK => {
                let block = match parse_block_index(arguments) {
                    Ok(block) => block,
                    Err(error) => return error,
                };
                self.write_block(block, &arguments[BLOCK_INDEX_SIZE..])
                    .map_or_else(|error| error, |_| device_status::OK)
            }
            _ => device_status::INVALID_OPERATION,
        }
    }
}

pub(crate) const fn default_block_count() -> u64 {
    DEFAULT_BLOCK_COUNT as u64
}

pub(crate) const fn default_block_size() -> u64 {
    DEFAULT_BLOCK_SIZE as u64
}

fn parse_block_index(arguments: &[u8]) -> Result<u32, i32> {
    let index = arguments
        .get(..BLOCK_INDEX_SIZE)
        .ok_or(device_status::ARGUMENT_OVERRUN)?;
    let bytes: [u8; BLOCK_INDEX_SIZE] = index
        .try_into()
        .map_err(|_| device_status::ARGUMENT_OVERRUN)?;
    Ok(u32::from_le_bytes(bytes))
}

fn disk_header(uuid: Uuid, block_size: u32, block_count: u32) -> [u8; HEADER_SIZE as usize] {
    let mut header = [0; HEADER_SIZE as usize];
    header[0..4].copy_from_slice(&MAGIC.to_be_bytes());
    header[4..8].copy_from_slice(&VERSION.to_be_bytes());
    header[8..24].copy_from_slice(uuid.as_bytes());
    header[24..28].copy_from_slice(&block_size.to_be_bytes());
    header[28..32].copy_from_slice(&block_count.to_be_bytes());
    header
}

fn validate_image(file: &File, uuid: Uuid, block_size: u32, block_count: u32) -> io::Result<()> {
    let expected_length = HEADER_SIZE + u64::from(block_size) * u64::from(block_count);
    if file.metadata()?.len() != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "disk image length or geometry mismatch",
        ));
    }
    let mut header = [0; HEADER_SIZE as usize];
    read_exact_at(file, &mut header, 0)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid disk image header"))?;
    if header != disk_header(uuid, block_size, block_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "disk UUID, geometry, or format mismatch",
        ));
    }
    Ok(())
}

fn write_u32(response: &mut [u8], value: u32) -> i32 {
    let bytes = value.to_le_bytes();
    if response.len() < bytes.len() {
        return device_status::RESPONSE_OVERRUN;
    }
    response[..bytes.len()].copy_from_slice(&bytes);
    bytes.len() as i32
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> Result<(), i32> {
    while !buffer.is_empty() {
        let read = file
            .seek_read(buffer, offset)
            .map_err(|_| device_status::DEVICE_BROKEN)?;
        if read == 0 {
            return Err(device_status::DEVICE_BROKEN);
        }
        offset = offset
            .checked_add(read as u64)
            .ok_or(device_status::DEVICE_BROKEN)?;
        buffer = &mut buffer[read..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> Result<(), i32> {
    while !buffer.is_empty() {
        let written = file
            .seek_write(buffer, offset)
            .map_err(|_| device_status::DEVICE_BROKEN)?;
        if written == 0 {
            return Err(device_status::DEVICE_BROKEN);
        }
        offset = offset
            .checked_add(written as u64)
            .ok_or(device_status::DEVICE_BROKEN)?;
        buffer = &buffer[written..];
    }
    Ok(())
}

mod op {
    pub const BLOCK_SIZE: u32 = 0x10;
    pub const BLOCK_COUNT: u32 = 0x11;
    pub const READ_BLOCK: u32 = 0x20;
    pub const WRITE_BLOCK: u32 = 0x30;
}

#[cfg(test)]
mod tests {
    use super::{Disk, op};
    use crate::device::{Device, device_status};
    use uuid::Uuid;

    fn temporary_disk() -> Disk {
        let uuid = Uuid::new_v4();
        let path = std::env::temp_dir()
            .join(format!("openwc-disk-test-{uuid}"))
            .join(uuid.to_string());
        Disk::create(uuid, path, 2, 8, false).expect("temporary disk is created")
    }

    #[test]
    fn block_write_then_read_round_trips_full_block() {
        let mut disk = temporary_disk();
        let parent = disk.path.parent().expect("disk has a parent").to_path_buf();
        let mut arguments = 1_u32.to_le_bytes().to_vec();
        arguments.extend_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);

        assert_eq!(
            disk.operate(op::WRITE_BLOCK, &arguments, &mut []),
            device_status::OK
        );

        let mut response = [0; 8];
        assert_eq!(
            disk.operate(op::READ_BLOCK, &1_u32.to_le_bytes(), &mut response),
            8
        );
        assert_eq!(response, [10, 20, 30, 40, 50, 60, 70, 80]);
        disk.delete().expect("temporary disk is removed");
        std::fs::remove_dir(parent).expect("temporary directory is removed");
    }

    #[test]
    fn block_operations_return_errors_for_invalid_ranges_and_sizes() {
        let mut disk = temporary_disk();
        let parent = disk.path.parent().expect("disk has a parent").to_path_buf();
        let mut response = [0; 8];

        assert_eq!(
            disk.operate(op::READ_BLOCK, &2_u32.to_le_bytes(), &mut response),
            device_status::OUT_OF_BOUNDS
        );
        assert_eq!(
            disk.operate(op::READ_BLOCK, &[], &mut response),
            device_status::ARGUMENT_OVERRUN
        );
        assert_eq!(
            disk.operate(op::READ_BLOCK, &0_u32.to_le_bytes(), &mut [0; 7]),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            disk.operate(op::WRITE_BLOCK, &0_u32.to_le_bytes(), &mut response),
            device_status::ARGUMENT_OVERRUN
        );
        let mut out_of_bounds_write = 2_u32.to_le_bytes().to_vec();
        out_of_bounds_write.extend_from_slice(&[0; 8]);
        assert_eq!(
            disk.operate(op::WRITE_BLOCK, &out_of_bounds_write, &mut response),
            device_status::OUT_OF_BOUNDS
        );
        disk.delete().expect("temporary disk is removed");
        std::fs::remove_dir(parent).expect("temporary directory is removed");
    }

    #[test]
    fn retired_root_operations_are_unsupported_and_queries_require_empty_arguments() {
        let mut disk = temporary_disk();
        let mut response = [0; 1];
        assert_eq!(
            disk.operate(0, &[], &mut response),
            device_status::INVALID_OPERATION
        );
        assert_eq!(
            disk.operate(1, &[1], &mut []),
            device_status::INVALID_OPERATION
        );
        assert_eq!(
            disk.operate(op::BLOCK_SIZE, &[1], &mut [0; 4]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            disk.operate(op::BLOCK_COUNT, &[1], &mut [0; 4]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            disk.operate(op::READ_BLOCK, &[0, 0, 0, 0, 0], &mut [0; 8]),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            disk.operate(op::READ_BLOCK, &[2, 0, 0, 0, 0], &mut [0; 8]),
            device_status::OUT_OF_BOUNDS
        );

        let parent = disk.path.parent().expect("disk has a parent").to_path_buf();
        disk.delete().expect("temporary disk is removed");
        std::fs::remove_dir(parent).expect("temporary directory is removed");
    }

    #[test]
    fn backing_file_is_removed_on_drop_when_configured() {
        let uuid = Uuid::new_v4();
        let parent = std::env::temp_dir().join(format!("openwc-disk-drop-{uuid}"));
        let path = parent.join(uuid.to_string());
        let disk = Disk::create(uuid, path.clone(), 2, 8, true).expect("temporary disk is created");

        assert!(path.exists());
        drop(disk);
        assert!(!path.exists());
        std::fs::remove_dir(parent).expect("temporary directory is removed");
    }

    #[test]
    fn backing_file_is_preserved_on_drop_by_default() {
        let uuid = Uuid::new_v4();
        let parent = std::env::temp_dir().join(format!("openwc-disk-keep-{uuid}"));
        let path = parent.join(uuid.to_string());
        let disk =
            Disk::create(uuid, path.clone(), 2, 8, false).expect("temporary disk is created");

        drop(disk);
        assert!(path.exists());
        std::fs::remove_file(path).expect("temporary disk file is removed");
        std::fs::remove_dir(parent).expect("temporary directory is removed");
    }

    #[test]
    fn persistent_image_uses_versioned_java_header_and_rejects_geometry_changes() {
        let uuid = Uuid::new_v4();
        let parent = std::env::temp_dir().join(format!("openwc-disk-header-{uuid}"));
        let path = parent.join(uuid.to_string());
        drop(Disk::create(uuid, path.clone(), 2, 8, false).expect("disk is created"));
        let header = std::fs::read(&path).expect("disk image is readable");
        assert_eq!(&header[..4], &0x4f57_4453_u32.to_be_bytes());
        assert_eq!(&header[4..8], &1_u32.to_be_bytes());
        assert_eq!(&header[8..24], uuid.as_bytes());
        assert_eq!(&header[24..28], &8_u32.to_be_bytes());
        assert_eq!(&header[28..32], &2_u32.to_be_bytes());

        let error = match Disk::create(uuid, path.clone(), 3, 8, false) {
            Err(error) => error,
            Ok(_) => panic!("an image cannot silently change configured geometry"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_file(path).expect("temporary disk is removed");
        std::fs::remove_dir(parent).expect("temporary directory is removed");
    }

    #[test]
    fn configured_disk_defers_invalid_image_failure_until_transfer() {
        let uuid = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("openwac-disk-lazy-{uuid}"));
        let directory = root.join("data").join("disks");
        std::fs::create_dir_all(&directory).expect("disk directory is created");
        let path = directory.join(uuid.to_string());
        std::fs::write(&path, b"invalid media").expect("bad media is present");

        let mut disk = Disk::configured(&root, uuid, false, 2, 8, false)
            .expect("device construction does not open media");
        let mut size = [0; 4];
        assert_eq!(disk.operate(op::BLOCK_SIZE, &[], &mut size), 4);
        assert_eq!(u32::from_le_bytes(size), 8);
        let mut count = [0; 4];
        assert_eq!(disk.operate(op::BLOCK_COUNT, &[], &mut count), 4);
        assert_eq!(u32::from_le_bytes(count), 2);
        assert_eq!(
            disk.operate(op::READ_BLOCK, &0_u32.to_le_bytes(), &mut [0; 8]),
            device_status::DEVICE_BROKEN
        );
        assert_eq!(
            std::fs::read(&path).expect("bad media remains intact"),
            b"invalid media"
        );

        drop(disk);
        std::fs::remove_dir_all(root).expect("temporary disk directory is removed");
    }
}

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path};

use uuid::Uuid;

use crate::device::{Device, device_status};

const BLOCK_SIZE: usize = 256;
const BLOCK_INDEX_SIZE: usize = size_of::<u32>();
const FILE_SIZE_SIZE: usize = size_of::<u64>();
const FILE_NAME_LENGTH_SIZE: usize = size_of::<u32>();

const EXTERNAL_FILE_PATH: &str = "external-files";

const INTERFACE_ID: u32 = 0x0200;

pub(crate) struct FileImporter {
    uuid: Uuid,
    runtime_root: std::path::PathBuf,
    relative_path: std::path::PathBuf,
    file: Option<std::path::PathBuf>,
    file_size: u64,
    file_name: String,
    initialized: bool,
}

impl FileImporter {
    pub(crate) fn configured(
        uuid: Uuid,
        runtime_root: &Path,
        relative_path: &Path,
    ) -> io::Result<Self> {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file importer path must be relative to the example's programs directory",
            ));
        }

        let file_name = relative_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file importer path must end in an ASCII file name",
                )
            })?
            .to_owned();

        Ok(Self {
            uuid,
            runtime_root: runtime_root.to_path_buf(),
            relative_path: relative_path.to_path_buf(),
            file: None,
            file_size: 0,
            file_name,
            initialized: false,
        })
    }

    fn initialize(&mut self) -> io::Result<()> {
        if self.initialized {
            return if self.file.is_some() {
                Ok(())
            } else {
                Err(io::Error::other("source initialization previously failed"))
            };
        }
        self.initialized = true;
        if !self.file_name.is_ascii() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file importer filename must be ASCII",
            ));
        }
        let external_dir = self.runtime_root.join(EXTERNAL_FILE_PATH).canonicalize()?;
        let requested_path = external_dir.join(&self.relative_path);
        let canonical_path = requested_path.canonicalize()?;
        if !canonical_path.starts_with(&external_dir) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file importer path must remain inside external-files",
            ));
        }
        let file = File::open(&canonical_path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file importer source must be a regular file",
            ));
        }
        self.file_size = metadata.len();
        self.file = Some(canonical_path);
        Ok(())
    }

    fn read_block(&mut self, block: u32, response: &mut [u8]) -> i32 {
        let offset = u64::from(block) * BLOCK_SIZE as u64;
        if offset >= self.file_size {
            return device_status::OUT_OF_BOUNDS;
        }

        let bytes_to_read = usize::try_from((self.file_size - offset).min(BLOCK_SIZE as u64))
            .expect("bounded by block size");
        if response.len() < bytes_to_read {
            return device_status::RESPONSE_OVERRUN;
        }

        let Some(path) = self.file.as_ref() else {
            return device_status::DEVICE_BROKEN;
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(_) => return device_status::DEVICE_BROKEN,
        };
        if file
            .metadata()
            .map_or(true, |metadata| metadata.len() < self.file_size)
        {
            return device_status::DEVICE_BROKEN;
        }
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return device_status::DEVICE_BROKEN;
        }
        if file.read_exact(&mut response[..bytes_to_read]).is_err() {
            return device_status::DEVICE_BROKEN;
        }

        i32::try_from(bytes_to_read).unwrap_or(device_status::DEVICE_BROKEN)
    }

    fn write_file_size(&self, response: &mut [u8]) -> i32 {
        if response.len() < FILE_SIZE_SIZE {
            return device_status::RESPONSE_OVERRUN;
        }
        response[..FILE_SIZE_SIZE].copy_from_slice(&self.file_size.to_le_bytes());
        FILE_SIZE_SIZE as i32
    }

    fn write_file_name(&self, response: &mut [u8]) -> i32 {
        let bytes = self.file_name.as_bytes();
        if response.len() < bytes.len() {
            return device_status::RESPONSE_OVERRUN;
        }
        response[..bytes.len()].copy_from_slice(bytes);
        i32::try_from(bytes.len()).unwrap_or(device_status::DEVICE_BROKEN)
    }

    fn write_file_name_length(&self, response: &mut [u8]) -> i32 {
        if response.len() < FILE_NAME_LENGTH_SIZE {
            return device_status::RESPONSE_OVERRUN;
        }
        let Ok(length) = u32::try_from(self.file_name.len()) else {
            return device_status::DEVICE_BROKEN;
        };
        response[..FILE_NAME_LENGTH_SIZE].copy_from_slice(&length.to_le_bytes());
        FILE_NAME_LENGTH_SIZE as i32
    }
}

impl Device for FileImporter {
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        let argument_error = match operation {
            operation::FILE_SIZE | operation::FILE_NAME | operation::FILE_NAME_LENGTH
                if !arguments.is_empty() =>
            {
                Some(device_status::INVALID_ARGUMENT)
            }
            operation::READ_BLOCK if arguments.len() < BLOCK_INDEX_SIZE => {
                Some(device_status::ARGUMENT_OVERRUN)
            }
            operation::READ_BLOCK if arguments.len() != BLOCK_INDEX_SIZE => {
                Some(device_status::INVALID_ARGUMENT)
            }
            _ => None,
        };
        if let Some(error) = argument_error {
            return error;
        }
        if !matches!(
            operation,
            operation::FILE_SIZE
                | operation::READ_BLOCK
                | operation::FILE_NAME
                | operation::FILE_NAME_LENGTH
        ) {
            return device_status::INVALID_OPERATION;
        }
        if self.initialize().is_err() {
            return device_status::DEVICE_BROKEN;
        }
        match operation {
            operation::FILE_SIZE => self.write_file_size(response),
            operation::READ_BLOCK => {
                let index = &arguments[..BLOCK_INDEX_SIZE];
                let block = u32::from_le_bytes(index.try_into().expect("four-byte block index"));
                self.read_block(block, response)
            }
            operation::FILE_NAME => self.write_file_name(response),
            operation::FILE_NAME_LENGTH => self.write_file_name_length(response),
            _ => device_status::INVALID_OPERATION,
        }
    }

    fn interface_id(&self) -> u32 {
        INTERFACE_ID
    }

    fn uuid(&self) -> Uuid {
        self.uuid
    }
}

mod operation {
    pub const FILE_SIZE: u32 = 0x00;
    pub const READ_BLOCK: u32 = 0x01;
    pub const FILE_NAME: u32 = 0x02;
    pub const FILE_NAME_LENGTH: u32 = 0x03;
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{BLOCK_SIZE, FileImporter, INTERFACE_ID, operation};
    use crate::device::{Device, device_status};
    use uuid::Uuid;

    const GCD_WAT: &[u8] = include_bytes!("../../examples/basic/external-files/gcd.wat");

    fn importer() -> FileImporter {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("basic");
        FileImporter::configured(Uuid::new_v4(), &root, Path::new("gcd.wat"))
            .expect("gcd.wat loads from the example programs directory")
    }

    #[test]
    fn exposes_interface_and_file_size() {
        let mut importer = importer();
        let mut response = [0; 8];

        assert_eq!(importer.interface_id(), INTERFACE_ID);
        assert_eq!(
            importer.operate(operation::FILE_SIZE, &[], &mut response),
            8
        );
        assert_eq!(u64::from_le_bytes(response), GCD_WAT.len() as u64);
    }

    #[test]
    fn reports_configured_file_basename_as_ascii_without_terminator() {
        let mut importer = importer();
        let mut length_response = [0; 4];

        assert_eq!(
            importer.operate(operation::FILE_NAME_LENGTH, &[], &mut length_response),
            4
        );
        let name_length = u32::from_le_bytes(length_response) as usize;
        assert_eq!(name_length, 7);

        let mut response = vec![b'X'; name_length + 1];

        assert_eq!(
            importer.operate(operation::FILE_NAME, &[], &mut response),
            name_length as i32
        );
        assert_eq!(&response[..name_length], b"gcd.wat");
        assert_eq!(response[name_length], b'X');
        assert_eq!(
            importer.operate(operation::FILE_NAME, &[1], &mut response),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            importer.operate(operation::FILE_NAME, &[], &mut [0; 6]),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            importer.operate(operation::FILE_NAME_LENGTH, &[1], &mut length_response),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            importer.operate(operation::FILE_NAME_LENGTH, &[], &mut [0; 3]),
            device_status::RESPONSE_OVERRUN
        );
    }

    #[test]
    fn rejects_non_ascii_file_name_on_first_valid_operation() {
        let mut importer = FileImporter::configured(
            Uuid::new_v4(),
            Path::new("unused"),
            Path::new("caf\u{e9}.wat"),
        )
        .expect("configuration defers source initialization");
        assert_eq!(
            importer.operate(operation::FILE_SIZE, &[], &mut [0; 8]),
            device_status::DEVICE_BROKEN
        );
    }
    #[test]
    fn reads_file_in_fixed_size_blocks_and_returns_partial_final_block() {
        let mut importer = importer();
        let block_count = GCD_WAT.len().div_ceil(BLOCK_SIZE);
        let mut actual = Vec::new();

        for block in 0..block_count {
            let mut response = [0; BLOCK_SIZE];
            let written = importer.operate(
                operation::READ_BLOCK,
                &(block as u32).to_le_bytes(),
                &mut response,
            );
            assert!(written > 0);
            actual.extend_from_slice(&response[..usize::try_from(written).unwrap()]);
        }

        assert_eq!(actual, GCD_WAT);
    }

    #[test]
    fn reports_invalid_ranges_and_rejects_paths_outside_program_fixtures() {
        let mut importer = importer();
        assert_eq!(
            importer.operate(operation::FILE_SIZE, &[], &mut [0; 7]),
            device_status::RESPONSE_OVERRUN
        );
        assert_eq!(
            importer.operate(operation::READ_BLOCK, &[], &mut [0; BLOCK_SIZE]),
            device_status::ARGUMENT_OVERRUN
        );
        assert_eq!(
            importer.operate(
                operation::READ_BLOCK,
                &[0, 0, 0, 0, 0],
                &mut [0; BLOCK_SIZE]
            ),
            device_status::INVALID_ARGUMENT
        );
        assert_eq!(
            importer.operate(
                operation::READ_BLOCK,
                &u32::MAX.to_le_bytes(),
                &mut [0; BLOCK_SIZE]
            ),
            device_status::OUT_OF_BOUNDS
        );
        assert_eq!(
            FileImporter::configured(
                Uuid::new_v4(),
                &PathBuf::from(env!("CARGO_MANIFEST_DIR")),
                Path::new("../data/disks/file"),
            )
            .err()
            .expect("parent traversal is rejected")
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn source_is_initialized_lazily_after_argument_validation() {
        let id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("openwac-importer-lazy-{id}"));
        let mut importer = FileImporter::configured(id, &root, Path::new("late.wat"))
            .expect("configuration does not open the source yet");
        assert_eq!(
            importer.operate(operation::FILE_SIZE, &[1], &mut [0; 8]),
            device_status::INVALID_ARGUMENT
        );
        std::fs::create_dir_all(root.join("external-files")).expect("source directory is created");
        std::fs::write(root.join("external-files/late.wat"), b"later")
            .expect("source is created after importer configuration");
        let mut response = [0; 8];
        assert_eq!(
            importer.operate(operation::FILE_SIZE, &[], &mut response),
            8
        );
        assert_eq!(u64::from_le_bytes(response), 5);
        std::fs::remove_dir_all(root).expect("temporary importer root is removed");
    }
}

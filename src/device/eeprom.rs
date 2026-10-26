use std::fs;
use std::path::{Component, Path, PathBuf};

use uuid::Uuid;

use crate::device::{Device, device_status};

const MAX_IMAGE_BYTES: usize = 1024 * 1024;

pub(crate) struct Eeprom {
    uuid: Uuid,
    path: PathBuf,
    image: Option<Vec<u8>>,
    attempted: bool,
}

impl Eeprom {
    pub(crate) fn from_image(uuid: Uuid, image: Vec<u8>) -> Result<Self, String> {
        if image.len() > MAX_IMAGE_BYTES {
            return Err("EEPROM firmware image exceeds 1 MiB".into());
        }
        Ok(Self {
            uuid,
            path: PathBuf::new(),
            image: Some(image),
            attempted: true,
        })
    }

    pub(crate) fn configured(uuid: Uuid, runtime_root: &Path, relative: &Path) -> Self {
        Self {
            uuid,
            path: runtime_root.join("firmware").join(relative),
            image: None,
            attempted: false,
        }
    }
    fn image(&mut self) -> Result<&[u8], i32> {
        if !self.attempted {
            self.attempted = true;
            let loaded = fs::read(&self.path).map_err(|_| device_status::DEVICE_BROKEN)?;
            if loaded.len() > MAX_IMAGE_BYTES {
                return Err(device_status::DEVICE_BROKEN);
            }
            self.image = Some(loaded);
        }
        self.image.as_deref().ok_or(device_status::DEVICE_BROKEN)
    }
}

impl Device for Eeprom {
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        match operation {
            0 => {
                if !arguments.is_empty() || response.len() < 12 {
                    return device_status::INVALID_ARGUMENT;
                }
                let len = match self.image() {
                    Ok(bytes) => bytes.len() as u32,
                    Err(error) => return error,
                };
                response[..12].copy_from_slice(
                    &[
                        len.to_le_bytes(),
                        (MAX_IMAGE_BYTES as u32).to_le_bytes(),
                        1_u32.to_le_bytes(),
                    ]
                    .concat(),
                );
                12
            }
            1 => {
                if arguments.len() != 8 {
                    return device_status::INVALID_ARGUMENT;
                }
                let offset = u32::from_le_bytes(arguments[..4].try_into().unwrap());
                let length = u32::from_le_bytes(arguments[4..8].try_into().unwrap());
                if offset > i32::MAX as u32
                    || length > i32::MAX as u32
                    || length as usize > response.len()
                {
                    return device_status::INVALID_ARGUMENT;
                }
                let image = match self.image() {
                    Ok(bytes) => bytes,
                    Err(error) => return error,
                };
                let end = match (offset as usize).checked_add(length as usize) {
                    Some(end) if end <= image.len() => end,
                    _ => return device_status::INVALID_ARGUMENT,
                };
                response[..length as usize].copy_from_slice(&image[offset as usize..end]);
                length as i32
            }
            _ => device_status::INVALID_ARGUMENT,
        }
    }
    fn interface_id(&self) -> u32 {
        0x0001
    }
    fn uuid(&self) -> Uuid {
        self.uuid
    }
}

pub(crate) fn validate_path(path: &Path) -> Result<(), String> {
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("EEPROM path must be relative to the runtime firmware directory".into());
    }
    Ok(())
}

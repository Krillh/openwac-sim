use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use minifb::{Scale, Window, WindowOptions};
use uuid::Uuid;

use crate::device::{Device, device_status};

pub(crate) const INTERFACE_ID: u32 = 0x0400;
pub(crate) const COLUMNS: usize = 60;
pub(crate) const ROWS: usize = 30;
const CELL_WIDTH: usize = 9;
const CELL_HEIGHT: usize = 16;
const ARG_RECT_SIZE: usize = 16;
const PIXEL_WIDTH: usize = COLUMNS * CELL_WIDTH;
const PIXEL_HEIGHT: usize = ROWS * CELL_HEIGHT;
const FRAME_BYTES: usize = PIXEL_WIDTH * PIXEL_HEIGHT;
const ATLAS: &[u8] = include_bytes!("../../_docs/Codepage-437-fixed.png");

mod operation {
    pub const GEOMETRY: u32 = 0x00;
    pub const READ_CELLS: u32 = 0x10;
    pub const WRITE_CELLS: u32 = 0x20;
    pub const FILL: u32 = 0x21;
    pub const COPY: u32 = 0x22;
    pub const SET_CURSOR: u32 = 0x30;
    pub const GET_CURSOR: u32 = 0x31;
}

#[derive(Clone, Copy)]
struct CursorState {
    x: usize,
    y: usize,
    visible: bool,
    blink: bool,
}

struct FrameUpdate {
    cells: Vec<u8>,
    cursor: CursorState,
}

pub(crate) struct Ttd {
    uuid: Uuid,
    cells: Vec<u8>,
    cursor: CursorState,
    window: Option<SyncSender<FrameUpdate>>,
    window_available: Option<Arc<AtomicBool>>,
}

impl Ttd {
    pub(crate) fn new(
        uuid: Uuid,
        headless: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (window, window_available) = if headless {
            (None, None)
        } else {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (window_tx, window_rx) = mpsc::sync_channel(1);
            let available = Arc::new(AtomicBool::new(true));
            let thread_available = Arc::clone(&available);
            thread::Builder::new()
                .name(format!("ttd-{uuid}"))
                .spawn(move || run_window(window_rx, ready_tx, thread_available))?;
            ready_rx
                .recv()
                .map_err(|_| "TTD window thread stopped during initialization")??;
            (Some(window_tx), Some(available))
        };
        let cells = vec![b' '; COLUMNS * ROWS];
        let cursor = CursorState {
            x: 0,
            y: 0,
            visible: false,
            blink: false,
        };
        if let Some(window) = &window {
            window
                .send(FrameUpdate {
                    cells: cells.clone(),
                    cursor,
                })
                .map_err(|_| "TTD window closed during initialization")?;
        }

        Ok(Self {
            uuid,
            cells,
            cursor,
            window,
            window_available,
        })
    }

    fn update_window(&self) -> i32 {
        let Some(window) = &self.window else {
            return device_status::OK;
        };
        match window.try_send(FrameUpdate {
            cells: self.cells.clone(),
            cursor: self.cursor,
        }) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => device_status::OK,
            Err(mpsc::TrySendError::Disconnected(_)) => device_status::DEVICE_NOT_AVAILABLE,
        }
    }

    fn rectangle(arguments: &[u8]) -> Result<(usize, usize, usize, usize, usize), i32> {
        if arguments.len() < ARG_RECT_SIZE {
            return Err(device_status::ARGUMENT_OVERRUN);
        }
        let x = read_u32(arguments, 0)? as usize;
        let y = read_u32(arguments, 4)? as usize;
        let width = read_u32(arguments, 8)? as usize;
        let height = read_u32(arguments, 12)? as usize;
        if width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|right| right > COLUMNS)
            || y.checked_add(height).is_none_or(|bottom| bottom > ROWS)
        {
            return Err(device_status::OUT_OF_BOUNDS);
        }
        let length = width
            .checked_mul(height)
            .ok_or(device_status::INVALID_ARGUMENT)?;
        Ok((x, y, width, height, length))
    }

    fn read_cells(&self, arguments: &[u8], response: &mut [u8]) -> i32 {
        let (x, y, width, height, length) = match Self::rectangle(arguments) {
            Ok(rectangle) => rectangle,
            Err(error) => return error,
        };
        if arguments.len() != ARG_RECT_SIZE {
            return device_status::INVALID_ARGUMENT;
        }
        if response.len() < length {
            return device_status::RESPONSE_OVERRUN;
        }
        let mut out = 0;
        for row in y..y + height {
            let start = row * COLUMNS + x;
            response[out..out + width].copy_from_slice(&self.cells[start..start + width]);
            out += width;
        }
        i32::try_from(length).unwrap_or(device_status::INVALID_ARGUMENT)
    }

    fn write_cells(&mut self, arguments: &[u8]) -> i32 {
        let (x, y, width, height, length) = match Self::rectangle(arguments) {
            Ok(rectangle) => rectangle,
            Err(error) => return error,
        };
        if arguments.len() < ARG_RECT_SIZE + length {
            return device_status::ARGUMENT_OVERRUN;
        }
        if arguments.len() != ARG_RECT_SIZE + length {
            return device_status::INVALID_ARGUMENT;
        }
        let mut input = ARG_RECT_SIZE;
        for row in y..y + height {
            let start = row * COLUMNS + x;
            self.cells[start..start + width].copy_from_slice(&arguments[input..input + width]);
            input += width;
        }
        self.update_window()
    }

    fn fill(&mut self, arguments: &[u8]) -> i32 {
        let (x, y, width, height, _) = match Self::rectangle(arguments) {
            Ok(rectangle) => rectangle,
            Err(error) => return error,
        };
        if arguments.len() < ARG_RECT_SIZE + 1 {
            return device_status::ARGUMENT_OVERRUN;
        }
        if arguments.len() != ARG_RECT_SIZE + 1 {
            return device_status::INVALID_ARGUMENT;
        }
        for row in y..y + height {
            let start = row * COLUMNS + x;
            self.cells[start..start + width].fill(arguments[ARG_RECT_SIZE]);
        }
        self.update_window()
    }

    fn copy(&mut self, arguments: &[u8]) -> i32 {
        let (x, y, width, height, length) = match Self::rectangle(arguments) {
            Ok(rectangle) => rectangle,
            Err(error) => return error,
        };
        if arguments.len() < 24 {
            return device_status::ARGUMENT_OVERRUN;
        }
        if arguments.len() != 24 {
            return device_status::INVALID_ARGUMENT;
        }
        let dest_x = match read_u32(arguments, 16) {
            Ok(value) => value as usize,
            Err(error) => return error,
        };
        let dest_y = match read_u32(arguments, 20) {
            Ok(value) => value as usize,
            Err(error) => return error,
        };
        if dest_x
            .checked_add(width)
            .is_none_or(|right| right > COLUMNS)
            || dest_y
                .checked_add(height)
                .is_none_or(|bottom| bottom > ROWS)
        {
            return device_status::OUT_OF_BOUNDS;
        }
        let mut snapshot = Vec::with_capacity(length);
        for row in y..y + height {
            let start = row * COLUMNS + x;
            snapshot.extend_from_slice(&self.cells[start..start + width]);
        }
        let mut input = 0;
        for row in dest_y..dest_y + height {
            let start = row * COLUMNS + dest_x;
            self.cells[start..start + width].copy_from_slice(&snapshot[input..input + width]);
            input += width;
        }
        self.update_window()
    }

    fn set_cursor(&mut self, arguments: &[u8]) -> i32 {
        if arguments.len() < 10 {
            return device_status::ARGUMENT_OVERRUN;
        }
        if arguments.len() != 10 || arguments[8] > 1 || arguments[9] > 1 {
            return device_status::INVALID_ARGUMENT;
        }
        let x = match read_u32(arguments, 0) {
            Ok(value) => value as usize,
            Err(error) => return error,
        };
        let y = match read_u32(arguments, 4) {
            Ok(value) => value as usize,
            Err(error) => return error,
        };
        if x >= COLUMNS || y >= ROWS {
            return device_status::OUT_OF_BOUNDS;
        }
        self.cursor = CursorState {
            x,
            y,
            visible: arguments[8] != 0,
            blink: arguments[9] != 0,
        };
        self.update_window()
    }
}

impl Device for Ttd {
    fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
        if self
            .window_available
            .as_ref()
            .is_some_and(|available| !available.load(Ordering::Acquire))
        {
            return device_status::DEVICE_NOT_AVAILABLE;
        }
        match operation {
            operation::GEOMETRY => {
                if !arguments.is_empty() {
                    return device_status::INVALID_ARGUMENT;
                }
                if response.len() < 8 {
                    return device_status::RESPONSE_OVERRUN;
                }
                response[..4].copy_from_slice(&(COLUMNS as u32).to_le_bytes());
                response[4..8].copy_from_slice(&(ROWS as u32).to_le_bytes());
                8
            }
            operation::READ_CELLS => self.read_cells(arguments, response),
            operation::WRITE_CELLS => self.write_cells(arguments),
            operation::FILL => self.fill(arguments),
            operation::COPY => self.copy(arguments),
            operation::SET_CURSOR => self.set_cursor(arguments),
            operation::GET_CURSOR => {
                if !arguments.is_empty() {
                    return device_status::INVALID_ARGUMENT;
                }
                if response.len() < 10 {
                    return device_status::RESPONSE_OVERRUN;
                }
                response[..4].copy_from_slice(&(self.cursor.x as u32).to_le_bytes());
                response[4..8].copy_from_slice(&(self.cursor.y as u32).to_le_bytes());
                response[8] = u8::from(self.cursor.visible);
                response[9] = u8::from(self.cursor.blink);
                10
            }
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

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, i32> {
    let field = bytes
        .get(offset..offset + 4)
        .ok_or(device_status::ARGUMENT_OVERRUN)?;
    Ok(u32::from_le_bytes(
        field.try_into().expect("four-byte slice"),
    ))
}

fn decode_atlas() -> Result<Vec<u8>, String> {
    let mut decoder = png::Decoder::new(Cursor::new(ATLAS));
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    if reader.info().width != 288 || reader.info().height != 128 {
        return Err("CP437 atlas must be 288 by 128 pixels".into());
    }
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut data)
        .map_err(|error| error.to_string())?;
    if info.color_type != png::ColorType::Rgba {
        return Err("CP437 atlas must decode to RGBA".into());
    }
    data.truncate(info.buffer_size());
    let mut alpha = Vec::with_capacity(288 * 128);
    for pixel in data.chunks_exact(4) {
        alpha.push(pixel[3]);
    }
    Ok(alpha)
}

fn run_window(
    receiver: Receiver<FrameUpdate>,
    ready: SyncSender<Result<(), String>>,
    available: Arc<AtomicBool>,
) {
    let atlas = match decode_atlas() {
        Ok(atlas) => atlas,
        Err(error) => {
            available.store(false, Ordering::Release);
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut window = match Window::new(
        "OpenWAC Tiny Text Display",
        PIXEL_WIDTH,
        PIXEL_HEIGHT,
        WindowOptions {
            resize: false,
            scale: Scale::X2,
            ..WindowOptions::default()
        },
    ) {
        Ok(window) => window,
        Err(error) => {
            available.store(false, Ordering::Release);
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    window.set_target_fps(60);
    if ready.send(Ok(())).is_err() {
        return;
    }
    let mut pixels = vec![0_u32; FRAME_BYTES];
    let mut latest = None;
    let mut current = None;
    let mut last_blink_phase = false;
    while window.is_open() {
        loop {
            match receiver.try_recv() {
                Ok(update) => latest = Some(update),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    available.store(false, Ordering::Release);
                    return;
                }
            }
        }
        let updated = if let Some(update) = latest.take() {
            current = Some(update);
            true
        } else {
            false
        };
        let blink_phase = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            / 500)
            % 2
            == 0;
        let blink_changed = current.as_ref().is_some_and(|frame: &FrameUpdate| {
            frame.cursor.visible && frame.cursor.blink && blink_phase != last_blink_phase
        });
        if (updated || blink_changed)
            && let Some(update) = current.as_ref()
        {
            draw_cells(&mut pixels, &atlas, update);
        }
        last_blink_phase = blink_phase;
        if window
            .update_with_buffer(&pixels, PIXEL_WIDTH, PIXEL_HEIGHT)
            .is_err()
        {
            available.store(false, Ordering::Release);
            return;
        }
        thread::sleep(Duration::from_millis(4));
    }
    available.store(false, Ordering::Release);
}

fn draw_cells(pixels: &mut [u32], atlas: &[u8], update: &FrameUpdate) {
    pixels.fill(0x000000);
    for (index, character) in update.cells.iter().copied().enumerate() {
        let cell_x = index % COLUMNS;
        let cell_y = index / COLUMNS;
        let source_x = usize::from(character % 32) * CELL_WIDTH;
        let source_y = usize::from(character / 32) * CELL_HEIGHT;
        for y in 0..CELL_HEIGHT {
            for x in 0..CELL_WIDTH {
                let alpha = u32::from(atlas[(source_y + y) * 288 + source_x + x]);
                if alpha != 0 {
                    let destination =
                        (cell_y * CELL_HEIGHT + y) * PIXEL_WIDTH + cell_x * CELL_WIDTH + x;
                    let component = (255 * alpha / 255) & 0xff;
                    pixels[destination] = (component << 16) | (component << 8) | component;
                }
            }
        }
    }
    if update.cursor.visible {
        let blink_on = !update.cursor.blink
            || (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 500)
                % 2
                == 0;
        if blink_on {
            let x = update.cursor.x * CELL_WIDTH;
            let y = update.cursor.y * CELL_HEIGHT + CELL_HEIGHT - 2;
            pixels[y * PIXEL_WIDTH + x..y * PIXEL_WIDTH + x + CELL_WIDTH].fill(0x00ff_ffff);
        }
    }
}

#ifndef OPENWAC_HAL_H
#define OPENWAC_HAL_H

#include <stdint.h>

/*
 * Guest-side declarations for the imports implemented by openwac/src.
 * When compiling with Clang for WebAssembly, each declaration is imported
 * from the host module named "hal". Unused declarations do not create imports.
 */
#if defined(__wasm__) && defined(__clang__)
#define HAL_IMPORT(name) \
    __attribute__((import_module("hal"), import_name(#name))) name
#define HAL_EXPORT_START __attribute__((export_name("_start"))) void _start(void)
#else
#define HAL_IMPORT(name) name
#define HAL_EXPORT_START void start(void)
#endif

#ifdef __cplusplus
extern "C" {
#endif

typedef uint8_t hal_uuid_t[16];

/* Shared status codes. Device interfaces use the device-level subset. */
enum hal_status {
    HAL_OK = 0,
    HAL_UNKNOWN_ERROR = -1,
    HAL_INVALID_OPERATION = -2,
    HAL_INVALID_ARGUMENT = -3,
    HAL_ARGUMENT_OVERRUN = -4,
    HAL_RESPONSE_OVERRUN = -5,
    HAL_DEVICE_BROKEN = -6,
    HAL_DEVICE_NOT_AVAILABLE = -7,
    HAL_POINTER_INVALID = -8,
    HAL_MEMORY_INVALID = -9,
    HAL_OUT_OF_BOUNDS = -10,
    HAL_SYSCALL_TOO_EXPENSIVE = -11,
    HAL_PERMISSION_DENIED = -12,
    HAL_KERNEL_NOT_INSTALLED = -13,
    HAL_KERNEL_PANICKED = -14,
    HAL_KERNEL_ALREADY_INSTALLED = -15,
    HAL_KERNEL_INVALID = -16
};

enum hal_interface_id {
    HAL_INTERFACE_DISK = 0x0100,
    HAL_INTERFACE_FILE_IMPORTER = 0x0200,
    HAL_INTERFACE_SERIAL_MONITOR = 0x0300
};

enum hal_disk_operation {
    HAL_DISK_IS_ROOT = 0x00,
    HAL_DISK_SET_ROOT = 0x01,
    HAL_DISK_BLOCK_SIZE = 0x10,
    HAL_DISK_BLOCK_COUNT = 0x11,
    HAL_DISK_READ_BLOCK = 0x20,
    HAL_DISK_WRITE_BLOCK = 0x30
};

enum hal_file_importer_operation {
    HAL_FILE_SIZE = 0x00,
    HAL_FILE_READ_BLOCK = 0x01,
    HAL_FILE_NAME = 0x02,
    HAL_FILE_NAME_LENGTH = 0x03
};

enum hal_serial_monitor_operation {
    HAL_SERIAL_WRITE = 0x00,
    HAL_SERIAL_READ = 0x01,
    HAL_SERIAL_PEEK = 0x02,
    HAL_SERIAL_AVAILABLE = 0x03
};

/* Counts the calling computer's hardware operations. */
void HAL_IMPORT(increment)(void);

/* Available to privileged and unprivileged processes. */
void HAL_IMPORT(hal_yield)(void);

/* The remaining HAL imports require a privileged process. */
int32_t HAL_IMPORT(hal_device_count)(void);
int32_t HAL_IMPORT(hal_device_info)(
    int32_t index,
    void *output,
    int32_t output_capacity);
int32_t HAL_IMPORT(hal_device_call)(
    const uint8_t device_uuid[16],
    int32_t operation,
    const void *arguments,
    int32_t arguments_len,
    void *response,
    int32_t response_capacity);
int32_t HAL_IMPORT(hal_poll_event)(void *output, int32_t output_capacity);
int64_t HAL_IMPORT(hal_spawn_process)(const void *wasm, int32_t wasm_len);
int64_t HAL_IMPORT(hal_spawn_process_privileged)(
    const void *wasm,
    int32_t wasm_len);
int32_t HAL_IMPORT(hal_kill_process)(int64_t pid);
int32_t HAL_IMPORT(hal_install_kernel)(const void *wasm, int32_t wasm_len);
int32_t HAL_IMPORT(hal_wake)(int64_t pid);

/*
 * The host registers this import only in debug builds. Define DEBUG when
 * compiling the guest to expose its declaration here as well.
 */
#if defined(DEBUG)
int32_t HAL_IMPORT(hal_DEBUG_PRINT)(const void *buffer, int32_t buffer_size);
#endif

#ifdef __cplusplus
}
#endif

#endif /* OPENWAC_HAL_H */

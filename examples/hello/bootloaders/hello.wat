(module
  (import "hal" "hal_chat_print" (func $print (param i32 i32) (result i32)))
  (import "hal" "hal_ram_used_pages" (func $ram (result i64)))
  (import "hal" "hal_device_count" (func $count (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "OpenWAC simulator ready!")
  (func (export "_start")
    ;; The running BIOS is the only allocated memory and the only device.
    call $ram i64.const 1 i64.ne if unreachable end
    call $count i32.const 1 i32.ne if unreachable end
    i32.const 0 i32.const 24 call $print if unreachable end))

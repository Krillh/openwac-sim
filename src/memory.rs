use std::sync::{Arc, Mutex, OnceLock};

use wasmtime::{ResourceLimiter, Result as WasmResult};

const SERVER_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// Shared linear-memory accounting for one simulated computer.
pub(crate) struct MemoryBudget {
    capacity: usize,
    used: Mutex<usize>,
}

impl MemoryBudget {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: Mutex::new(0),
        }
    }

    fn reserve(&self, bytes: usize) -> bool {
        let Ok(mut used) = self.used.lock() else {
            return false;
        };
        if bytes > self.capacity.saturating_sub(*used) {
            return false;
        }
        *used += bytes;
        true
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.lock().expect("memory budget lock poisoned");
        assert!(bytes <= *used, "invalid memory budget release");
        *used -= bytes;
    }

    pub(crate) fn used_bytes(&self) -> usize {
        *self.used.lock().expect("memory budget lock poisoned")
    }
}

fn server_budget() -> Arc<MemoryBudget> {
    static SERVER: OnceLock<Arc<MemoryBudget>> = OnceLock::new();
    Arc::clone(SERVER.get_or_init(|| Arc::new(MemoryBudget::new(SERVER_MEMORY_BYTES))))
}

/// Per-store limiter. Wasmtime reports linear-memory sizes in bytes; Wasm
/// guarantees those sizes are multiples of the 64 KiB page size.
pub(crate) struct MemoryLimiter {
    computer: Arc<MemoryBudget>,
    server: Arc<MemoryBudget>,
    max_bytes: usize,
    reserved: usize,
    pending_growth: usize,
}

impl MemoryLimiter {
    pub(crate) fn new(computer: Arc<MemoryBudget>, max_bytes: usize) -> Self {
        Self {
            computer,
            server: server_budget(),
            max_bytes,
            reserved: 0,
            pending_growth: 0,
        }
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> WasmResult<bool> {
        // A successful growth leaves its reservation in `reserved`; the
        // failure callback applies only to the current growth attempt. Clear
        // the marker before evaluating this attempt so a denied request can
        // never make a later failure release an earlier successful growth.
        self.pending_growth = 0;
        if desired > self.max_bytes || maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        let delta = desired.saturating_sub(current);
        if !self.computer.reserve(delta) {
            return Ok(false);
        }
        if !self.server.reserve(delta) {
            self.computer.release(delta);
            return Ok(false);
        }
        self.reserved += delta;
        self.pending_growth = delta;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _error: wasmtime::Error) -> WasmResult<()> {
        let delta = std::mem::take(&mut self.pending_growth);
        if delta != 0 {
            self.server.release(delta);
            self.computer.release(delta);
            self.reserved -= delta;
        }
        Ok(())
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> WasmResult<bool> {
        Ok(true)
    }
}

impl Drop for MemoryLimiter {
    fn drop(&mut self) {
        if self.reserved != 0 {
            self.server.release(self.reserved);
            self.computer.release(self.reserved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryBudget, MemoryLimiter};
    use std::sync::Arc;
    use wasmtime::ResourceLimiter;

    #[test]
    fn memory_limiter_accounts_per_computer_and_releases_on_drop() {
        let budget = Arc::new(MemoryBudget::new(2 * 65_536));
        {
            let mut first = MemoryLimiter::new(Arc::clone(&budget), 65_536);
            assert!(first.memory_growing(0, 65_536, None).unwrap());
            assert_eq!(budget.used_bytes(), 65_536);
            assert!(!first.memory_growing(65_536, 131_072, None).unwrap());
            let mut second = MemoryLimiter::new(Arc::clone(&budget), 65_536);
            assert!(second.memory_growing(0, 65_536, None).unwrap());
            assert_eq!(budget.used_bytes(), 2 * 65_536);
        }
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn failed_later_growth_releases_only_its_reservation() {
        let budget = Arc::new(MemoryBudget::new(2 * 65_536));
        let mut limiter = MemoryLimiter::new(Arc::clone(&budget), 2 * 65_536);
        assert!(limiter.memory_growing(0, 65_536, None).unwrap());
        assert!(limiter.memory_growing(65_536, 131_072, None).unwrap());
        limiter
            .memory_grow_failed(wasmtime::Error::msg("simulated grow failure"))
            .unwrap();
        assert_eq!(budget.used_bytes(), 65_536);
        drop(limiter);
        assert_eq!(budget.used_bytes(), 0);
    }
}

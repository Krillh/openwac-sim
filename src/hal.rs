use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::Notify;
use uuid::Uuid;

use crate::device::Device;
use crate::memory::{MemoryBudget, MemoryLimiter};
use crate::simulation::ProcessId;

pub(crate) struct DeviceTable {
    devices: Mutex<HashMap<Uuid, Box<dyn Device>>>,
    operation_count: AtomicU64,
}

impl Default for DeviceTable {
    fn default() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
            operation_count: AtomicU64::new(0),
        }
    }
}

impl DeviceTable {
    pub(crate) fn record_operation(&self) {
        self.operation_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn operation_count(&self) -> u64 {
        self.operation_count.load(Ordering::Relaxed)
    }

    pub(crate) fn count(&self) -> usize {
        self.devices
            .lock()
            .expect("device table lock poisoned")
            .len()
    }

    pub(crate) fn device_info(&self, index: usize) -> Option<(u32, Uuid)> {
        let devices = self.devices.lock().expect("device table lock poisoned");
        let mut ids: Vec<_> = devices.keys().copied().collect();
        ids.sort_unstable_by(java_uuid_cmp);
        let id = *ids.get(index)?;
        let device = devices.get(&id)?;
        Some((device.interface_id(), device.uuid()))
    }

    pub(crate) fn device_infos(&self) -> Vec<(u32, Uuid)> {
        let devices = self.devices.lock().expect("device table lock poisoned");
        let mut ids: Vec<_> = devices.keys().copied().collect();
        ids.sort_unstable_by(java_uuid_cmp);
        ids.into_iter()
            .filter_map(|id| {
                devices
                    .get(&id)
                    .map(|device| (device.interface_id(), device.uuid()))
            })
            .collect()
    }

    pub(crate) fn find_host_device(&self, name: &str) -> Option<Uuid> {
        self.devices
            .lock()
            .expect("device table lock poisoned")
            .iter()
            .find_map(|(id, device)| (device.host_name() == Some(name)).then_some(*id))
    }

    pub(crate) fn insert(&self, device: Box<dyn Device>) -> Result<Uuid, String> {
        let id = device.uuid();
        let mut devices = self
            .devices
            .lock()
            .map_err(|_| "device table lock poisoned".to_owned())?;
        if devices.contains_key(&id) {
            return Err(format!("device {id} already exists"));
        }
        if devices.len() >= 64 {
            return Err("device limit reached".into());
        }
        devices.insert(id, device);
        Ok(id)
    }

    pub(crate) fn remove(&self, id: Uuid) -> Result<Box<dyn Device>, String> {
        self.devices
            .lock()
            .map_err(|_| "device table lock poisoned".to_owned())?
            .remove(&id)
            .ok_or_else(|| format!("device {id} does not exist"))
    }

    pub(crate) fn with_device_mut<T>(
        &self,
        id: Uuid,
        action: impl FnOnce(&mut dyn Device) -> T,
    ) -> Result<T, ()> {
        let mut devices = self.devices.lock().map_err(|_| ())?;
        let device = devices.get_mut(&id).ok_or(())?;
        Ok(action(device.as_mut()))
    }

    pub(crate) fn operate(
        &self,
        id: Uuid,
        operation: u32,
        arguments: &[u8],
        response: &mut [u8],
    ) -> Result<i32, ()> {
        self.with_device_mut(id, |device| {
            self.record_operation();
            device.operate(operation, arguments, response)
        })
    }
}

fn java_uuid_cmp(left: &Uuid, right: &Uuid) -> std::cmp::Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    i64::from_be_bytes(left[..8].try_into().expect("UUID high bits"))
        .cmp(&i64::from_be_bytes(
            right[..8].try_into().expect("UUID high bits"),
        ))
        .then_with(|| {
            i64::from_be_bytes(left[8..].try_into().expect("UUID low bits")).cmp(
                &i64::from_be_bytes(right[8..].try_into().expect("UUID low bits")),
            )
        })
}

const EVENT_QUEUE_CAPACITY: usize = 64;

#[derive(Default)]
pub(crate) struct Mailbox {
    messages: Mutex<VecDeque<Vec<u8>>>,
}

impl Mailbox {
    #[allow(dead_code)]
    pub(crate) fn send(&self, _recipient: ProcessId, message: Vec<u8>) -> bool {
        let Ok(mut messages) = self.messages.lock() else {
            return false;
        };
        if messages.len() >= EVENT_QUEUE_CAPACITY {
            return false;
        }
        messages.push_back(message);
        true
    }

    #[allow(dead_code)]
    pub(crate) fn receive(&self, _recipient: ProcessId) -> Option<Vec<u8>> {
        self.messages
            .lock()
            .expect("mailbox lock poisoned")
            .pop_front()
    }

    pub(crate) fn receive_with_capacity(
        &self,
        _recipient: ProcessId,
        capacity: usize,
    ) -> Result<Option<Vec<u8>>, ()> {
        let mut messages = self.messages.lock().map_err(|_| ())?;
        let Some(message) = messages.front() else {
            return Ok(None);
        };
        if message.len() > capacity {
            return Err(());
        }
        Ok(messages.pop_front())
    }
}

#[derive(Debug)]
pub(crate) enum ProcessRequest {
    Spawn {
        pid: ProcessId,
        wasm: Vec<u8>,
        privileged: bool,
    },
    Kill(ProcessId),
}

pub(crate) struct ProcessControl {
    next_pid: AtomicU64,
    requests: Mutex<VecDeque<ProcessRequest>>,
}

impl ProcessControl {
    const MAX_PENDING_KILLS: usize = 256;

    pub(crate) fn new() -> Self {
        Self {
            next_pid: AtomicU64::new(1),
            requests: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn allocate_pid(&self) -> Result<ProcessId, String> {
        self.next_pid
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| "process ID space exhausted".to_owned())
    }

    pub(crate) fn enqueue(&self, request: ProcessRequest) -> Result<(), String> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "process request queue lock poisoned".to_owned())?;
        if let ProcessRequest::Kill(pid) = request {
            let pending_kills = requests
                .iter()
                .filter(|queued| matches!(queued, ProcessRequest::Kill(_)))
                .count();
            if pending_kills >= Self::MAX_PENDING_KILLS {
                return Err("pending process kill limit reached".into());
            }
            if requests
                .iter()
                .any(|queued| matches!(queued, ProcessRequest::Kill(existing) if *existing == pid))
            {
                return Ok(());
            }
            requests.push_back(ProcessRequest::Kill(pid));
        } else {
            requests.push_back(request);
        }
        Ok(())
    }

    /// Removes queued kills whose targets currently exist. Spawn requests and
    /// kills for pending or unknown PIDs remain queued for end-of-tick handling.
    pub(crate) fn take_live_kills(
        &self,
        live_pids: &HashSet<ProcessId>,
    ) -> Result<Vec<ProcessId>, String> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "process request queue lock poisoned".to_owned())?;
        let mut retained = VecDeque::with_capacity(requests.len());
        let mut kills = Vec::new();
        for request in requests.drain(..) {
            match request {
                ProcessRequest::Kill(pid) if live_pids.contains(&pid) => kills.push(pid),
                other => retained.push_back(other),
            }
        }
        *requests = retained;
        Ok(kills)
    }

    pub(crate) fn take_requests(&self) -> Result<Vec<ProcessRequest>, String> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "process request queue lock poisoned".to_owned())?;
        Ok(requests.drain(..).collect())
    }

    fn reset(&self) {
        self.next_pid.store(1, Ordering::Release);
        self.requests
            .lock()
            .expect("process request queue lock poisoned")
            .clear();
    }
}

#[derive(Default)]
pub(crate) struct SimClock(AtomicU64);

impl SimClock {
    pub(crate) fn tick(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn advance_to(&self, tick: u64) {
        self.0.store(tick, Ordering::Relaxed);
    }
}

/// Why a process stopped running; passed to the kernel's exit hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitReason {
    Exited = 0,
    Trapped = 1,
    Killed = 2,
}

/// A sticky wake-up flag. A process blocked in a syscall is made runnable again
/// by `hal_wake`; a wake that arrives before the process blocks is not lost.
#[derive(Default)]
pub(crate) struct WakeToken {
    pending: AtomicBool,
}

impl WakeToken {
    pub(crate) fn wake(&self) {
        self.pending.store(true, Ordering::Release);
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Completes once woken. It registers no waker: the scheduler checks
    /// `is_pending` and polls the process again.
    pub(crate) async fn wait(&self) {
        std::future::poll_fn(|_| {
            if self.pending.swap(false, Ordering::AcqRel) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

pub(crate) struct ComputerShared {
    pub(crate) devices: DeviceTable,
    pub(crate) mailbox: Mailbox,
    pub(crate) clock: SimClock,
    pub(crate) process_control: ProcessControl,
    pub(crate) cpu_budget: CpuBudget,
    pub(crate) memory_budget: Arc<MemoryBudget>,
    max_memory_bytes: usize,
    wake_tokens: Mutex<WakeRegistry>,
    exits: Mutex<Vec<(ProcessId, ExitReason)>>,
    exit_notifications_enabled: AtomicBool,
    restart_requested: AtomicBool,
    chat: Mutex<Vec<String>>,
    chat_this_tick: AtomicUsize,
    cpu_time_nanos: AtomicU64,
    process_count: AtomicUsize,
    process_limit: usize,
}

#[derive(Default)]
struct WakeRegistry {
    tokens: HashMap<ProcessId, Arc<WakeToken>>,
    pending_spawns: HashSet<ProcessId>,
    pending_wakes: HashSet<ProcessId>,
}

impl ComputerShared {
    pub(crate) fn new(
        fuel_per_tick: u64,
        quantum_fuel: u64,
        max_processes: usize,
        max_memory_bytes: usize,
    ) -> Self {
        Self {
            devices: DeviceTable::default(),
            mailbox: Mailbox::default(),
            clock: SimClock::default(),
            process_control: ProcessControl::new(),
            cpu_budget: CpuBudget::new(fuel_per_tick, quantum_fuel),
            memory_budget: Arc::new(MemoryBudget::new(max_memory_bytes)),
            max_memory_bytes,
            wake_tokens: Mutex::new(WakeRegistry::default()),
            exits: Mutex::new(Vec::new()),
            exit_notifications_enabled: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            chat: Mutex::new(Vec::new()),
            chat_this_tick: AtomicUsize::new(0),
            cpu_time_nanos: AtomicU64::new(0),
            process_count: AtomicUsize::new(0),
            process_limit: max_processes,
        }
    }

    pub(crate) fn begin_tick(&self) {
        self.cpu_budget.begin_tick();
        self.chat_this_tick.store(0, Ordering::Release);
    }

    pub(crate) fn register_wake_token(&self, pid: ProcessId, token: Arc<WakeToken>) {
        let mut registry = self.wake_tokens.lock().expect("wake token lock poisoned");
        registry.pending_spawns.remove(&pid);
        if registry.pending_wakes.remove(&pid) {
            token.wake();
        }
        registry.tokens.insert(pid, token);
    }

    pub(crate) fn unregister_wake_token(&self, pid: ProcessId) {
        let mut registry = self.wake_tokens.lock().expect("wake token lock poisoned");
        registry.tokens.remove(&pid);
        registry.pending_spawns.remove(&pid);
        registry.pending_wakes.remove(&pid);
    }

    /// Returns false if the process does not exist on this computer.
    pub(crate) fn wake_process(&self, pid: ProcessId) -> bool {
        let mut registry = self.wake_tokens.lock().expect("wake token lock poisoned");
        match registry.tokens.get(&pid) {
            Some(token) => {
                token.wake();
                true
            }
            None if registry.pending_spawns.contains(&pid) => {
                registry.pending_wakes.insert(pid);
                true
            }
            None => false,
        }
    }

    pub(crate) fn mark_pending_spawn(&self, pid: ProcessId) {
        self.wake_tokens
            .lock()
            .expect("wake token lock poisoned")
            .pending_spawns
            .insert(pid);
    }

    pub(crate) fn unmark_pending_spawn(&self, pid: ProcessId) {
        let mut registry = self.wake_tokens.lock().expect("wake token lock poisoned");
        registry.pending_spawns.remove(&pid);
        registry.pending_wakes.remove(&pid);
    }

    pub(crate) fn new_memory_limiter(&self) -> MemoryLimiter {
        MemoryLimiter::new(Arc::clone(&self.memory_budget), self.max_memory_bytes)
    }

    pub(crate) fn memory_used_bytes(&self) -> u64 {
        self.memory_budget.used_bytes() as u64
    }

    pub(crate) fn cpu_time_nanos(&self) -> u64 {
        self.cpu_time_nanos.load(Ordering::Acquire)
    }

    pub(crate) fn set_cpu_time_nanos(&self, elapsed: u64) {
        self.cpu_time_nanos.store(elapsed, Ordering::Release);
    }

    pub(crate) fn record_chat(&self, message: String) -> bool {
        if self
            .chat_this_tick
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 4).then_some(count + 1)
            })
            .is_err()
        {
            return false;
        }
        let mut chat = self.chat.lock().expect("chat queue lock poisoned");
        if chat.len() >= 16 {
            return false;
        }
        chat.push(message);
        true
    }

    pub(crate) fn take_chat(&self) -> Vec<String> {
        std::mem::take(&mut *self.chat.lock().expect("chat queue lock poisoned"))
    }

    pub(crate) fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Release);
    }

    pub(crate) fn take_restart_request(&self) -> bool {
        self.restart_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn restart_requested(&self) -> bool {
        self.restart_requested.load(Ordering::Acquire)
    }

    pub(crate) fn reserve_process_slot(&self) -> bool {
        let queued_notifications = self.exits.lock().expect("exit queue lock poisoned").len();
        self.process_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count.saturating_add(queued_notifications) < self.process_limit)
                    .then_some(count + 1)
            })
            .is_ok()
    }

    pub(crate) fn release_process_slot(&self) {
        let prior = self.process_count.fetch_sub(1, Ordering::AcqRel);
        assert!(prior > 0, "process slot released without reservation");
    }

    pub(crate) fn record_exit(&self, pid: ProcessId, reason: ExitReason) {
        if !self.exit_notifications_enabled.load(Ordering::Acquire) {
            return;
        }
        let mut exits = self.exits.lock().expect("exit queue lock poisoned");
        if self.exit_notifications_enabled.load(Ordering::Acquire)
            && exits.len() < self.process_limit
        {
            exits.push((pid, reason));
        }
    }

    /// Enables kernel exit callbacks once a ready kernel exports a hook.
    /// Disabling callbacks also discards notifications queued for the old
    /// kernel instance.
    pub(crate) fn set_exit_notifications(&self, enabled: bool) {
        self.exit_notifications_enabled
            .store(enabled, Ordering::Release);
        if !enabled {
            self.exits.lock().expect("exit queue lock poisoned").clear();
        }
    }

    pub(crate) fn take_exits(&self) -> Vec<(ProcessId, ExitReason)> {
        std::mem::take(&mut *self.exits.lock().expect("exit queue lock poisoned"))
    }

    pub(crate) fn has_exits(&self) -> bool {
        !self
            .exits
            .lock()
            .expect("exit queue lock poisoned")
            .is_empty()
    }

    pub(crate) fn restore_exits(&self, mut exits: Vec<(ProcessId, ExitReason)>) {
        let mut queue = self.exits.lock().expect("exit queue lock poisoned");
        exits.append(&mut queue);
        *queue = exits;
    }

    pub(crate) fn allocate_process_id(&self) -> Result<ProcessId, String> {
        self.process_control.allocate_pid()
    }

    pub(crate) fn take_process_requests(&self) -> Result<Vec<ProcessRequest>, String> {
        self.process_control.take_requests()
    }

    pub(crate) fn reset_runtime_state(&self) {
        self.process_control.reset();
        self.cpu_time_nanos.store(0, Ordering::Release);
        self.mailbox
            .messages
            .lock()
            .expect("mailbox lock poisoned")
            .clear();
        self.wake_tokens
            .lock()
            .expect("wake token lock poisoned")
            .tokens
            .clear();
        self.wake_tokens
            .lock()
            .expect("wake token lock poisoned")
            .pending_spawns
            .clear();
        self.wake_tokens
            .lock()
            .expect("wake token lock poisoned")
            .pending_wakes
            .clear();
        self.exits.lock().expect("exit queue lock poisoned").clear();
        self.exit_notifications_enabled
            .store(false, Ordering::Release);
        self.process_count.store(0, Ordering::Release);
        self.restart_requested.store(false, Ordering::Release);
        self.chat.lock().expect("chat queue lock poisoned").clear();
    }
}

pub(crate) struct CpuBudget {
    max_fuel: u64,
    quantum_fuel: u64,
    remaining: Mutex<u64>,
    tick_changed: Notify,
}

impl CpuBudget {
    fn new(max_fuel: u64, quantum_fuel: u64) -> Self {
        Self {
            max_fuel,
            quantum_fuel,
            remaining: Mutex::new(0),
            tick_changed: Notify::new(),
        }
    }

    pub(crate) fn begin_tick(&self) {
        *self.remaining.lock().expect("CPU budget lock poisoned") = self.max_fuel;
        self.tick_changed.notify_waiters();
    }

    pub(crate) fn reserve_quantum(&self) -> bool {
        let mut remaining = self.remaining.lock().expect("CPU budget lock poisoned");
        if *remaining < self.quantum_fuel {
            return false;
        }
        *remaining -= self.quantum_fuel;
        true
    }

    pub(crate) fn quantum_fuel(&self) -> u64 {
        self.quantum_fuel
    }

    pub(crate) fn remaining_fuel(&self) -> u64 {
        *self.remaining.lock().expect("CPU budget lock poisoned")
    }

    pub(crate) async fn charge_syscall(&self, cost: u64) -> Result<bool, ()> {
        // A syscall that does not fit in the current tick may use a whole
        // later tick. The scheduler gives a process resuming this charge the
        // tick before reserving another Wasm quantum.
        let chunk_limit = self.max_fuel;
        let mut remaining_cost = cost;
        let mut waited_for_tick = false;
        while remaining_cost > 0 {
            let chunk = remaining_cost.min(chunk_limit);
            loop {
                let changed = self.tick_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut remaining = self.remaining.lock().expect("CPU budget lock poisoned");
                    if *remaining >= chunk {
                        *remaining -= chunk;
                        break;
                    }
                }
                waited_for_tick = true;
                changed.await;
            }
            remaining_cost -= chunk;
        }
        Ok(waited_for_tick)
    }
}

#[derive(Clone)]
pub(crate) struct SyncGate {
    notify: Arc<Notify>,
}

impl SyncGate {
    pub(crate) fn new() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn wait_for_next_tick(&self) -> impl std::future::Future<Output = ()> + Send {
        Arc::clone(&self.notify).notified_owned()
    }

    pub(crate) fn release_for_tick(&self) {
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use uuid::Uuid;

    use super::{
        ComputerShared, CpuBudget, DeviceTable, ExitReason, Mailbox, ProcessControl,
        ProcessRequest, java_uuid_cmp,
    };
    use crate::device::Device;

    struct EchoDevice;

    impl Device for EchoDevice {
        fn operate(&mut self, operation: u32, arguments: &[u8], response: &mut [u8]) -> i32 {
            if operation == 0 {
                return crate::device::device_status::INVALID_OPERATION;
            }
            let copied = arguments.len().min(response.len());
            response[..copied].copy_from_slice(&arguments[..copied]);
            copied as i32
        }

        fn interface_id(&self) -> u32 {
            7
        }

        fn uuid(&self) -> Uuid {
            Uuid::from_u128(7)
        }
    }

    #[test]
    fn device_table_exposes_count_id_and_device_operations() {
        let devices = DeviceTable::default();
        let id = Uuid::from_u128(7);
        assert_eq!(devices.insert(Box::new(EchoDevice)), Ok(Uuid::from_u128(7)));
        assert_eq!(devices.count(), 1);
        assert_eq!(devices.device_info(0), Some((7, id)));

        let mut response = [0; 3];
        let status = devices
            .operate(id, 42, &[1, 2, 3, 4], &mut response)
            .expect("device exists");
        assert_eq!(status, 3);
        assert_eq!(response, [1, 2, 3]);
        let error = devices
            .operate(id, 0, &[], &mut response)
            .expect("device exists");
        assert_eq!(error, crate::device::device_status::INVALID_OPERATION);
    }

    #[test]
    fn syscall_budget_spreads_large_charges_across_ticks() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds");
        let budget = Arc::new(CpuBudget::new(10, 2));

        runtime.block_on(async {
            budget.begin_tick();
            for _ in 0..5 {
                assert!(budget.reserve_quantum());
            }
            assert!(!budget.reserve_quantum());
            let waiting_budget = Arc::clone(&budget);
            let waiter = tokio::spawn(async move { waiting_budget.charge_syscall(9).await });
            tokio::task::yield_now().await;
            budget.begin_tick();
            assert_eq!(waiter.await.expect("waiter joins"), Ok(true));
        });
    }

    #[test]
    fn full_tick_quantum_can_charge_on_a_later_tick() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds");
        let budget = Arc::new(CpuBudget::new(10, 10));

        runtime.block_on(async {
            budget.begin_tick();
            assert!(budget.reserve_quantum());
            let waiting_budget = Arc::clone(&budget);
            let waiter = tokio::spawn(async move { waiting_budget.charge_syscall(1).await });
            tokio::task::yield_now().await;
            budget.begin_tick();
            assert_eq!(waiter.await.expect("waiter joins"), Ok(true));
        });
    }

    #[test]
    fn uuid_enumeration_uses_java_signed_word_order() {
        let positive = Uuid::from_u128(0x7fff_ffff_ffff_ffff_0000_0000_0000_0000);
        let negative = Uuid::from_u128(0x8000_0000_0000_0000_0000_0000_0000_0000);
        assert_eq!(
            java_uuid_cmp(&negative, &positive),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn event_mailbox_is_shared_bounded_and_preserves_oversize_head() {
        let mailbox = Mailbox::default();
        assert!(mailbox.send(11, vec![1, 2, 3]));
        assert!(mailbox.send(12, vec![4]));
        assert_eq!(mailbox.receive_with_capacity(99, 2), Err(()));
        assert_eq!(
            mailbox.receive_with_capacity(99, 3),
            Ok(Some(vec![1, 2, 3]))
        );
        assert_eq!(mailbox.receive(99), Some(vec![4]));

        for sequence in 0..64_u8 {
            assert!(mailbox.send(0, vec![sequence]));
        }
        assert!(!mailbox.send(0, vec![64]));
        assert_eq!(mailbox.receive(7), Some(vec![0]));
        assert!(mailbox.send(7, vec![64]));
        for sequence in 1..=64_u8 {
            assert_eq!(mailbox.receive(999), Some(vec![sequence]));
        }
        assert_eq!(mailbox.receive(0), None);
    }

    #[test]
    fn exit_notifications_are_enabled_bounded_and_consume_process_admission() {
        let shared = ComputerShared::new(100, 10, 2, 2 * 65_536);
        shared.record_exit(1, ExitReason::Exited);
        assert!(
            !shared.has_exits(),
            "callbacks are disabled until kernel ready"
        );
        assert!(shared.reserve_process_slot());
        assert!(shared.reserve_process_slot());
        assert!(!shared.reserve_process_slot());

        shared.set_exit_notifications(true);
        shared.record_exit(1, ExitReason::Exited);
        shared.release_process_slot();
        assert!(shared.has_exits());
        assert!(
            !shared.reserve_process_slot(),
            "a queued callback consumes a bounded process slot"
        );

        shared.record_exit(2, ExitReason::Killed);
        shared.release_process_slot();
        shared.record_exit(3, ExitReason::Trapped);
        assert_eq!(shared.take_exits().len(), 2);
        assert!(shared.reserve_process_slot());
        assert!(shared.reserve_process_slot());
        assert!(!shared.reserve_process_slot());
        shared.release_process_slot();
        shared.release_process_slot();

        shared.set_exit_notifications(false);
        shared.record_exit(4, ExitReason::Exited);
        assert!(!shared.has_exits());
    }

    #[test]
    fn disabling_exit_notifications_discards_old_kernel_callbacks() {
        let shared = ComputerShared::new(100, 10, 1, 65_536);
        assert!(shared.reserve_process_slot());
        shared.set_exit_notifications(true);
        shared.record_exit(1, ExitReason::Exited);
        shared.release_process_slot();
        assert!(shared.has_exits());

        shared.set_exit_notifications(false);
        assert!(!shared.has_exits());
        assert!(shared.reserve_process_slot());
        shared.release_process_slot();
    }

    #[test]
    fn process_control_deduplicates_and_bounds_pending_kills() {
        let control = ProcessControl::new();
        control.enqueue(ProcessRequest::Kill(1)).unwrap();
        control.enqueue(ProcessRequest::Kill(2)).unwrap();
        control.enqueue(ProcessRequest::Kill(2)).unwrap();
        control
            .enqueue(ProcessRequest::Spawn {
                pid: 3,
                wasm: vec![0, 1],
                privileged: false,
            })
            .unwrap();

        let live = [1].into_iter().collect();
        assert_eq!(control.take_live_kills(&live).unwrap(), vec![1]);
        let retained = control.take_requests().unwrap();
        assert_eq!(retained.len(), 2);
        assert!(matches!(retained[0], ProcessRequest::Kill(2)));
        assert!(matches!(retained[1], ProcessRequest::Spawn { pid: 3, .. }));

        for pid in 1..=256 {
            control.enqueue(ProcessRequest::Kill(pid)).unwrap();
        }
        control.enqueue(ProcessRequest::Kill(1)).unwrap();
        assert!(control.enqueue(ProcessRequest::Kill(257)).is_err());
    }
}

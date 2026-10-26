//! Wasm compilation off the controller thread.
//!
//! Compiling a module is the slowest thing the simulator does and costs
//! wall-clock time but no simulated fuel. A `CompileJob` runs it on a worker
//! thread so it can overlap the idle time between ticks. Results are
//! collected at a fixed simulated tick, so timing in the simulation does not
//! depend on how long compilation took on the host.

use std::sync::mpsc::{self, Receiver};

use wasmtime::{Engine, Module};

pub(crate) struct CompileJob {
    result: Receiver<Result<Module, String>>,
}

impl CompileJob {
    pub(crate) fn start(engine: &Engine, wasm: Vec<u8>) -> Self {
        let (sender, result) = mpsc::channel();
        let compile = {
            let engine = engine.clone();
            move || {
                let compiled = crate::validation::validate_module(&wasm)
                    .and_then(|()| Module::new(&engine, &wasm).map_err(|error| error.to_string()));
                // The receiver is gone if the job was abandoned.
                let _ = sender.send(compiled);
            }
        };
        if let Err(error) = std::thread::Builder::new()
            .name("wasm_compile".into())
            .spawn(compile)
        {
            // The closure was consumed by the failed spawn; report the failure
            // to the waiter instead of compiling inline.
            let (sender, failed) = mpsc::channel();
            let _ = sender.send(Err(format!("could not start a compile thread: {error}")));
            return Self { result: failed };
        }
        Self { result }
    }

    /// Blocks until the module is compiled. Returns immediately if the
    /// compile already finished.
    pub(crate) fn wait(self) -> Result<Module, String> {
        self.result
            .recv()
            .unwrap_or_else(|_| Err("compile thread ended without a result".to_owned()))
    }
}

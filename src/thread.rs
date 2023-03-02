#[cfg(not(feature = "wasm"))]
use std::thread;
#[cfg(feature = "wasm")]
use wasm_thread as thread;

pub use thread::*;

//! Bloodhound: a portable time-travel debugger built over a small self-contained
//! stack VM.
//!
//! The crate is split into five layers:
//! - [`vm`]: the stack machine, its opcode set, call frames, linear memory, and
//!   the per-instruction undo journal that makes reverse execution possible.
//! - [`asm`]: a tiny assembler that turns human readable source into a
//!   [`vm::Program`], keeping a source and line table for source-level debugging.
//! - [`debugger`]: breakpoints, watchpoints, stepping (into/over/out) and time
//!   travel (step back, run back, goto step N) layered on top of the VM.
//! - [`expr`]: a tiny safe expression language used for conditional breakpoints
//!   and conditional watchpoints.
//! - [`samples`]: ready to load example programs used by the CLI and the tests.
//!
//! The headline feature is reverse execution. Every executed instruction records
//! a compact undo record, so the debugger can walk backward to any earlier step
//! and reconstruct the exact machine state that forward execution had there.

pub mod asm;
pub mod debugger;
pub mod expr;
pub mod samples;
pub mod vm;

pub use asm::{assemble, AsmError};
pub use debugger::{Debugger, StopReason, WatchHit, WatchLoc};
pub use expr::{EvalCtx, Expr, ExprError};
pub use vm::{Frame, Op, Program, Snapshot, Vm};

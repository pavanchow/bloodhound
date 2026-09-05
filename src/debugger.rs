//! The debugger layer: breakpoints, watchpoints, source-level stepping, and time
//! travel over a [`Vm`].
//!
//! Time travel is implemented by journaling. Each forward instruction pushes its
//! undo record onto a journal; stepping back pops and applies one. Because the VM
//! is deterministic, moving forward past the current point simply re-executes,
//! so [`Debugger::goto`] can reach any step in either direction and reconstruct
//! the exact machine state forward execution had there.

use crate::vm::{Program, Snapshot, Undo, Vm};
use std::collections::BTreeSet;

/// A place the debugger can watch for changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchLoc {
    /// A global slot.
    Global(usize),
    /// A linear memory cell.
    Mem(usize),
    /// A local slot in the current (innermost) frame.
    Local(usize),
}

/// A fired watchpoint: the location, its previous and new value, and the step at
/// which the change happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchHit {
    /// The location that changed.
    pub loc: WatchLoc,
    /// The value before the changing instruction.
    pub old: i64,
    /// The value after the changing instruction.
    pub new: i64,
    /// The step index the change happened at.
    pub step: usize,
}

/// Why a run/step operation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Halted at a breakpoint address (the instruction about to execute).
    Breakpoint(usize),
    /// A watched location changed.
    Watchpoint(WatchHit),
    /// The program halted.
    Halted,
    /// A single step completed.
    Stepped,
    /// Reached a new source line (from a line-level step).
    Line,
    /// Reached the beginning of history (cannot go further back).
    Start,
    /// A safety step limit was hit without otherwise stopping.
    Limit,
}

const STEP_LIMIT: usize = 5_000_000;

/// A time-travel debugger over a single program.
#[derive(Clone, Debug)]
pub struct Debugger {
    /// The loaded program.
    pub program: Program,
    vm: Vm,
    journal: Vec<Undo>,
    breakpoints: BTreeSet<usize>,
    watchpoints: Vec<WatchLoc>,
}

impl Debugger {
    /// Load a program and place the machine at step 0.
    pub fn new(program: Program) -> Self {
        let vm = Vm::new(&program);
        Debugger {
            program,
            vm,
            journal: Vec::new(),
            breakpoints: BTreeSet::new(),
            watchpoints: Vec::new(),
        }
    }

    /// Reset execution to step 0, discarding history. Breakpoints and
    /// watchpoints are preserved.
    pub fn reset(&mut self) {
        self.vm = Vm::new(&self.program);
        self.journal.clear();
    }

    /// The current step index (number of instructions executed from the start).
    pub fn step_count(&self) -> usize {
        self.journal.len()
    }

    /// Immutable access to the underlying VM.
    pub fn vm(&self) -> &Vm {
        &self.vm
    }

    /// Whether the machine has halted.
    pub fn halted(&self) -> bool {
        self.vm.halted
    }

    /// A full comparable snapshot of the current machine state.
    pub fn snapshot(&self) -> Snapshot {
        self.vm.snapshot()
    }

    /// The 1-based source line the machine is about to execute.
    pub fn current_line(&self) -> u32 {
        self.program.line_at(self.vm.pc)
    }

    /// The current program counter.
    pub fn pc(&self) -> usize {
        self.vm.pc
    }

    // --- breakpoints -------------------------------------------------------

    /// Set a breakpoint at an instruction address.
    pub fn add_break(&mut self, addr: usize) {
        self.breakpoints.insert(addr);
    }

    /// Remove a breakpoint at an instruction address. Returns true if present.
    pub fn remove_break(&mut self, addr: usize) -> bool {
        self.breakpoints.remove(&addr)
    }

    /// The set of breakpoint addresses, ascending.
    pub fn breakpoints(&self) -> Vec<usize> {
        self.breakpoints.iter().copied().collect()
    }

    /// The first instruction address that maps to a source line, if any.
    pub fn line_to_addr(&self, line: u32) -> Option<usize> {
        self.program.line_of.iter().position(|&l| l == line)
    }

    /// Set a breakpoint by source line. Returns the resolved address.
    pub fn add_break_line(&mut self, line: u32) -> Option<usize> {
        let addr = self.line_to_addr(line)?;
        self.breakpoints.insert(addr);
        Some(addr)
    }

    // --- watchpoints -------------------------------------------------------

    /// Add a watchpoint. Duplicates are ignored.
    pub fn add_watch(&mut self, loc: WatchLoc) {
        if !self.watchpoints.contains(&loc) {
            self.watchpoints.push(loc);
        }
    }

    /// Remove a watchpoint. Returns true if it was present.
    pub fn remove_watch(&mut self, loc: WatchLoc) -> bool {
        if let Some(i) = self.watchpoints.iter().position(|&w| w == loc) {
            self.watchpoints.remove(i);
            true
        } else {
            false
        }
    }

    /// The current watchpoints.
    pub fn watchpoints(&self) -> &[WatchLoc] {
        &self.watchpoints
    }

    fn read_watch(&self, loc: WatchLoc) -> i64 {
        match loc {
            WatchLoc::Global(i) => self.vm.globals.get(i).copied().unwrap_or(0),
            WatchLoc::Mem(i) => self.vm.memory.get(i).copied().unwrap_or(0),
            WatchLoc::Local(i) => self.vm.local(i),
        }
    }

    fn watch_values(&self) -> Vec<i64> {
        self.watchpoints.iter().map(|&l| self.read_watch(l)).collect()
    }

    fn watch_diff(&self, before: &[i64]) -> Option<WatchHit> {
        for (i, &loc) in self.watchpoints.iter().enumerate() {
            let new = self.read_watch(loc);
            if before.get(i).copied().unwrap_or(new) != new {
                return Some(WatchHit {
                    loc,
                    old: before[i],
                    new,
                    step: self.step_count(),
                });
            }
        }
        None
    }

    // --- primitive motion --------------------------------------------------

    /// Execute one instruction. Returns false if already halted.
    pub fn forward(&mut self) -> bool {
        if self.vm.halted {
            return false;
        }
        match self.vm.step() {
            Some(u) => {
                self.journal.push(u);
                true
            }
            None => false,
        }
    }

    /// Reverse one instruction. Returns false if already at step 0.
    pub fn backward(&mut self) -> bool {
        match self.journal.pop() {
            Some(u) => {
                self.vm.undo(u);
                true
            }
            None => false,
        }
    }

    // --- stepping ----------------------------------------------------------

    /// Step a single instruction, reporting any watchpoint that fired.
    pub fn step_instr(&mut self) -> StopReason {
        if self.vm.halted {
            return StopReason::Halted;
        }
        let pre = self.watch_values();
        self.forward();
        if let Some(hit) = self.watch_diff(&pre) {
            return StopReason::Watchpoint(hit);
        }
        if self.vm.halted {
            StopReason::Halted
        } else {
            StopReason::Stepped
        }
    }

    /// Step into: advance until the source line changes (descending into calls).
    pub fn step_line(&mut self) -> StopReason {
        let start = self.current_line();
        let mut n = 0;
        while n < STEP_LIMIT {
            let pre = self.watch_values();
            if !self.forward() {
                return StopReason::Halted;
            }
            if let Some(hit) = self.watch_diff(&pre) {
                return StopReason::Watchpoint(hit);
            }
            if self.vm.halted {
                return StopReason::Halted;
            }
            if self.current_line() != start {
                return StopReason::Line;
            }
            n += 1;
        }
        StopReason::Limit
    }

    /// Step over: advance to the next source line in the same frame, executing
    /// any calls to completion rather than descending into them.
    pub fn step_over(&mut self) -> StopReason {
        let start_line = self.current_line();
        let start_depth = self.vm.depth();
        let mut n = 0;
        while n < STEP_LIMIT {
            let pre = self.watch_values();
            if !self.forward() {
                return StopReason::Halted;
            }
            if let Some(hit) = self.watch_diff(&pre) {
                return StopReason::Watchpoint(hit);
            }
            if self.vm.halted {
                return StopReason::Halted;
            }
            if self.vm.depth() <= start_depth && self.current_line() != start_line {
                return StopReason::Line;
            }
            n += 1;
        }
        StopReason::Limit
    }

    /// Step out: run until the current frame returns.
    pub fn step_out(&mut self) -> StopReason {
        let start_depth = self.vm.depth();
        let mut n = 0;
        while n < STEP_LIMIT {
            let pre = self.watch_values();
            if !self.forward() {
                return StopReason::Halted;
            }
            if let Some(hit) = self.watch_diff(&pre) {
                return StopReason::Watchpoint(hit);
            }
            if self.vm.halted {
                return StopReason::Halted;
            }
            if self.vm.depth() < start_depth {
                return StopReason::Line;
            }
            n += 1;
        }
        StopReason::Limit
    }

    // --- continue / reverse continue --------------------------------------

    /// Run forward until a breakpoint, a watchpoint change, a halt, or the limit.
    pub fn cont(&mut self) -> StopReason {
        let mut n = 0;
        while n < STEP_LIMIT {
            if self.vm.halted {
                return StopReason::Halted;
            }
            let pre = self.watch_values();
            if !self.forward() {
                return StopReason::Halted;
            }
            if let Some(hit) = self.watch_diff(&pre) {
                return StopReason::Watchpoint(hit);
            }
            if !self.vm.halted && self.breakpoints.contains(&self.vm.pc) {
                return StopReason::Breakpoint(self.vm.pc);
            }
            if self.vm.halted {
                return StopReason::Halted;
            }
            n += 1;
        }
        StopReason::Limit
    }

    /// Run backward until the most recent earlier breakpoint, or the start.
    pub fn run_back(&mut self) -> StopReason {
        while self.backward() {
            if self.breakpoints.contains(&self.vm.pc) {
                return StopReason::Breakpoint(self.vm.pc);
            }
        }
        StopReason::Start
    }

    // --- time travel -------------------------------------------------------

    /// Move to an absolute step index, forward or backward, reconstructing the
    /// exact state forward execution had at that step. Steps beyond the natural
    /// end of the program are clamped at the end.
    pub fn goto(&mut self, n: usize) -> StopReason {
        if n < self.step_count() {
            while self.step_count() > n {
                self.backward();
            }
            StopReason::Stepped
        } else {
            while self.step_count() < n {
                if !self.forward() {
                    return StopReason::Halted;
                }
            }
            StopReason::Stepped
        }
    }

    // --- inspection --------------------------------------------------------

    /// The active call frames, entry frame first.
    pub fn backtrace(&self) -> &[crate::vm::Frame] {
        &self.vm.frames
    }

    /// The locals of the innermost frame.
    pub fn locals(&self) -> &[i64] {
        self.vm
            .frames
            .last()
            .map(|f| f.locals.as_slice())
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::assemble;

    fn dbg(src: &str) -> Debugger {
        Debugger::new(assemble(src).unwrap())
    }

    #[test]
    fn forward_then_back_is_identity_from_any_step() {
        let mut d = dbg("  push 3\n  push 4\n  add\n  storeg 0\n  loadg 0\n  print\n  halt\n");
        let total = {
            let mut c = d.clone();
            let mut n = 0;
            while c.forward() {
                n += 1;
            }
            n
        };
        for start in 0..=total {
            let k = (total - start).min(3);
            if k == 0 {
                continue;
            }
            d.reset();
            d.goto(start);
            let before = d.snapshot();
            for _ in 0..k {
                assert!(d.forward());
            }
            for _ in 0..k {
                assert!(d.backward());
            }
            assert_eq!(d.snapshot(), before, "mismatch at start step {start}");
        }
    }

    #[test]
    fn goto_reconstructs_forward_trace() {
        let mut d = dbg("  push 5\n  push 2\n  mul\n  storeg 1\n  print\n  halt\n");
        let mut trace = Vec::new();
        let mut c = d.clone();
        trace.push(c.snapshot());
        while c.forward() {
            trace.push(c.snapshot());
        }
        for (n, want) in trace.iter().enumerate() {
            d.reset();
            d.goto(n);
            assert_eq!(&d.snapshot(), want, "goto({n})");
        }
    }

    #[test]
    fn watchpoint_reports_old_and_new() {
        let mut d = dbg("  push 11\n  storeg 0\n  push 22\n  storeg 0\n  halt\n");
        d.add_watch(WatchLoc::Global(0));
        let r1 = d.cont();
        assert_eq!(
            r1,
            StopReason::Watchpoint(WatchHit {
                loc: WatchLoc::Global(0),
                old: 0,
                new: 11,
                step: 2
            })
        );
        let r2 = d.cont();
        assert_eq!(
            r2,
            StopReason::Watchpoint(WatchHit {
                loc: WatchLoc::Global(0),
                old: 11,
                new: 22,
                step: 4
            })
        );
    }

    #[test]
    fn step_over_skips_calls() {
        // line 1 call, line 2 print, callee on later lines
        let src = "  call helper 1 1\n  print\n  halt\nhelper:\n  load 0\n  dup\n  add\n  ret\n";
        // push arg first
        let src = format!("  push 8\n{src}");
        let mut d = dbg(&src);
        d.step_over(); // over the push -> next line still line-based
        // walk to the call line, then step over it
        // Simplest: reset and drive explicitly.
        d.reset();
        d.step_instr(); // execute push (line 1)
        let depth_before = d.vm().depth();
        d.step_over(); // over the call (line 2) -> should land back at depth, next line
        assert_eq!(d.vm().depth(), depth_before);
    }
}

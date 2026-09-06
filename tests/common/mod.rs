//! Shared helpers for the correctness gate and the stress suite.
//!
//! The generators here are deterministic: the same seed always produces the
//! same program, so failures are reproducible from the seed alone.

// Generator variables use single letters that match the math they mirror.
#![allow(clippy::many_single_char_names)]

#![allow(dead_code)]

use bloodhound::asm::assemble;
use bloodhound::debugger::{Debugger, StopReason, WatchHit, WatchLoc};
use bloodhound::vm::{Program, Snapshot, Vm};

/// One entry of the ground-truth trace.
pub struct Step {
    pub pc: usize,
    pub line: u32,
    pub depth: usize,
    pub halted: bool,
    pub snap: Snapshot,
}

pub const TRACE_CAP: usize = 2_000_000;

/// The default number of random programs the gate uses.
pub fn fuzz_count() -> u64 {
    std::env::var("BLOODHOUND_FUZZ_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48)
}

/// The iteration count the stress suite gives to its looping generator. The
/// default keeps the default suite in seconds, max scale is 110000.
pub fn stress_iters() -> u64 {
    std::env::var("BLOODHOUND_STRESS_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400)
}

pub struct Rng(pub u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
    }
    pub fn next(&mut self) -> u64 {
        // xorshift64star
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A uniform value in 0..n. The result is below `n` by construction, so the
    /// narrowing cast to usize cannot lose information.
    #[allow(clippy::cast_possible_truncation)]
    pub fn range(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    #[allow(dead_code)]
    pub fn small(&mut self) -> i64 {
        // The modulo result is 0..=18, so the cast to i64 is exact.
        #[allow(clippy::cast_possible_wrap)]
        let v = (self.next() % 19) as i64;
        v - 9
    }
}

/// Generate a well-formed, always-terminating program that exercises the full
/// arithmetic set including division and remainder by zero, negation, locals,
/// globals, memory writes and wrapped loads through negative addresses, a
/// conditional branch in both polarities, nested calls (depth 3), and prints.
pub fn gen_program(seed: u64) -> Program {
    let mut r = Rng::new(seed);
    let ops = ["add", "sub", "mul", "div", "mod", "neg"];
    let cmps = ["lt", "gt", "le", "ge", "eq", "ne"];
    let brs = ["jz", "jnz"];
    let a = r.small();
    let b = r.small();
    let c = r.small();
    let d = r.small();
    // The modulo result is 0..=16, so the cast to i64 is exact.
    #[allow(clippy::cast_possible_wrap)]
    let ma = r.range(17) as i64 - 9;
    let mv = r.small();
    let op1 = ops[r.range(ops.len())];
    let op2 = ops[r.range(ops.len())];
    let cmp = cmps[r.range(cmps.len())];
    let br = brs[r.range(brs.len())];

    let src = format!(
        "\
.globals 4
.memory 8
main:
  push {mv}
  push {ma}
  storemem
  push {ma}
  loadmem
  pop
  push {a}
  call h1 1 2
  storeg 0
  loadg 0
  push {d}
  {cmp}
  {br} skip
  push 7
  storeg 2
skip:
  loadg 0
  print
  loadg 2
  print
  halt
h1:
  load 0
  push {b}
  {op1}
  call h2 1 2
  load 0
  {op2}
  storeg 1
  loadg 1
  ret
h2:
  load 0
  push {c}
  mul
  storeg 3
  loadg 3
  ret
"
    );
    assemble(&src).unwrap_or_else(|e| panic!("gen seed {seed}: {e}"))
}

/// Generate a counted loop that runs `iters` iterations. Each iteration does
/// global arithmetic, a nested call two frames deep, a memory write, a
/// conditional branch that alternates, and the loop exit test. It prints once
/// at the end and always terminates, so the trace length is a deterministic
/// linear function of `iters`.
pub fn gen_loop_program(seed: u64, iters: u64) -> Program {
    let mut r = Rng::new(seed);
    let ma = r.range(15);
    let mv = r.small();
    let src = format!(
        "\
.globals 4
.memory 16
main:
  push {mv}
  push {ma}
  storemem
  push 0
  storeg 0
  push 0
  storeg 1
loop:
  loadg 1
  push {iters}
  lt
  jz done
  loadg 0
  loadg 1
  add
  storeg 0
  loadg 1
  push 15
  mod
  push 77
  storemem
  loadg 1
  call bump 1 2
  storeg 2
  loadg 1
  push 3
  mod
  jz alt
  push 1
  jmp inc
alt:
  push 0
inc:
  pop
  loadg 1
  push 1
  add
  storeg 1
  jmp loop
done:
  loadg 0
  print
  halt
bump:
  load 0
  call dbl 1 1
  push 1
  add
  ret
dbl:
  load 0
  push 2
  mul
  ret
"
    );
    assemble(&src).unwrap_or_else(|e| panic!("gen_loop seed {seed}: {e}"))
}

/// A recursive program that descends `n` frames before unwinding, exercising
/// deep frame push and pop sequences.
pub fn gen_recursion_program(n: u64) -> Program {
    let src = format!(
        "\
.globals 2
.memory 8
main:
  push {n}
  call fact 1 2
  print
  halt
fact:
  load 0
  push 1
  le
  jz recurse
  push 1
  ret
recurse:
  load 0
  load 0
  push 1
  sub
  call fact 1 2
  mul
  ret
"
    );
    assemble(&src).unwrap_or_else(|e| panic!("gen_recursion: {e}"))
}

/// Build the ground-truth forward trace: index i is the state after i steps.
/// Stops at `TRACE_CAP` states for very long programs.
pub fn ground_truth(p: &Program) -> Vec<Step> {
    let mut vm = Vm::new(p);
    let mut out = Vec::new();
    loop {
        let snap = vm.snapshot();
        out.push(Step {
            pc: snap.pc,
            line: p.line_at(snap.pc),
            depth: snap.frames.len(),
            halted: snap.halted,
            snap,
        });
        if vm.halted {
            break;
        }
        if vm.step().is_none() {
            break;
        }
        if out.len() > TRACE_CAP {
            break;
        }
    }
    out
}

/// Walk the whole program forward with a raw VM, calling `on_step(i, vm)` once
/// per state i (state i is the machine after i instructions), and return the
/// index of the final state. This is the independent oracle for the stress
/// suite: plain forward execution, no debugger involved.
pub fn raw_pass(p: &Program, mut on_step: impl FnMut(usize, &Vm)) -> usize {
    let mut vm = Vm::new(p);
    let mut n = 0usize;
    loop {
        on_step(n, &vm);
        if vm.halted || vm.step().is_none() {
            return n;
        }
        n += 1;
    }
}

/// Expected breakpoint arrivals from a trace: steps whose pc equals the
/// address and which have not halted.
pub fn expected_breakpoint_stops(trace: &[Step], addr: usize) -> Vec<usize> {
    (1..trace.len())
        .filter(|&i| trace[i].pc == addr && !trace[i].halted)
        .collect()
}

pub fn watch_val(s: &Snapshot, loc: WatchLoc) -> i64 {
    match loc {
        WatchLoc::Global(i) => s.globals.get(i).copied().unwrap_or(0),
        WatchLoc::Mem(i) => s.memory.get(i).copied().unwrap_or(0),
        WatchLoc::Local(i) => s
            .frames
            .last()
            .and_then(|f| f.locals.get(i))
            .copied()
            .unwrap_or(0),
    }
}

/// Reference watch hits with the first-match-per-step semantics the debugger
/// uses in `cont`.
pub fn expected_watch_hits(trace: &[Step], watches: &[WatchLoc]) -> Vec<WatchHit> {
    let mut hits = Vec::new();
    for i in 1..trace.len() {
        for &loc in watches {
            let old = watch_val(&trace[i - 1].snap, loc);
            let new = watch_val(&trace[i].snap, loc);
            if old != new {
                hits.push(WatchHit { loc, old, new, step: i });
                break;
            }
        }
    }
    hits
}

pub fn expected_step_over(trace: &[Step], s: usize) -> usize {
    let start_line = trace[s].line;
    let start_depth = trace[s].depth;
    for (j, step) in trace.iter().enumerate().skip(s + 1) {
        if step.halted || (step.depth <= start_depth && step.line != start_line) {
            return j;
        }
    }
    trace.len() - 1
}

#[allow(dead_code)]
pub fn expected_step_out(trace: &[Step], s: usize) -> usize {
    let start_depth = trace[s].depth;
    for (j, step) in trace.iter().enumerate().skip(s + 1) {
        if step.halted || step.depth < start_depth {
            return j;
        }
    }
    trace.len() - 1
}

#[allow(dead_code)]
pub fn expected_step_into(trace: &[Step], s: usize) -> usize {
    let start_line = trace[s].line;
    for (j, step) in trace.iter().enumerate().skip(s + 1) {
        if step.halted || step.line != start_line {
            return j;
        }
    }
    trace.len() - 1
}

/// Run `d.cont()` until halt, collecting breakpoint stops.
pub fn collect_cont_stops(d: &mut Debugger, addr: usize) -> Vec<usize> {
    let mut stops = Vec::new();
    loop {
        match d.cont() {
            StopReason::Breakpoint(a) => {
                assert_eq!(a, addr);
                stops.push(d.step_count());
            }
            StopReason::Halted => break,
            StopReason::Limit => panic!("hit step limit"),
            other => panic!("unexpected stop {other:?}"),
        }
    }
    stops
}

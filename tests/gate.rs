//! The Bloodhound correctness gate.
//!
//! These tests are the machine-checkable oracle for the debugger's claims. They
//! build a ground-truth forward trace that snapshots the full VM state at every
//! step, then check that time travel, breakpoints, watchpoints, and the stepping
//! modes all agree with that trace exactly.
//!
//! The number of random programs is bounded for CI and controlled by the
//! `BLOODHOUND_FUZZ_OPS` environment variable.

use bloodhound::asm::assemble;
use bloodhound::debugger::{Debugger, StopReason, WatchHit, WatchLoc};
use bloodhound::samples;
use bloodhound::vm::{Op, Program, Snapshot, Vm};

/// One entry of the ground-truth trace.
struct Step {
    pc: usize,
    line: u32,
    depth: usize,
    halted: bool,
    snap: Snapshot,
}

const TRACE_CAP: usize = 2_000_000;

/// Build the ground-truth forward trace: index i is the state after i steps.
fn ground_truth(p: &Program) -> Vec<Step> {
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

fn fuzz_count() -> u64 {
    std::env::var("BLOODHOUND_FUZZ_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48)
}

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
    }
    fn next(&mut self) -> u64 {
        // xorshift64star
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn small(&mut self) -> i64 {
        (self.next() % 19) as i64 - 9
    }
}

/// Generate a well-formed, always-terminating program that exercises arithmetic,
/// locals, globals, memory, a forward branch, nested calls (depth 3), and prints.
fn gen_program(seed: u64) -> Program {
    let mut r = Rng::new(seed);
    let ops = ["add", "sub", "mul"];
    let cmps = ["lt", "gt", "le", "ge", "eq", "ne"];
    let a = r.small();
    let b = r.small();
    let c = r.small();
    let d = r.small();
    let ma = r.range(8);
    let mv = r.small();
    let op1 = ops[r.range(ops.len())];
    let op2 = ops[r.range(ops.len())];
    let cmp = cmps[r.range(cmps.len())];

    let src = format!(
        "\
.globals 4
.memory 8
main:
  push {mv}
  push {ma}
  storemem
  push {a}
  call h1 1 2
  storeg 0
  loadg 0
  push {d}
  {cmp}
  jz skip
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

// --- Gate 1: time-travel reversibility ------------------------------------

fn check_reversibility(p: &Program, seed_note: &str) {
    let trace = ground_truth(p);
    let total = trace.len() - 1;
    let mut d = Debugger::new(p.clone());

    // goto(n) reconstructs exactly the forward state at step n.
    for (n, step) in trace.iter().enumerate() {
        d.reset();
        d.goto(n);
        assert_eq!(d.snapshot(), step.snap, "{seed_note}: goto({n}) mismatch");
        assert_eq!(d.step_count(), n, "{seed_note}: step_count after goto({n})");
    }

    // From any start step, forward K then back K returns to the exact state.
    let mut r = Rng::new(0xF00D ^ p.code.len() as u64);
    for _ in 0..12 {
        let s = if total == 0 { 0 } else { r.range(total + 1) };
        let room = total - s;
        if room == 0 {
            continue;
        }
        let k = 1 + r.range(room);
        d.reset();
        d.goto(s);
        let before = d.snapshot();
        for _ in 0..k {
            assert!(d.forward(), "{seed_note}: forward within bounds");
        }
        for _ in 0..k {
            assert!(d.backward(), "{seed_note}: backward within bounds");
        }
        assert_eq!(d.snapshot(), before, "{seed_note}: forward {k} then back {k} at step {s}");
        assert_eq!(d.step_count(), s);
    }
}

#[test]
fn gate_reversibility_fuzz() {
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        check_reversibility(&p, &format!("seed {seed}"));
    }
}

#[test]
fn gate_reversibility_samples() {
    for (name, src) in samples::ALL {
        let p = assemble(src).unwrap();
        check_reversibility(&p, name);
    }
}

// --- Gate 2: breakpoint correctness ---------------------------------------

fn expected_breakpoint_stops(trace: &[Step], addr: usize) -> Vec<usize> {
    // A breakpoint stops before executing an instruction, so a halted terminal
    // state (which is about to execute nothing) is never a stop, even if its pc
    // still equals the breakpoint address.
    (1..trace.len())
        .filter(|&i| trace[i].pc == addr && !trace[i].halted)
        .collect()
}

fn check_breakpoint(p: &Program, addr: usize, note: &str) {
    let trace = ground_truth(p);
    let expected = expected_breakpoint_stops(&trace, addr);

    let mut d = Debugger::new(p.clone());
    d.add_break(addr);
    let mut actual = Vec::new();
    loop {
        match d.cont() {
            StopReason::Breakpoint(a) => {
                assert_eq!(a, addr, "{note}: stopped at wrong address");
                assert_eq!(d.pc(), addr, "{note}: pc not at breakpoint");
                actual.push(d.step_count());
            }
            StopReason::Halted => break,
            StopReason::Limit => panic!("{note}: hit step limit"),
            other => panic!("{note}: unexpected stop {other:?}"),
        }
    }
    assert_eq!(actual, expected, "{note}: breakpoint stops (addr {addr}) did not match trace");
}

#[test]
fn gate_breakpoint_no_spurious_fuzz() {
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        // Test a breakpoint at every instruction address, including ones never hit.
        for addr in 0..p.code.len() {
            check_breakpoint(&p, addr, &format!("seed {seed} addr {addr}"));
        }
        // An address past the end must never stop.
        check_breakpoint(&p, p.code.len() + 3, &format!("seed {seed} oob"));
    }
}

#[test]
fn gate_breakpoint_recurring_samples() {
    // Factorial recurses: the entry of `fact` is hit multiple times.
    let p = assemble(samples::FACTORIAL).unwrap();
    for addr in 0..p.code.len() {
        check_breakpoint(&p, addr, &format!("factorial addr {addr}"));
    }
    // Sum loop: the loop body pcs repeat.
    let p2 = assemble(samples::SUM_LOOP).unwrap();
    for addr in 0..p2.code.len() {
        check_breakpoint(&p2, addr, &format!("sum_loop addr {addr}"));
    }
}

// --- Gate 3: watchpoint correctness ---------------------------------------

fn watch_val(s: &Snapshot, loc: WatchLoc) -> i64 {
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

/// Reference watch hits with the same first-match-per-step semantics the
/// debugger uses in `cont`.
fn expected_watch_hits(trace: &[Step], watches: &[WatchLoc]) -> Vec<WatchHit> {
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

fn check_watchpoints(p: &Program, watches: &[WatchLoc], note: &str) {
    let trace = ground_truth(p);
    let expected = expected_watch_hits(&trace, watches);

    let mut d = Debugger::new(p.clone());
    for &w in watches {
        d.add_watch(w);
    }
    let mut actual = Vec::new();
    loop {
        match d.cont() {
            StopReason::Watchpoint(hit) => actual.push(hit),
            StopReason::Halted => break,
            StopReason::Limit => panic!("{note}: step limit"),
            other => panic!("{note}: unexpected {other:?}"),
        }
    }
    assert_eq!(actual, expected, "{note}: watch hits did not match reference diff");
}

#[test]
fn gate_watchpoint_fuzz() {
    let watches = [WatchLoc::Global(0), WatchLoc::Global(1), WatchLoc::Global(3), WatchLoc::Mem(0)];
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        check_watchpoints(&p, &watches, &format!("seed {seed}"));
    }
}

#[test]
fn gate_watchpoint_samples() {
    let p = assemble(samples::SUM_LOOP).unwrap();
    check_watchpoints(&p, &[WatchLoc::Global(0), WatchLoc::Global(1)], "sum_loop");
}

// --- Gate 4: step-over / step-out / step-into semantics --------------------

fn expected_step_over(trace: &[Step], s: usize) -> usize {
    let start_line = trace[s].line;
    let start_depth = trace[s].depth;
    for (j, step) in trace.iter().enumerate().skip(s + 1) {
        if step.halted || (step.depth <= start_depth && step.line != start_line) {
            return j;
        }
    }
    trace.len() - 1
}

fn expected_step_out(trace: &[Step], s: usize) -> usize {
    let start_depth = trace[s].depth;
    for (j, step) in trace.iter().enumerate().skip(s + 1) {
        if step.halted || step.depth < start_depth {
            return j;
        }
    }
    trace.len() - 1
}

fn expected_step_into(trace: &[Step], s: usize) -> usize {
    let start_line = trace[s].line;
    for (j, step) in trace.iter().enumerate().skip(s + 1) {
        if step.halted || step.line != start_line {
            return j;
        }
    }
    trace.len() - 1
}

fn check_stepping(p: &Program, note: &str) {
    let trace = ground_truth(p);
    let total = trace.len() - 1;
    let mut d = Debugger::new(p.clone());

    let is_call = |pc: usize| matches!(p.code.get(pc), Some(Op::Call(..)));

    for s in 0..total {
        if trace[s].halted {
            continue;
        }
        // step over
        let want = expected_step_over(&trace, s);
        d.reset();
        d.goto(s);
        d.step_over();
        assert_eq!(d.step_count(), want, "{note}: step_over from {s} (call={})", is_call(trace[s].pc));
        assert_eq!(d.snapshot(), trace[want].snap, "{note}: step_over state from {s}");

        // step out
        let want = expected_step_out(&trace, s);
        d.reset();
        d.goto(s);
        d.step_out();
        assert_eq!(d.step_count(), want, "{note}: step_out from {s}");
        assert_eq!(d.snapshot(), trace[want].snap, "{note}: step_out state from {s}");

        // step into (line)
        let want = expected_step_into(&trace, s);
        d.reset();
        d.goto(s);
        d.step_line();
        assert_eq!(d.step_count(), want, "{note}: step_into from {s}");
        assert_eq!(d.snapshot(), trace[want].snap, "{note}: step_into state from {s}");
    }
}

#[test]
fn gate_stepping_fuzz() {
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        check_stepping(&p, &format!("seed {seed}"));
    }
}

#[test]
fn gate_stepping_samples() {
    for (name, src) in samples::ALL {
        let p = assemble(src).unwrap();
        check_stepping(&p, name);
    }
}

// --- Gate 0: machine and assembler hardening --------------------------------

#[test]
fn running_off_the_end_halts_cleanly() {
    // A program with no halt and no ret falls off the end of the code. The
    // machine must end in a halted terminal state, not a stuck non-halted
    // state whose pc is out of range and which no longer steps.
    let p = assemble("  push 1\n  push 2\n").unwrap();
    let mut vm = Vm::new(&p);
    while vm.step().is_some() {}
    let snap = vm.snapshot();
    assert!(snap.halted, "terminal state must be halted, got {snap:?}");
    assert!(vm.step().is_none(), "no further progress is possible");

    // The same must hold when a jump or call leaves the code outright. The
    // assembler no longer produces such targets, but the public API allows
    // hand-built programs, so the machine itself must stay safe.
    let one = |ops: Vec<Op>| Program {
        line_of: vec![1; ops.len()],
        source: vec!["<test>".to_string()],
        labels: vec![None; ops.len()],
        globals: 4,
        memory: 8,
        code: ops,
    };
    for ops in [
        vec![Op::Push(1), Op::Jmp(9)],
        vec![Op::Push(1), Op::Push(0), Op::Jz(9)],
        vec![Op::Push(1), Op::Push(0), Op::Jnz(9)],
        vec![Op::Push(1), Op::Call(9, 0, 1)],
    ] {
        let p = one(ops);
        let mut vm = Vm::new(&p);
        while vm.step().is_some() {}
        assert!(vm.snapshot().halted, "op {:?} must end halted", p.code);
    }

    // Falling off the end must be reversible like any other step.
    let p = assemble("  push 5\n").unwrap();
    let mut vm = Vm::new(&p);
    let u = vm.step().expect("one step");
    assert!(vm.halted, "falling off the end must halt");
    vm.undo(u);
    assert!(!vm.halted);
    assert_eq!(vm.pc, 0);
    assert_eq!(vm.stack, vec![0; 0]);
}

#[test]
fn absurd_data_sizes_are_rejected() {
    // A directive sized to OOM the machine must be a clean assembly error.
    let e = assemble(".memory 1000000000000\n  halt\n").unwrap_err();
    assert!(e.message.contains("too large"), "got: {e}");
    let e = assemble(".globals 99999999999\n  halt\n").unwrap_err();
    assert!(e.message.contains("too large"), "got: {e}");
    // In-range sizes still work.
    let p = assemble(".memory 1048576\n  halt\n").unwrap();
    assert_eq!(p.memory, 1_048_576);
}

#[test]
fn duplicate_directives_are_rejected() {
    assert!(assemble(".memory 8\n.memory 16\n  halt\n").is_err());
    assert!(assemble(".globals 2\n.globals 3\n  halt\n").is_err());
    // Same directive repeated is fine if it errors, distinct directives are fine.
    assert!(assemble(".globals 2\n.memory 8\n  halt\n").is_ok());
}

#[test]
fn out_of_range_branch_targets_are_rejected() {
    // len == 2, so target 7 is malformed and must be a clean error.
    assert!(assemble("  jmp 7\n  halt\n").is_err());
    assert!(assemble("  jz 7\n  halt\n").is_err());
    assert!(assemble("  jnz 7\n  halt\n").is_err());
    assert!(assemble("  call 7 0 1\n  halt\n").is_err());
    // One past the last instruction is a legal run-off-the-end exit.
    let p = assemble("  jmp 2\n  halt\n").unwrap();
    assert_eq!(p.code[0], Op::Jmp(2));
}

#[test]
fn numeric_labels_are_rejected() {
    // A label that parses as a number can never be targeted by name because
    // numeric operands win, so defining one is a silent trap.
    assert!(assemble("2:\n  halt\n").is_err());
    assert!(assemble("999:\n  halt\n").is_err());
}

// --- a targeted step-over-across-nested-calls assertion --------------------

#[test]
fn step_over_skips_nested_calls_lands_same_depth() {
    let p = gen_program(1);
    let trace = ground_truth(&p);
    // Find the main-level call site (call h1) and step over it.
    let call_step = (0..trace.len() - 1)
        .find(|&i| matches!(p.code.get(trace[i].pc), Some(Op::Call(..))) && trace[i].depth == 1)
        .expect("a depth-1 call site exists");
    let mut d = Debugger::new(p.clone());
    d.goto(call_step);
    let depth_before = d.vm().depth();
    d.step_over();
    assert_eq!(d.vm().depth(), depth_before, "step-over returned to the original frame depth");
    // And it advanced to a different line (past the call), verified against trace.
    assert_ne!(d.current_line(), trace[call_step].line);
}

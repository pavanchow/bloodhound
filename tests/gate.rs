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
use bloodhound::expr::{EvalCtx, Expr, ExprError};
use bloodhound::samples;
use bloodhound::vm::{Op, Program, Snapshot, Vm};

mod common;
use common::*;

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

// --- Gate 5: adversarial edge cases -----------------------------------------

#[test]
fn breakpoint_at_address_zero() {
    // Forward: a straight-line program leaves address 0 behind immediately, so
    // continuing never stops there (step 0 is not an arrival).
    let p = assemble("  push 1\n  print\n  halt\n").unwrap();
    assert_eq!(p.line_at(0), 1);
    check_breakpoint(&p, 0, "bp at addr 0 forward");

    // Backward: run_back must land exactly on step 0 with the breakpoint.
    let mut d = Debugger::new(p.clone());
    d.add_break(0);
    while d.forward() {}
    let r = d.run_back();
    assert_eq!(r, StopReason::Breakpoint(0));
    assert_eq!(d.step_count(), 0);
    assert_eq!(d.snapshot(), ground_truth(&p)[0].snap);

    // On a looping program address 0 is re-arrived, so forward cont stops there.
    let p2 = assemble(samples::SUM_LOOP).unwrap();
    check_breakpoint(&p2, 0, "bp at addr 0 loop");
}

#[test]
fn breakpoint_at_last_instruction() {
    // The halt instruction's own address stops once, before the halt executes,
    // and never on the halted terminal state.
    let p = assemble("  push 1\n  halt\n").unwrap();
    let halt_addr = p.code.len() - 1;
    let trace = ground_truth(&p);
    let mut d = Debugger::new(p);
    d.add_break(halt_addr);
    let r = d.cont();
    assert_eq!(r, StopReason::Breakpoint(halt_addr));
    assert_eq!(d.step_count(), trace.len() - 2);
    assert!(!d.halted());
    assert_eq!(d.cont(), StopReason::Halted);
    assert_eq!(d.step_count(), trace.len() - 1);
    // A third cont is a no-op on the halted machine.
    assert_eq!(d.cont(), StopReason::Halted);
}

#[test]
fn watchpoint_on_memory_written_by_the_watched_instruction() {
    // storemem computes its target from the stack, so one instruction both
    // decides and performs the write to the watched cell.
    let src = "\n.memory 8\nmain:\n  push 3\n  push 5\n  storemem\n  halt\n";
    let p = assemble(src).unwrap();
    check_watchpoints(&p, &[WatchLoc::Mem(3)], "storemem self-write");

    // A write that stores the same value is not a change and must not fire.
    let p2 = assemble("  push 7\n  storeg 0\n  push 7\n  storeg 0\n  halt\n").unwrap();
    let trace = ground_truth(&p2);
    let mut d = Debugger::new(p2);
    d.add_watch(WatchLoc::Global(0));
    let r = d.cont();
    assert_eq!(
        r,
        StopReason::Watchpoint(WatchHit { loc: WatchLoc::Global(0), old: 0, new: 7, step: 2 })
    );
    // Second write is a no-op: continue runs all the way to the halt.
    assert_eq!(d.cont(), StopReason::Halted);
    assert_eq!(d.step_count(), trace.len() - 1);
}

#[test]
fn watchpoint_on_locals_across_calls() {
    // A local watch follows the innermost frame, so a call changes what the
    // watch reads even though no store executed. The reference diff uses the
    // same rule, and both must agree.
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        check_watchpoints(&p, &[WatchLoc::Local(0), WatchLoc::Local(1)], &format!("seed {seed} locals"));
    }
}

#[test]
fn step_out_at_outermost_frame_runs_to_halt() {
    let p = assemble(samples::SUM_LOOP).unwrap();
    let trace = ground_truth(&p);
    let want = expected_step_out(&trace, 0);
    assert_eq!(want, trace.len() - 1, "outermost step-out lands on the halted terminal state");
    let mut d = Debugger::new(p);
    d.step_out();
    assert_eq!(d.step_count(), want);
    assert_eq!(d.snapshot(), trace[want].snap);
    assert!(d.halted());
}

#[test]
fn step_over_on_a_call_at_the_final_instruction() {
    // The call is the last instruction of the program and its callee halts.
    let src = "main:\n  jmp start\nf:\n  halt\nstart:\n  push 1\n  call f 0 1\n";
    let p = assemble(src).unwrap();
    assert!(matches!(p.code.last(), Some(Op::Call(..))), "call must be the final instruction");
    let trace = ground_truth(&p);
    let call_step = trace
        .iter()
        .position(|s| matches!(p.code.get(s.pc), Some(Op::Call(..))))
        .expect("the call executes");
    let want = expected_step_over(&trace, call_step);
    assert_eq!(want, trace.len() - 1);
    let mut d = Debugger::new(p);
    d.goto(call_step);
    d.step_over();
    assert_eq!(d.step_count(), want);
    assert_eq!(d.snapshot(), trace[want].snap);
    assert!(d.halted());
}

#[test]
fn goto_to_the_current_step_is_a_no_op() {
    let p = assemble(samples::FACTORIAL).unwrap();
    let trace = ground_truth(&p);
    let mut d = Debugger::new(p);
    for k in [0usize, 3, trace.len() / 2, trace.len() - 1] {
        d.reset();
        d.goto(k);
        let before = d.snapshot();
        assert_eq!(d.goto(k), StopReason::Stepped);
        assert_eq!(d.step_count(), k);
        assert_eq!(d.snapshot(), before, "goto(current) at step {k} moved the machine");
    }
}

#[test]
fn reverse_at_step_zero_and_forward_at_the_end() {
    let p = assemble(samples::SUM_LOOP).unwrap();
    let trace = ground_truth(&p);
    let total = trace.len() - 1;
    let mut d = Debugger::new(p);

    // At step 0 there is nothing to reverse.
    assert!(!d.backward());
    assert_eq!(d.run_back(), StopReason::Start);
    assert_eq!(d.snapshot(), trace[0].snap);

    // Run to the end: nothing more to run forward, cont is a no-op, and a huge
    // goto clamps at the terminal step.
    while d.forward() {}
    assert!(!d.forward());
    assert_eq!(d.cont(), StopReason::Halted);
    assert_eq!(d.step_count(), total);
    assert_eq!(d.goto(total + 1_000_000), StopReason::Halted);
    assert_eq!(d.step_count(), total);
    assert_eq!(d.snapshot(), trace[total].snap);

    // A stopped-in-time goto just past the natural end also clamps.
    d.reset();
    assert_eq!(d.goto(total + 5), StopReason::Halted);
    assert_eq!(d.snapshot(), trace[total].snap);
}

#[test]
fn watchpoint_takes_precedence_over_a_coincident_breakpoint() {
    // One step both writes the watched global and lands on a breakpoint (the
    // halt instruction at address 2). The watch is reported for that step, and
    // continuing from there still terminates cleanly.
    let p = assemble("  push 5\n  storeg 0\n  halt\n").unwrap();
    let mut d = Debugger::new(p);
    d.add_break(2); // the halt instruction
    d.add_watch(WatchLoc::Global(0));
    let r = d.cont();
    assert_eq!(
        r,
        StopReason::Watchpoint(WatchHit { loc: WatchLoc::Global(0), old: 0, new: 5, step: 2 })
    );
    assert_eq!(d.pc(), 2, "the machine sits on the breakpoint address");
    assert_eq!(d.cont(), StopReason::Halted);
}

#[test]
fn assembler_negative_battery() {
    let bad = [
        "  frobnicate\n  halt\n",                      // unknown opcode
        "  push 99999999999999999999999999\n  halt\n", // immediate overflow
        "loop:\nloop:\n  halt\n",                      // duplicate labels
        "",                                            // empty program
        "   \n; only a comment\n",                     // blank and comment only
        "solo_label:\n",                               // label with no instruction
        "  jmp\n  halt\n",                             // missing target
        "  push\n  halt\n",                            // missing integer
        "  load -1\n  halt\n",                         // negative slot index
        "  store\n  halt\n",                           // missing slot index
        ".bss 8\n  halt\n",                            // unknown directive
        ".globals -3\n  halt\n",                       // invalid directive value
        ".globals\n  halt\n",                          // directive without value
        ":\n  halt\n",                                 // empty label
        "  call f 1\nf:\n  ret\n",                     // call missing an operand
        "  jz\n  halt\n",                              // conditional branch missing target
    ];
    for src in bad {
        let res = assemble(src);
        assert!(res.is_err(), "expected `{src:?}` to be rejected");
        let e = res.unwrap_err();
        assert!(!e.message.is_empty(), "error for `{src:?}` needs a message");
        assert!(e.line >= 1, "error for `{src:?}` needs a source line");
    }

    // A program with no halt still assembles. Running it falls off the end and
    // the machine reports a halted terminal state.
    let p = assemble("  push 1\n  push 2\n  add\n").unwrap();
    let mut vm = Vm::new(&p);
    while vm.step().is_some() {}
    assert!(vm.snapshot().halted);
}

// --- Gate 6: conditional breakpoints and expression watches -----------------

/// A snapshot predicate written in plain Rust, used as the independent
/// reference for expression conditions.
type Pred = fn(&Snapshot) -> bool;

fn top_of(s: &Snapshot) -> i64 {
    s.stack.last().copied().unwrap_or(0)
}

/// Reference conditional breakpoint stops computed straight from the trace:
/// an arrival whose pc equals the address, not halted, and whose snapshot
/// satisfies `pred`. The predicate is plain Rust on the snapshot, never the
/// expression evaluator, so the two sides are independent.
fn expected_cond_breakpoint_stops(
    trace: &[Step],
    addr: usize,
    pred: impl Fn(&Snapshot) -> bool,
) -> Vec<usize> {
    (1..trace.len())
        .filter(|&i| trace[i].pc == addr && !trace[i].halted && pred(&trace[i].snap))
        .collect()
}

#[test]
fn gate_conditional_breakpoints_fuzz() {
    // (text, independent predicate) pairs evaluated against every arrival.
    let cases: [(&str, Pred); 3] = [
        ("globals[0] > 3", |s| s.globals.first().copied().unwrap_or(0) > 3),
        ("memory[0] % 2 == 1", |s| {
            let v = s.memory.first().copied().unwrap_or(0);
            v.wrapping_rem(2) == 1
        }),
        ("top >= 0 && depth == 1", |s| top_of(s) >= 0 && s.frames.len() == 1),
    ];
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        let trace = ground_truth(&p);
        for addr in [0usize, p.code.len() / 2, p.code.len() - 1] {
            for (text, pred) in cases {
                let cond = Expr::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
                let want = expected_cond_breakpoint_stops(&trace, addr, pred);
                let mut d = Debugger::new(p.clone());
                d.add_break_cond(addr, cond);
                let got = collect_cont_stops(&mut d, addr);
                assert_eq!(got, want, "seed {seed} addr {addr} cond `{text}`");
            }
        }
    }
}

#[test]
fn gate_conditional_run_back_matches_trace() {
    // From the halted end, reverse-continue with a conditional breakpoint must
    // visit exactly the forward stop list, last hit first.
    let p = gen_program(2);
    let trace = ground_truth(&p);
    let addr = p.code.len() / 2;
    let pred = |s: &Snapshot| s.globals.first().copied().unwrap_or(0) > 2;
    let want_fwd = expected_cond_breakpoint_stops(&trace, addr, pred);

    let mut d = Debugger::new(p);
    d.add_break_cond(addr, Expr::parse("globals[0] > 2").unwrap());
    while d.forward() {}
    let mut got_rev = Vec::new();
    loop {
        match d.run_back() {
            StopReason::Breakpoint(a) => {
                assert_eq!(a, addr);
                got_rev.push(d.step_count());
            }
            StopReason::Start => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    got_rev.reverse();
    assert_eq!(got_rev, want_fwd, "run_back visits the forward stops in reverse");
}

#[test]
fn gate_watch_if_fuzz() {
    // A conditional watch fires on the first changed watch per step whose
    // condition holds on the post-change state. The reference implements the
    // same rule with plain Rust predicates on the trace.
    fn reference(trace: &[Step], loc: WatchLoc, keep: impl Fn(&Snapshot) -> bool) -> Vec<WatchHit> {
        let mut hits = Vec::new();
        for i in 1..trace.len() {
            let old = watch_val(&trace[i - 1].snap, loc);
            let new = watch_val(&trace[i].snap, loc);
            if old != new && keep(&trace[i].snap) {
                hits.push(WatchHit { loc, old, new, step: i });
            }
        }
        hits
    }

    let cases: [(&str, Pred); 2] = [
        ("globals[0] % 2 == 0", |s| {
            let v = s.globals.first().copied().unwrap_or(0);
            v.wrapping_rem(2) == 0
        }),
        ("globals[0] > memory[0]", |s| {
            let g = s.globals.first().copied().unwrap_or(0);
            let m = s.memory.first().copied().unwrap_or(0);
            g > m
        }),
    ];
    for seed in 0..fuzz_count() {
        let p = gen_program(seed);
        let trace = ground_truth(&p);
        for (text, keep) in cases {
            let want = reference(&trace, WatchLoc::Global(0), keep);
            let mut d = Debugger::new(p.clone());
            d.add_watch_cond(WatchLoc::Global(0), Expr::parse(text).unwrap());
            let mut got = Vec::new();
            loop {
                match d.cont() {
                    StopReason::Watchpoint(hit) => {
                        assert_eq!(hit.loc, WatchLoc::Global(0));
                        got.push(hit);
                    }
                    StopReason::Halted => break,
                    StopReason::Limit => panic!("step limit"),
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert_eq!(got, want, "seed {seed} watch-if `{text}`");
        }
    }
}

#[test]
fn gate_watch_if_skipped_condition_does_not_mask_other_watches() {
    // Two watches change on the same step. The first has a false condition, so
    // the scan continues and the second watch is reported.
    let src = "\
.globals 3
.memory 8
main:
  push 1
  storeg 0
  push 5
  storeg 1
  halt
";
    let p = assemble(src).unwrap();
    let mut d = Debugger::new(p);
    d.add_watch_cond(WatchLoc::Global(0), Expr::parse("globals[0] > 100").unwrap());
    d.add_watch(WatchLoc::Global(1));
    let r = d.cont();
    assert_eq!(
        r,
        StopReason::Watchpoint(WatchHit { loc: WatchLoc::Global(1), old: 0, new: 5, step: 4 })
    );
}

#[test]
fn expression_evaluation_never_mutates_machine_state() {
    let p = gen_program(4);
    let mut d = Debugger::new(p);
    d.goto(7);
    let before = d.snapshot();
    let exprs = [
        "pc", "depth", "top", "-top", "!pc", "pc + depth * 2",
        "globals[0] + globals[3] - memory[7]", "memory[pc % 8]",
        "globals[1000]", "memory[-5]", "1 / 0", "1 % 0",
        "(top < 0) && (pc > 0) || (depth == 3)",
        "9223372036854775807 + 1 + 9223372036854775807 + 1",
    ];
    for text in exprs {
        let e = Expr::parse(text).unwrap_or_else(|err| panic!("{text}: {err}"));
        let v = e.eval(&d.eval_ctx());
        let _ = v;
        assert_eq!(d.snapshot(), before, "evaluating `{text}` mutated the machine");
    }
    // Malformed parses must not touch the machine either.
    for text in ["", "pc +", "bogus", "globals["] {
        assert!(Expr::parse(text).is_err(), "`{text}` should not parse");
        assert_eq!(d.snapshot(), before, "parsing `{text}` mutated the machine");
    }
}

#[test]
fn expression_indexing_matches_vm_reads() {
    // memory[e] in an expression must read the same cell loadmem reads for the
    // same address, including wrapped negative addresses.
    let src = "\
.memory 8
main:
  push -1
  push 42
  storemem
  push -1
  loadmem
  print
  halt
";
    let p = assemble(src).unwrap();
    let mut vm = Vm::new(&p);
    while vm.step().is_some() && vm.output.is_empty() {}
    let printed: i64 = vm.output.last().and_then(|s| s.parse().ok()).expect("a printed value");
    let cond = Expr::parse("memory[-1] == 42").unwrap();
    assert!(cond.eval_cond(&EvalCtx::of_vm(&vm)), "memory[-1] must read the wrapped cell");
    assert_eq!(printed, 42);
}

#[test]
fn expression_errors_are_clean_values() {
    // Parse errors carry a message and a source-free error type, nothing else.
    let ExprError { message } = Expr::parse("globals[99999999999999999999]").unwrap_err();
    assert!(message.contains("out of range"), "got: {message}");
    let ExprError { message } = Expr::parse("foo").unwrap_err();
    assert!(message.contains("unknown variable"), "got: {message}");
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

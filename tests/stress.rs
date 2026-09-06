//! The Bloodhound max-scale stress suite.
//!
//! These tests push the debugger to its limits: multi-million-step traces,
//! thousands of time-travel moves, deep recursion, and long breakpoint and
//! watchpoint streams, all verified against independently computed
//! expectations. They are marked `#[ignore]` so the normal gate stays fast,
//! and run with:
//!
//! ```text
//! cargo test --release --test stress -- --ignored --nocapture
//! ```
//!
//! Scale is controlled by environment variables:
//! - `BLOODHOUND_STRESS_ITERS` loop iterations for the long programs
//!   (default 55000, roughly 1.9 million steps per program)
//! - `BLOODHOUND_STRESS_SCRUBS` random `goto` jumps (default 50000)
//! - `BLOODHOUND_STRESS_ALT` alternating forward/backward moves (default 10000)
//!
//! Run single threaded for stable timings and bounded memory.

// Test routines use compact single-letter names for programs, debuggers, and
// trace indices.
#![allow(clippy::many_single_char_names)]

use bloodhound::debugger::{Debugger, StopReason, WatchHit, WatchLoc};
use bloodhound::expr::Expr;
use bloodhound::vm::{Program, Snapshot};

mod common;
use common::*;

fn stress_scrubs() -> usize {
    std::env::var("BLOODHOUND_STRESS_SCRUBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000)
}

fn stress_alt() -> usize {
    std::env::var("BLOODHOUND_STRESS_ALT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// Run the full forward pass once, recording (step, snapshot) anchors every
/// `every` steps and at the final state. Anchors are the ground truth for
/// random-access scrubbing without storing millions of snapshots.
fn anchors(p: &Program, every: usize) -> (usize, Vec<(usize, Snapshot)>) {
    let mut out = Vec::new();
    let total = raw_pass(p, |i, vm| {
        if i % every == 0 {
            out.push((i, vm.snapshot()));
        }
    });
    let last = out.last().map_or(0, |&(i, _)| i);
    if last != total {
        // Re-run to capture the terminal state (raw_pass is cheap).
        let mut vm = bloodhound::vm::Vm::new(p);
        let mut n = 0;
        loop {
            if n == total {
                out.push((n, vm.snapshot()));
                break;
            }
            vm.step().expect("step within total");
            n += 1;
        }
    }
    (total, out)
}

// --- stress 1: reversibility on a multi-million-step path -------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_long_reversibility() {
    let iters = stress_iters();
    let p = gen_loop_program(0xBEEF, iters);
    let (total, marks) = anchors(&p, 512);
    println!("stress_long_reversibility: iters={iters} total_steps={total} anchors={}", marks.len());

    let mut d = Debugger::new(p);
    // goto to every anchor reconstructs the exact forward state.
    for &(i, ref snap) in &marks {
        d.reset();
        d.goto(i);
        assert_eq!(d.step_count(), i, "step_count after goto({i})");
        assert_eq!(d.snapshot(), *snap, "goto({i}) mismatch on a {total}-step trace");
    }

    // From random start steps, forward K then back K returns exactly.
    let mut r = Rng::new(0x5CA1E);
    let mut d2 = Debugger::new(gen_loop_program(0xBEEF, iters));
    for _ in 0..256 {
        let s = marks[r.range(marks.len())].0;
        let room = total - s;
        if room == 0 {
            continue;
        }
        let k = 1 + r.range(room.min(4096));
        d2.reset();
        d2.goto(s);
        let before = d2.snapshot();
        for _ in 0..k {
            assert!(d2.forward());
        }
        for _ in 0..k {
            assert!(d2.backward());
        }
        assert_eq!(d2.snapshot(), before, "forward {k} back {k} at step {s}");
        assert_eq!(d2.step_count(), s);
    }

    // Thousands of alternating moves mid-program: state must flip exactly.
    let mid = marks[marks.len() / 2].0;
    d2.reset();
    d2.goto(mid);
    let before = d2.snapshot();
    let alt = stress_alt();
    for j in 0..alt {
        assert!(d2.forward());
        assert!(d2.backward());
        assert_eq!(d2.snapshot(), before, "alternation {j} at step {mid}");
    }
    // And a long reverse run from the end back to step 0 is exact.
    d2.reset();
    d2.goto(total);
    for i in (0..total).rev() {
        assert!(d2.backward());
        if i % 4096 == 0 {
            let want = marks.iter().find(|m| m.0 == i).expect("anchor exists");
            assert_eq!(d2.snapshot(), want.1, "reverse pass at step {i}");
        }
    }
    assert_eq!(d2.step_count(), 0);
}

// --- stress 2: breakpoint stream over a long loop ---------------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_long_breakpoints() {
    let iters = stress_iters();
    let p = gen_loop_program(0x00C0_FFEE, iters);
    // The accumulator store inside the loop body: hit once per iteration. It
    // is the last StoreG(0) in the program (the first is the prologue init).
    let bp_addr = p
        .code
        .iter()
        .rposition(|op| matches!(op, bloodhound::vm::Op::StoreG(0)))
        .expect("a storeg 0 in the loop body");

    // Independent expectation: a raw forward pass records every arrival.
    let mut expected = Vec::new();
    let total = raw_pass(&p, |i, vm| {
        if i > 0 && vm.pc == bp_addr && !vm.halted {
            expected.push(i);
        }
    });
    println!(
        "stress_long_breakpoints: iters={iters} total_steps={total} expected_hits={}",
        expected.len()
    );
    assert!(expected.len() > 1000, "the loop must produce a long hit stream");

    let mut d = Debugger::new(p);
    d.add_break(bp_addr);
    let got = collect_cont_stops(&mut d, bp_addr);
    assert_eq!(got.len(), expected.len(), "hit count");
    assert_eq!(got, expected, "hit steps must match the raw pass exactly");

    // Breakpoint alignment after goto: from a random point, the next stop must
    // be the first expected hit strictly after it.
    let mut r = Rng::new(0xABC);
    for _ in 0..600 {
        let target = r.range(total + 1);
        d.reset();
        d.goto(target);
        let want = expected.iter().find(|&&s| s > target).copied();
        match d.cont() {
            StopReason::Breakpoint(a) => {
                assert_eq!(a, bp_addr);
                assert_eq!(Some(d.step_count()), want, "next hit after goto({target})");
            }
            StopReason::Halted => assert!(want.is_none(), "missed hit after goto({target})"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

// --- stress 3: watchpoint stream over a long loop ----------------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_long_watchpoints() {
    let iters = stress_iters();
    let p = gen_loop_program(0xD00D, iters);
    let watches = [WatchLoc::Global(0), WatchLoc::Mem(0)];

    // Independent reference diff over the raw pass.
    let mut expected: Vec<WatchHit> = Vec::new();
    let mut prev: Option<Vec<i64>> = None;
    let total = raw_pass(&p, |i, vm| {
        let vals: Vec<i64> = watches.iter().map(|&l| watch_val_raw(vm, l)).collect();
        if let Some(before) = &prev {
            for (k, &loc) in watches.iter().enumerate() {
                if before[k] != vals[k] {
                    expected.push(WatchHit {
                        loc,
                        old: before[k],
                        new: vals[k],
                        step: i,
                    });
                    break;
                }
            }
        }
        prev = Some(vals);
    });
    println!(
        "stress_long_watchpoints: iters={iters} total_steps={total} expected_hits={}",
        expected.len()
    );
    assert!(expected.len() > 1000, "watched values must change often");

    let mut d = Debugger::new(p);
    for &w in &watches {
        d.add_watch(w);
    }
    let mut got = Vec::new();
    loop {
        match d.cont() {
            StopReason::Watchpoint(hit) => got.push(hit),
            StopReason::Halted => break,
            StopReason::Limit => panic!("step limit"),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(got.len(), expected.len(), "hit count");
    assert_eq!(got, expected, "watch hits must match the reference diff exactly");
}

fn watch_val_raw(vm: &bloodhound::vm::Vm, loc: WatchLoc) -> i64 {
    match loc {
        WatchLoc::Global(i) => vm.globals.get(i).copied().unwrap_or(0),
        WatchLoc::Mem(i) => vm.memory.get(i).copied().unwrap_or(0),
        WatchLoc::Local(i) => vm.local(i),
    }
}

// --- stress 4: random-access scrubbing ---------------------------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_goto_scrub() {
    let iters = stress_iters();
    let p = gen_loop_program(0xFACE, iters);
    let (total, marks) = anchors(&p, 512);
    println!("stress_goto_scrub: iters={iters} total_steps={total} anchors={}", marks.len());
    let index: std::collections::HashMap<usize, &Snapshot> =
        marks.iter().map(|&(i, ref s)| (i, s)).collect();

    let mut d = Debugger::new(p);
    let mut r = Rng::new(0x6E11);
    let scrubs = stress_scrubs();
    let mut min = total;
    let mut max = 0usize;
    for j in 0..scrubs {
        let &(i, ref snap) = &marks[r.range(marks.len())];
        d.goto(i);
        min = min.min(i);
        max = max.max(i);
        assert_eq!(d.step_count(), i, "scrub {j}: goto({i})");
        assert_eq!(d.snapshot(), *snap, "scrub {j}: goto({i}) state");
        // Hop to the current step: a no-op that must not move anything.
        d.goto(d.step_count());
        assert_eq!(d.snapshot(), *snap);
        // Random forward/back drift around the target, then re-scrub.
        let k = r.range(64);
        for _ in 0..k {
            assert!(d.forward());
        }
        for _ in 0..k {
            assert!(d.backward());
        }
        assert_eq!(d.snapshot(), *index[&i], "scrub {j}: drift restore at {i}");
    }
    println!("stress_goto_scrub: scrubbed {scrubs} targets spanning [{min}, {max}]");
}

// --- stress 5: deep recursion -------------------------------------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_deep_recursion() {
    let n = std::env::var("BLOODHOUND_STRESS_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_500u64);
    let p = gen_recursion_program(n);
    let trace = ground_truth(&p);
    let total = trace.len() - 1;
    let max_depth = trace.iter().map(|s| s.depth).max().unwrap_or(0);
    println!("stress_deep_recursion: n={n} total_steps={total} max_depth={max_depth}");
    assert_eq!(max_depth as u64, n + 1, "entry frame plus {n} nested calls");

    // Step out from the deepest frame lands back at depth n after one ret.
    let deepest = trace.iter().position(|s| s.depth == max_depth).unwrap();
    let want = expected_step_out(&trace, deepest);
    let mut d = Debugger::new(p.clone());
    d.goto(deepest);
    d.step_out();
    assert_eq!(d.step_count(), want);
    assert_eq!(d.snapshot(), trace[want].snap);
    assert_eq!(d.vm().depth(), max_depth - 1);

    // Step out from the outermost frame runs to the halted end.
    d.reset();
    d.step_out();
    assert!(d.halted());
    assert_eq!(d.snapshot(), trace[total].snap);

    // Reversibility around the deepest frame pushes and pops.
    let mut r = Rng::new(0xDEE);
    for _ in 0..32 {
        let s = deepest.saturating_sub(r.range(64));
        let room = (total - s).min(256);
        if room == 0 {
            continue;
        }
        let k = 1 + r.range(room);
        d.reset();
        d.goto(s);
        let before = d.snapshot();
        for _ in 0..k {
            assert!(d.forward());
        }
        for _ in 0..k {
            assert!(d.backward());
        }
        assert_eq!(d.snapshot(), before, "deep cycle at step {s}");
    }

    // A breakpoint on the function entry is hit once per recursion level.
    let entry_addr = trace
        .iter()
        .position(|s| s.depth == 2 && !s.halted && s.pc > 0)
        .expect("first callee entry");
    let expected = expected_breakpoint_stops(&trace, trace[entry_addr].pc);
    assert_eq!(expected.len() as u64, n, "one hit per recursive call");
    let mut d2 = Debugger::new(p);
    d2.add_break(trace[entry_addr].pc);
    let got = collect_cont_stops(&mut d2, trace[entry_addr].pc);
    assert_eq!(got, expected);
}

// --- stress 6: single-step bookkeeping over a long stretch -------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_single_step_alignment() {
    let iters = stress_iters();
    let p = gen_loop_program(0x5EED, iters);
    let (total, marks) = anchors(&p, 4096);
    println!("stress_single_step_alignment: iters={iters} total_steps={total} anchors={}", marks.len());
    let index: std::collections::HashMap<usize, &Snapshot> =
        marks.iter().map(|&(i, ref s)| (i, s)).collect();

    let mut d = Debugger::new(p);
    let mut r = Rng::new(0x1234);
    // From random anchors, single-step forward across at least one anchor
    // boundary and require every intermediate state to match the trace.
    for _ in 0..48 {
        let start = marks[r.range(marks.len() - 1)].0;
        d.reset();
        d.goto(start);
        let run = 65_536.min(total - start);
        for j in 0..run {
            assert!(d.forward(), "forward at step {}", start + j);
            let at = start + j + 1;
            if let Some(want) = index.get(&at) {
                assert_eq!(d.snapshot(), **want, "state at step {at}");
            }
            assert_eq!(d.step_count(), at, "bookkeeping at step {at}");
        }
    }
}

// --- stress 7: conditional breakpoints at scale -------------------------------

#[test]
#[ignore = "max-scale stress, run explicitly (see the module docs)"]
fn stress_conditional_breakpoints_long() {
    let iters = stress_iters();
    let p = gen_loop_program(0xF005, iters);
    let bp_addr = p
        .code
        .iter()
        .rposition(|op| matches!(op, bloodhound::vm::Op::StoreG(0)))
        .expect("a storeg 0 in the loop body");
    let cond = Expr::parse("globals[0] % 2 == 1").unwrap();

    // Independent expectation with a plain Rust predicate.
    let mut expected = Vec::new();
    let total = raw_pass(&p, |i, vm| {
        if i > 0 && vm.pc == bp_addr && !vm.halted {
            let acc = vm.globals.first().copied().unwrap_or(0);
            if acc.rem_euclid(2) == 1 {
                expected.push(i);
            }
        }
    });
    println!(
        "stress_conditional_breakpoints_long: iters={iters} total_steps={total} expected_hits={}",
        expected.len()
    );
    assert!(expected.len() > 1000);

    let mut d = Debugger::new(p);
    d.add_break_cond(bp_addr, cond);
    let got = collect_cont_stops(&mut d, bp_addr);
    assert_eq!(got, expected, "conditional hits must match the raw pass exactly");

    // A conditional breakpoint that never fires must run to the halt.
    let mut d2 = Debugger::new(gen_loop_program(0xF005, iters));
    d2.add_break_cond(bp_addr, Expr::parse("globals[0] < 0").unwrap());
    assert_eq!(d2.cont(), StopReason::Halted);
    assert_eq!(d2.step_count(), total);
}

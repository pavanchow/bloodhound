# Bloodhound

A portable time-travel debugger. Bloodhound runs a small self-contained stack VM and lets you step backward through a program, scrub to any point on its execution timeline, and watch state change and un-change.

Live playground: https://pavanchow.github.io/bloodhound/

## What it is

Most debuggers only move forward. When you overshoot the moment a bug appears you restart and try again. Bloodhound records every instruction it runs, so you can step back, jump to an arbitrary earlier step, and reconstruct the exact machine state that forward execution had there. The stack, the memory, the locals, the call frames, and the program counter all rewind together.

Bloodhound debugs its own tiny stack VM rather than a native process. Attaching to real processes means ptrace on Linux, mach exceptions on macOS, and the debug API on Windows, none of which is portable and all of which needs privileged, platform-specific code. By shipping the machine it debugs, Bloodhound is one small Rust crate with zero dependencies that behaves identically everywhere, and its reverse execution can be checked by a machine oracle.

## The gap it fills

Reverse debuggers exist (rr, WinDbg time travel, gdb record) but they are heavy, platform bound, and hard to embed. Bloodhound is the opposite. It is a compact, dependency-free, fully deterministic reference implementation of the ideas: a bytecode VM, a per-instruction undo journal, and source-level stepping over reversible execution.

A person uses it to learn how time-travel debugging actually works, or as a teaching tool, because every layer is small enough to read in one sitting. An AI agent uses it as a safe, deterministic sandbox: it can generate a program, run it, set breakpoints and watchpoints, and step forward and backward to localise a fault, all through a stable API with no operating system entanglement and no flaky timing.

## Quickstart

```
cargo run                 # load the factorial sample into the REPL
cargo run -- demo         # run the scripted demonstration and exit
cargo run -- sum_loop     # load a built-in sample by name
cargo run -- list         # list the built-in samples
cargo run -- file prog.asm  # load an assembly file
cargo test                # run the unit tests and the correctness gate
```

Inside the REPL, type `help` for the full command list and `src` to see the loaded program.

## Debugger commands

| Command | What it does |
| --- | --- |
| `c` / `continue` | run forward to the next breakpoint or watchpoint |
| `rc` | reverse-continue back to the previous breakpoint |
| `s` / `step` | step to the next source line, descending into calls |
| `n` / `next` | step over, running any call to completion |
| `out` / `finish` | step out of the current function |
| `stepi` / `si` | execute a single instruction |
| `back` / `b` | step one instruction backward in time |
| `goto <step>` | jump to an absolute step index, forward or back |
| `break <line> [if <expr>]` | set a breakpoint on a source line, optionally conditional |
| `breaki <addr> [if <expr>]` | set a breakpoint on an instruction address, optionally conditional |
| `delete <line>` | remove a breakpoint on a source line |
| `watch g<i>` / `m<i>` / `l<i> [if <expr>]` | watch a global, memory cell, or local, optionally conditional |
| `eval <expr>` | evaluate an expression against the current machine state |
| `bt` / `where` | show the call stack |
| `p` / `print` | show full machine state |
| `reset` | restart at step 0 |
| `q` / `quit` | exit |

## The assembly language

Programs are written in a tiny assembly. Lines hold a label, a directive, an instruction, or a comment introduced by `;`.

```
.globals 2
main:
  push 0
  storeg 0       ; sum = 0
  push 1
  storeg 1       ; i = 1
loop:
  loadg 1
  push 5
  le
  jz done        ; while i <= 5
  loadg 0
  loadg 1
  add
  storeg 0       ; sum += i
  loadg 1
  push 1
  add
  storeg 1       ; i += 1
  jmp loop
done:
  loadg 0
  print
  halt
```

The full opcode set (arithmetic, comparisons, locals and globals, linear memory, branches, calls and returns, print) is documented in [DESIGN.md](DESIGN.md).

## Conditional breakpoints and data watches

Any breakpoint or watch can carry a condition written in a tiny expression language, separated from the command by `if`. A conditional breakpoint fires only when the condition holds at the stop, and a conditional watch fires only when the watched location changed and the condition holds on the state after the change.

```
break 7 if globals[0] > 100
breaki 12 if top < 0 && depth > 1
watch g0 if globals[0] % 2 == 1
```

The language has integer constants, the machine variables `pc`, `depth`, and `top` (the operand stack top, 0 when the stack is empty), the indexed reads `globals[e]` and `memory[e]`, arithmetic (`+ - * / %`), comparisons (`== != < <= > >=`), logic (`&& || !`), unary minus, and parentheses. Evaluation is total (division by zero yields 0, out-of-range globals read 0, memory indices wrap the way `loadmem` wraps) and strictly read-only, so evaluating a condition can never disturb the machine. Malformed expressions are rejected at parse time with the source position.

## The correctness gate

The debugger's claims are backed by a machine-checkable oracle in `tests/gate.rs`. It builds a ground-truth forward trace that snapshots the full VM state at every step, then checks each feature against it.

1. **Time-travel reversibility.** For random programs, stepping forward K instructions and then backward K returns a byte-identical state, and `goto(N)` reconstructs exactly the state forward execution had at step N. This proves reverse execution is a true inverse, not an approximation.
2. **Breakpoint correctness.** Continuing stops at exactly the steps whose program counter equals a breakpoint address and nowhere else, checked against every address in the program including addresses that are never reached.
3. **Watchpoint correctness.** A watchpoint fires on exactly the steps where the watched location changes, matching a reference diff of the per-step trace.
4. **Step-over and step-out semantics.** Step-over lands on the next source line in the same frame depth even across nested calls, and step-out lands right after the current frame returns, both verified from every executable step in the trace.
5. **Adversarial edge cases.** Breakpoints at address 0 and at the last instruction, a watch on a cell written by the watched instruction itself, step-out at the outermost frame, step-over on a call at the final instruction, `goto` to the current step, reverse at step 0, forward at the halted end, and an assembler negative battery (unknown opcodes, out-of-range immediates and branch targets, duplicate and numeric labels, missing operands, empty programs).
6. **Conditional breakpoints and expression watches.** Every conditional stop is compared against an independent plain-Rust predicate evaluated on the trace, in both continue and reverse-continue directions, and expression evaluation is proven never to mutate machine state.

The number of random programs is bounded for CI and controlled by an environment variable:

```
BLOODHOUND_FUZZ_OPS=400 cargo test    # more programs, longer run
```

Alongside the gate there are unit tests per module: assembler round-trip, each opcode, division by zero, memory round-trip, frame unwinding, and single-step reversibility.

## The stress suite

`tests/stress.rs` pushes the same oracles to max scale. It runs multi-million-step programs near the trace cap, verifies thousands of random-access `goto` scrubs against independently recorded anchor snapshots, drives thousands of alternating forward and backward moves, replays full breakpoint and watchpoint hit streams against a raw forward pass, and descends deep recursion stacks. The tests run in the default suite at a small size and reach max scale through environment variables, with no code change:

```
cargo test    # small scale, part of the default suite
BLOODHOUND_STRESS_ITERS=110000 BLOODHOUND_STRESS_SCRUBS=40000 \
  cargo test --release --test stress -- --nocapture --test-threads=1    # max scale
```

Scale knobs: `BLOODHOUND_STRESS_ITERS` (loop iterations per long program, default 400, max 110000), `BLOODHOUND_STRESS_SCRUBS` (random goto jumps, default 200, max 40000), `BLOODHOUND_STRESS_ALT` (alternating forward and backward moves, default 200, max 30000), and `BLOODHOUND_STRESS_DEPTH` (recursion depth, default 40, max 2000).

## Layout

- `src/vm.rs` the stack machine, opcode set, call frames, linear memory, and the undo journal.
- `src/asm.rs` the two-pass assembler and the source and line table.
- `src/expr.rs` the safe expression language for conditional breakpoints and data watches.
- `src/debugger.rs` breakpoints, watchpoints, stepping, and time travel.
- `src/samples.rs` the built-in example programs.
- `src/bin/bloodhound.rs` the command line REPL and the scripted demo.
- `docs/index.html` the browser playground, a faithful re-implementation.
- `tests/gate.rs` the correctness gate.
- `tests/stress.rs` the max-scale stress suite.

## License

MIT.

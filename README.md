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
| `break <line>` | set a breakpoint on a source line |
| `breaki <addr>` | set a breakpoint on an instruction address |
| `delete <line>` | remove a breakpoint on a source line |
| `watch g<i>` / `m<i>` / `l<i>` | watch a global, memory cell, or local |
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

## The correctness gate

The debugger's claims are backed by a machine-checkable oracle in `tests/gate.rs`. It builds a ground-truth forward trace that snapshots the full VM state at every step, then checks each feature against it.

1. **Time-travel reversibility.** For random programs, stepping forward K instructions and then backward K returns a byte-identical state, and `goto(N)` reconstructs exactly the state forward execution had at step N. This proves reverse execution is a true inverse, not an approximation.
2. **Breakpoint correctness.** Continuing stops at exactly the steps whose program counter equals a breakpoint address and nowhere else, checked against every address in the program including addresses that are never reached.
3. **Watchpoint correctness.** A watchpoint fires on exactly the steps where the watched location changes, matching a reference diff of the per-step trace.
4. **Step-over and step-out semantics.** Step-over lands on the next source line in the same frame depth even across nested calls, and step-out lands right after the current frame returns, both verified from every executable step in the trace.

The number of random programs is bounded for CI and controlled by an environment variable:

```
BLOODHOUND_FUZZ_OPS=400 cargo test    # more programs, longer run
```

Alongside the gate there are unit tests per module: assembler round-trip, each opcode, division by zero, memory round-trip, frame unwinding, and single-step reversibility.

## Layout

- `src/vm.rs` the stack machine, opcode set, call frames, linear memory, and the undo journal.
- `src/asm.rs` the two-pass assembler and the source and line table.
- `src/debugger.rs` breakpoints, watchpoints, stepping, and time travel.
- `src/samples.rs` the built-in example programs.
- `src/bin/bloodhound.rs` the command line REPL and the scripted demo.
- `docs/index.html` the browser playground, a faithful re-implementation.
- `tests/gate.rs` the correctness gate.

## License

MIT.

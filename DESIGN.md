# Bloodhound design

This document describes the architecture, the virtual machine and its bytecode, the debugger model, how reverse execution is implemented, and why each correctness gate proves what it claims.

## Overview

Bloodhound is four layers stacked on one another.

1. The **VM** (`src/vm.rs`) is a deterministic stack machine. It executes one instruction at a time and, crucially, each step produces a compact undo record.
2. The **assembler** (`src/asm.rs`) turns human readable source into a program and keeps a source and line table so the debugger can work at the level of source lines rather than raw addresses.
3. The **debugger** (`src/debugger.rs`) sits over the VM and adds breakpoints, watchpoints, source-level stepping, and time travel.
4. The **frontends** are a command line REPL (`src/bin/bloodhound.rs`) and a browser playground (`docs/index.html`).

The design choice that shapes everything is that Bloodhound debugs its own VM. Debugging a real native process requires privileged, per-platform mechanisms. A shipped VM is portable, deterministic, and, most importantly, its reverse execution can be verified against a ground-truth trace by an automated oracle.

## The virtual machine

The machine has these pieces of state.

- A **program counter**, an index into the flat instruction stream.
- A shared **operand stack** of 64-bit integers. Operands are pushed and popped here.
- A vector of **globals**, addressed by slot index.
- A **linear memory** of integer cells, addressed by a runtime value.
- A stack of **call frames**. Each frame owns its **locals**, remembers its **return address**, records the **operand-stack length** at the moment it was created, and carries a display name and call-site line for the backtrace.
- An **output** log, one entry per `print`.

Values are plain 64-bit integers. Keeping a single scalar type makes a machine state easy to snapshot and to compare for equality, which is what the reversibility oracle depends on.

### Totality

Every instruction is total. It never panics and never traps. Division or remainder by zero yields zero. Stack underflow reads zero. A memory address is reduced into range with Euclidean remainder. This matters for two reasons. First, the fuzz gate generates random programs, and a trap would turn a test into a crash rather than a checkable outcome. Second, a total step function has a clean inverse, which keeps the undo logic simple and provably correct.

### Bytecode

The opcode set is small and orthogonal.

- Stack: `push c`, `pop`, `dup`.
- Arithmetic: `add`, `sub`, `mul`, `div`, `mod`, `neg`, all using wrapping 64-bit semantics.
- Comparisons: `lt`, `gt`, `le`, `ge`, `eq`, `ne`, each pushing 1 or 0.
- Locals and globals: `load n`, `store n`, `loadg n`, `storeg n`.
- Memory: `loadmem` pops an address and pushes the cell, `storemem` pops a value and an address and writes the cell.
- Control: `jmp a`, `jz a`, `jnz a`.
- Functions: `call target nargs nlocals`, `ret`.
- Side effects and end: `print`, `halt`.

A `call` pops `nargs` values off the operand stack into the first locals of a fresh frame sized to `nlocals`, then jumps to the target. A `ret` pops the return value, drops the current frame, restores the caller program counter, and pushes the return value back onto the shared stack. Because the operand stack is shared across frames and each frame remembers its base, argument passing and returns need no copying between separate stacks.

### The assembler

Assembly is two passes. The first pass walks the source, resolves label definitions to instruction addresses, and applies the `.globals` and `.memory` directives. The second pass encodes each instruction, resolving jump and call targets that may be written as either a numeric address or a label name. Throughout, the assembler records `line_of[i]`, the 1-based source line that produced instruction `i`, and keeps the original source text. That map is the bridge from raw addresses to the lines a user reads, and it is what makes source-line breakpoints and line stepping possible.

## The debugger model

The debugger wraps one VM and one program and tracks the current step, a journal of undo records, a set of breakpoint addresses, and a list of watched locations.

### Breakpoints

A breakpoint is an instruction address. Continuing runs the VM forward and stops the moment the program counter about to execute equals a breakpoint. Source-line breakpoints resolve to the first instruction address that maps to that line. A halted machine is never a stop, because it is about to execute nothing.

A breakpoint may carry a **condition**, an expression over the machine state. A conditional breakpoint fires only when its condition evaluates to nonzero on the state at the stop, with the pc pointing at the breakpointed instruction. On a miss the machine runs on, so a condition that never holds turns `continue` into a run to the halt. Reverse-continue uses the same rule backward, which means the sequence of stops a reverse-continue visits is exactly the forward stop list played back in reverse, a property the gate checks.

### Watchpoints

A watchpoint names a global, a memory cell, or a local of the current frame. Before each forward step the debugger reads the watched values, and after the step it compares. A change is reported with the old value, the new value, and the step it happened on. When several watched locations change on one step the first in registration order is reported, which gives a single well defined event per step. A changed watch whose condition is false is skipped and the scan continues to the next watch, so one filtered watch can never mask a different watch that changed on the same step.

### The expression language

Conditions come from a small, safe expression language (`src/expr.rs`). It has integer constants, the machine variables `pc`, `depth`, and `top` (the operand stack top, 0 when the stack is empty), the indexed reads `globals[e]` and `memory[e]`, arithmetic (`+ - * / %`), comparisons (`== != < <= > >=`), logic (`&& || !` with short circuit), unary minus, and parentheses. Arithmetic uses the same wrapping 64-bit semantics as the VM, so an expression and the machine agree at the boundaries.

Evaluation is total and read-only. Division or remainder by zero yields 0, an out-of-range global index reads 0, and a memory index is reduced into range with Euclidean remainder, mirroring `loadmem` exactly, including wrapped negative addresses. The evaluator takes an immutable context built from the VM or from a snapshot, so evaluating a condition can never mutate machine state, and a malformed expression is rejected at parse time with the source position rather than at stop time.

### Stepping

All source-level stepping is built on the single-instruction step plus two facts recorded at the start line: the current source line and the current call depth.

- **Step into** advances until the source line changes at any depth, so it descends into callees.
- **Step over** advances until the source line changes and the call depth is no deeper than the start, so a call runs to completion and control resumes on the next line of the same function.
- **Step out** advances until the call depth becomes shallower than the start, so it lands in the caller right after the current frame returns.

Each of these also stops on a halt or a watchpoint. A safety limit bounds any run so a nonterminating program cannot hang the debugger.

## Reverse execution

Reverse execution is the point of the project, so it deserves the detail.

### How it is implemented: journaling in the engine

The Rust engine uses a per-instruction undo journal. Every time the VM executes an instruction it emits an `Undo` record that captures the minimum needed to reverse exactly that instruction:

- the program counter before the step,
- how many values the instruction pushed and which values it popped,
- any local, global, or memory write together with the old value at that location,
- whether a frame was pushed or, if one was popped, the whole popped frame,
- whether output was produced,
- the halted flag before the step.

To step back, the debugger pops the last record and applies its inverse: pop the values the instruction pushed, push back the values it popped, restore each written location to its old value, undo the frame change, drop any emitted output, and restore the program counter and halted flag. Because the forward step is total and the record captures every mutation, `undo(step(s))` equals `s` for every reachable state `s`.

`goto(N)` uses this in both directions. To move earlier it applies undo records until the step count reaches N. To move later it simply re-executes, which is safe because the VM is deterministic, so re-running reproduces the identical records and states.

### Cost

Journaling trades memory for the ability to reverse. Each executed instruction stores a small record whose size is proportional to what that instruction touched, typically a handful of integers, and a frame-sized record only on a return. So the memory cost is on the order of the number of instructions executed, and stepping back or forward by one is constant work. This is cheaper than snapshotting the entire machine on every step, which would cost the size of the whole state per step.

### The snapshot alternative, used in the playground

The browser playground (`docs/index.html`) is a faithful re-implementation that takes the other classic route. It runs the program once and deep-clones the full machine state after every step into a history array. Time travel is then a direct index into that array, and every motion command is index arithmetic over the history, which mirrors exactly how the correctness gate computes its expected answers. Snapshotting is simpler and gives instant scrubbing to any step, at the cost of storing a full state per step. Journaling is more memory efficient and is the better fit for a real engine. Presenting both makes the tradeoff concrete.

## Why each gate proves its claim

The gate in `tests/gate.rs` first builds a ground-truth trace by running a fresh VM and snapshotting the complete state at every step. The snapshot is an independent source of truth, computed by plain forward execution with no debugger involved, so every check compares the debugger against reality rather than against itself.

- **Reversibility.** Checking `goto(N)` against `trace[N]` for every N proves the debugger can reconstruct any historical state exactly. Separately, from random starting steps, forward K then back K is asserted equal to the starting snapshot, which proves reverse stepping is a true inverse of forward stepping and not a lossy approximation. Random programs exercise the full arithmetic set including division and remainder by zero, negation, locals, globals, memory writes and wrapped loads, a conditional branch in both polarities, and nested calls to depth three, so the undo logic is tested on every kind of mutation including frame pushes and pops.
- **Breakpoints.** The set of steps where continuing stops is compared to the set of steps whose program counter equals the breakpoint address, taken from the independent trace, for every address in the program and for an address that is never reached. Equality of the two sets rules out both missed stops and spurious stops.
- **Watchpoints.** The sequence of fired watchpoints is compared to a reference diff of the per-step trace using the same first-change-per-step rule the debugger uses. Matching sequences prove a watchpoint fires on exactly the steps where the value changes and never otherwise.
- **Step-over and step-out.** For every executable step, the landing step of each stepping mode is computed independently from the trace using the line and depth columns, then compared to where the debugger actually lands, including the full state at that point. Because the starting steps include the call sites, this specifically proves step-over skips nested calls and resumes at the right line and depth, and that step-out returns to the caller.
- **Adversarial edges.** The boundaries where an off-by-one would hide are each pinned by a dedicated test: a breakpoint at address 0 and at the last instruction, a watch on a cell the watched instruction itself writes, step-out at the outermost frame, step-over on a call that is the final instruction, `goto` to the step the machine already sits on, reverse at step 0, forward at the halted end, and a battery of malformed programs the assembler must reject cleanly with a source line.
- **Conditional breakpoints and expression watches.** For every arrival of a conditional breakpoint, an independent plain-Rust predicate written directly on the snapshot decides whether the stop should happen, and the two stop lists must be identical. The same construction covers conditional watches and reverse-continue with conditions. Because the reference predicate never goes through the expression evaluator, a bug in the evaluator or in the fire logic cannot hide behind itself.

## The stress suite

The gate bounds program size so CI stays fast. `tests/stress.rs` keeps the same oracles and removes the size bound. It runs looping programs near the two-million-step trace cap and verifies that `goto` to each of thousands of anchor steps reconstructs the exact independently recorded snapshot, that random forward-and-back cycles return byte-identical states, that a full reverse pass from the terminal state to step 0 lands on every anchor, that complete breakpoint and watchpoint hit streams over multi-million-step paths match a raw forward pass with no debugger involved, that breakpoint alignment survives thousands of random `goto` jumps, and that deep recursion pushes and pops thousands of frames reversibly. The suite runs in the default test command at a small scale and reaches max scale through environment variables, so the same code is verified at both scales.

Together the independent trace and these four checks turn the phrase "time-travel debugger" into a property that either holds for every generated program or fails loudly with the exact program and step that broke it.

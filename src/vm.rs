//! The Bloodhound stack virtual machine.
//!
//! A program is a flat vector of [`Op`]. Execution state is a shared operand
//! stack, a vector of globals, a linear memory, and a stack of call [`Frame`]s
//! each holding its own locals. Every instruction is total (it never panics and
//! never traps): division by zero yields zero and stack underflow reads zero, so
//! that randomly generated programs used by the fuzz gate always make progress
//! and are always reversible.

use std::fmt;

/// The default number of global slots when a program does not request more.
pub const DEFAULT_GLOBALS: usize = 16;
/// The default number of linear-memory cells when a program does not request more.
pub const DEFAULT_MEMORY: usize = 64;

/// The instruction set of the stack VM.
///
/// Addresses are instruction indices into [`Program::code`]. Local, global and
/// memory operands are slot indices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Push a constant onto the operand stack.
    Push(i64),
    /// Pop and discard the top of stack.
    Pop,
    /// Duplicate the top of stack.
    Dup,
    /// Pop b, pop a, push a + b.
    Add,
    /// Pop b, pop a, push a - b.
    Sub,
    /// Pop b, pop a, push a * b.
    Mul,
    /// Pop b, pop a, push a / b (b == 0 yields 0).
    Div,
    /// Pop b, pop a, push a % b (b == 0 yields 0).
    Mod,
    /// Pop a, push -a.
    Neg,
    /// Pop b, pop a, push 1 if a < b else 0.
    Lt,
    /// Pop b, pop a, push 1 if a > b else 0.
    Gt,
    /// Pop b, pop a, push 1 if a <= b else 0.
    Le,
    /// Pop b, pop a, push 1 if a >= b else 0.
    Ge,
    /// Pop b, pop a, push 1 if a == b else 0.
    Eq,
    /// Pop b, pop a, push 1 if a != b else 0.
    Ne,
    /// Push the value of local slot n in the current frame.
    Load(usize),
    /// Pop and store into local slot n of the current frame.
    Store(usize),
    /// Push the value of global slot n.
    LoadG(usize),
    /// Pop and store into global slot n.
    StoreG(usize),
    /// Pop an address, push memory at that address.
    LoadMem,
    /// Pop a value, pop an address, store the value at that address.
    StoreMem,
    /// Unconditional jump to an address.
    Jmp(usize),
    /// Pop a value, jump if it is zero.
    Jz(usize),
    /// Pop a value, jump if it is non-zero.
    Jnz(usize),
    /// Call a function: (target, nargs, nlocals). Pops nargs values into the new
    /// frame's first locals; the frame has nlocals slots in total.
    Call(usize, usize, usize),
    /// Return: pop the return value, drop the current frame, push the value back.
    Ret,
    /// Pop a value and append it to the program output.
    Print,
    /// Stop execution.
    Halt,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Op::Push(v) => write!(f, "push {v}"),
            Op::Pop => write!(f, "pop"),
            Op::Dup => write!(f, "dup"),
            Op::Add => write!(f, "add"),
            Op::Sub => write!(f, "sub"),
            Op::Mul => write!(f, "mul"),
            Op::Div => write!(f, "div"),
            Op::Mod => write!(f, "mod"),
            Op::Neg => write!(f, "neg"),
            Op::Lt => write!(f, "lt"),
            Op::Gt => write!(f, "gt"),
            Op::Le => write!(f, "le"),
            Op::Ge => write!(f, "ge"),
            Op::Eq => write!(f, "eq"),
            Op::Ne => write!(f, "ne"),
            Op::Load(n) => write!(f, "load {n}"),
            Op::Store(n) => write!(f, "store {n}"),
            Op::LoadG(n) => write!(f, "loadg {n}"),
            Op::StoreG(n) => write!(f, "storeg {n}"),
            Op::LoadMem => write!(f, "loadmem"),
            Op::StoreMem => write!(f, "storemem"),
            Op::Jmp(a) => write!(f, "jmp {a}"),
            Op::Jz(a) => write!(f, "jz {a}"),
            Op::Jnz(a) => write!(f, "jnz {a}"),
            Op::Call(t, na, nl) => write!(f, "call {t} {na} {nl}"),
            Op::Ret => write!(f, "ret"),
            Op::Print => write!(f, "print"),
            Op::Halt => write!(f, "halt"),
        }
    }
}

/// A loaded program: code, the source it came from, and the instruction to
/// source-line map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    /// The flat instruction stream.
    pub code: Vec<Op>,
    /// `line_of[i]` is the 1-based source line that produced `code[i]`.
    pub line_of: Vec<u32>,
    /// The original source text, split into lines (0-based index, 1-based line).
    pub source: Vec<String>,
    /// A human readable name per code address, used for the backtrace. Filled in
    /// for function entry points, empty otherwise.
    pub labels: Vec<Option<String>>,
    /// Requested number of global slots.
    pub globals: usize,
    /// Requested number of linear memory cells.
    pub memory: usize,
}

impl Program {
    /// The 1-based source line for an instruction address, or 0 if out of range.
    pub fn line_at(&self, pc: usize) -> u32 {
        self.line_of.get(pc).copied().unwrap_or(0)
    }
}

/// A single call frame. Locals are private to the frame; the operand stack is
/// shared across frames but partitioned by [`Frame::stack_base`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// A display name for the function this frame is executing.
    pub func: String,
    /// The address to return to when this frame returns.
    pub return_pc: usize,
    /// The frame's local variable slots.
    pub locals: Vec<i64>,
    /// The operand-stack length at the moment this frame was created.
    pub stack_base: usize,
    /// The source line of the call site (0 for the entry frame).
    pub call_line: u32,
}

/// Which side of the frame stack an instruction touched, recorded for undo.
#[derive(Clone, Debug)]
enum FrameDelta {
    None,
    Pushed,
    Popped(Frame),
}

/// A memory location an instruction may have written, recorded with its old
/// value so the write can be reversed.
#[derive(Clone, Copy, Debug)]
enum Loc {
    Local(usize),
    Global(usize),
    Mem(usize),
}

/// A compact undo record: everything needed to reverse exactly one instruction.
#[derive(Clone, Debug)]
pub struct Undo {
    pc_before: usize,
    pushed: usize,
    popped: Vec<i64>,
    writes: Vec<(Loc, i64)>,
    frame: FrameDelta,
    printed: bool,
    halted_before: bool,
}

/// A comparable snapshot of the full observable machine state, used by the tests
/// to verify that time travel reconstructs state exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The program counter.
    pub pc: usize,
    /// Whether the machine has halted.
    pub halted: bool,
    /// The operand stack, bottom to top.
    pub stack: Vec<i64>,
    /// The global slots.
    pub globals: Vec<i64>,
    /// The linear memory.
    pub memory: Vec<i64>,
    /// The call frames, entry frame first.
    pub frames: Vec<Frame>,
    /// The accumulated program output.
    pub output: Vec<String>,
}

/// The stack virtual machine.
#[derive(Clone, Debug)]
pub struct Vm {
    /// The program counter (index into the code).
    pub pc: usize,
    /// True once a `halt` has executed.
    pub halted: bool,
    /// The shared operand stack.
    pub stack: Vec<i64>,
    /// The global slots.
    pub globals: Vec<i64>,
    /// The linear memory cells.
    pub memory: Vec<i64>,
    /// The call frame stack. Always non-empty during a run (entry frame at 0).
    pub frames: Vec<Frame>,
    /// Accumulated `print` output, one entry per print.
    pub output: Vec<String>,
    code: Vec<Op>,
    lines: Vec<u32>,
    names: Vec<Option<String>>,
}

impl Vm {
    /// Create a VM ready to run `program` from address 0 with a single entry
    /// frame.
    pub fn new(program: &Program) -> Self {
        let entry = Frame {
            func: "main".to_string(),
            return_pc: usize::MAX,
            locals: vec![0; 8],
            stack_base: 0,
            call_line: 0,
        };
        Vm {
            pc: 0,
            halted: false,
            stack: Vec::new(),
            globals: vec![0; program.globals.max(1)],
            memory: vec![0; program.memory.max(1)],
            frames: vec![entry],
            output: Vec::new(),
            code: program.code.clone(),
            lines: program.line_of.clone(),
            names: program.labels.clone(),
        }
    }

    /// The current call depth (number of active frames).
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The op about to execute, if the pc is in range and not halted.
    pub fn current_op(&self) -> Option<&Op> {
        if self.halted {
            None
        } else {
            self.code.get(self.pc)
        }
    }

    /// A comparable snapshot of the full observable state.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            pc: self.pc,
            halted: self.halted,
            stack: self.stack.clone(),
            globals: self.globals.clone(),
            memory: self.memory.clone(),
            frames: self.frames.clone(),
            output: self.output.clone(),
        }
    }

    /// Read a local of the current frame (0 if out of range).
    pub fn local(&self, i: usize) -> i64 {
        self.frames.last().and_then(|f| f.locals.get(i)).copied().unwrap_or(0)
    }

    fn spop(&mut self, u: &mut Undo) -> i64 {
        match self.stack.pop() {
            Some(v) => {
                u.popped.push(v);
                v
            }
            None => 0,
        }
    }

    fn spush(&mut self, u: &mut Undo, v: i64) {
        self.stack.push(v);
        u.pushed += 1;
    }

    fn write_local(&mut self, u: &mut Undo, i: usize, v: i64) {
        if let Some(f) = self.frames.last_mut() {
            if i < f.locals.len() {
                u.writes.push((Loc::Local(i), f.locals[i]));
                f.locals[i] = v;
            }
        }
    }

    fn write_global(&mut self, u: &mut Undo, i: usize, v: i64) {
        if i < self.globals.len() {
            u.writes.push((Loc::Global(i), self.globals[i]));
            self.globals[i] = v;
        }
    }

    fn write_mem(&mut self, u: &mut Undo, i: usize, v: i64) {
        if i < self.memory.len() {
            u.writes.push((Loc::Mem(i), self.memory[i]));
            self.memory[i] = v;
        }
    }

    /// Execute a single instruction, returning the undo record for it. Returns
    /// `None` if the machine is already halted or the pc is out of range.
    pub fn step(&mut self) -> Option<Undo> {
        if self.halted {
            return None;
        }
        let op = self.code.get(self.pc)?.clone();
        let mut u = Undo {
            pc_before: self.pc,
            pushed: 0,
            popped: Vec::new(),
            writes: Vec::new(),
            frame: FrameDelta::None,
            printed: false,
            halted_before: self.halted,
        };
        let mut next = self.pc + 1;
        match op {
            Op::Push(v) => self.spush(&mut u, v),
            Op::Pop => {
                self.spop(&mut u);
            }
            Op::Dup => {
                let v = self.spop(&mut u);
                self.spush(&mut u, v);
                self.spush(&mut u, v);
            }
            Op::Add => {
                let b = self.spop(&mut u);
                let a = self.spop(&mut u);
                self.spush(&mut u, a.wrapping_add(b));
            }
            Op::Sub => {
                let b = self.spop(&mut u);
                let a = self.spop(&mut u);
                self.spush(&mut u, a.wrapping_sub(b));
            }
            Op::Mul => {
                let b = self.spop(&mut u);
                let a = self.spop(&mut u);
                self.spush(&mut u, a.wrapping_mul(b));
            }
            Op::Div => {
                let b = self.spop(&mut u);
                let a = self.spop(&mut u);
                let r = if b == 0 { 0 } else { a.wrapping_div(b) };
                self.spush(&mut u, r);
            }
            Op::Mod => {
                let b = self.spop(&mut u);
                let a = self.spop(&mut u);
                let r = if b == 0 { 0 } else { a.wrapping_rem(b) };
                self.spush(&mut u, r);
            }
            Op::Neg => {
                let a = self.spop(&mut u);
                self.spush(&mut u, a.wrapping_neg());
            }
            Op::Lt => self.cmp(&mut u, |a, b| a < b),
            Op::Gt => self.cmp(&mut u, |a, b| a > b),
            Op::Le => self.cmp(&mut u, |a, b| a <= b),
            Op::Ge => self.cmp(&mut u, |a, b| a >= b),
            Op::Eq => self.cmp(&mut u, |a, b| a == b),
            Op::Ne => self.cmp(&mut u, |a, b| a != b),
            Op::Load(i) => {
                let v = self.local(i);
                self.spush(&mut u, v);
            }
            Op::Store(i) => {
                let v = self.spop(&mut u);
                self.write_local(&mut u, i, v);
            }
            Op::LoadG(i) => {
                let v = self.globals.get(i).copied().unwrap_or(0);
                self.spush(&mut u, v);
            }
            Op::StoreG(i) => {
                let v = self.spop(&mut u);
                self.write_global(&mut u, i, v);
            }
            Op::LoadMem => {
                let addr = self.spop(&mut u);
                let idx = self.mem_index(addr);
                let v = self.memory.get(idx).copied().unwrap_or(0);
                self.spush(&mut u, v);
            }
            Op::StoreMem => {
                let v = self.spop(&mut u);
                let addr = self.spop(&mut u);
                let idx = self.mem_index(addr);
                self.write_mem(&mut u, idx, v);
            }
            Op::Jmp(a) => next = a,
            Op::Jz(a) => {
                if self.spop(&mut u) == 0 {
                    next = a;
                }
            }
            Op::Jnz(a) => {
                if self.spop(&mut u) != 0 {
                    next = a;
                }
            }
            Op::Call(target, nargs, nlocals) => {
                let mut args = vec![0i64; nlocals.max(nargs)];
                for k in (0..nargs).rev() {
                    args[k] = self.spop(&mut u);
                }
                let name = self
                    .names
                    .get(target)
                    .and_then(|o| o.clone())
                    .unwrap_or_else(|| format!("fn@{target}"));
                let frame = Frame {
                    func: name,
                    return_pc: self.pc + 1,
                    locals: args,
                    stack_base: self.stack.len(),
                    call_line: self.current_line(),
                };
                self.frames.push(frame);
                u.frame = FrameDelta::Pushed;
                next = target;
            }
            Op::Ret => {
                let rv = self.spop(&mut u);
                if self.frames.len() > 1 {
                    let f = self.frames.pop().expect("frame present");
                    next = f.return_pc;
                    u.frame = FrameDelta::Popped(f);
                    self.spush(&mut u, rv);
                } else {
                    // Returning from the entry frame ends the program.
                    self.spush(&mut u, rv);
                    self.halted = true;
                    next = self.pc;
                }
            }
            Op::Print => {
                let v = self.spop(&mut u);
                self.output.push(v.to_string());
                u.printed = true;
            }
            Op::Halt => {
                self.halted = true;
                next = self.pc;
            }
        }
        self.pc = next;
        Some(u)
    }

    fn current_line(&self) -> u32 {
        self.lines.get(self.pc).copied().unwrap_or(0)
    }

    fn mem_index(&self, addr: i64) -> usize {
        let n = self.memory.len() as i64;
        if n == 0 {
            0
        } else {
            addr.rem_euclid(n) as usize
        }
    }

    fn cmp(&mut self, u: &mut Undo, f: impl Fn(i64, i64) -> bool) {
        let b = self.spop(u);
        let a = self.spop(u);
        self.spush(u, if f(a, b) { 1 } else { 0 });
    }

    /// Reverse a single instruction using its undo record. This is the inverse
    /// of [`Vm::step`]: applying `undo(step(s)) == s` for every reachable state.
    pub fn undo(&mut self, u: Undo) {
        for _ in 0..u.pushed {
            self.stack.pop();
        }
        for &v in u.popped.iter().rev() {
            self.stack.push(v);
        }
        for &(loc, old) in u.writes.iter().rev() {
            match loc {
                Loc::Local(i) => {
                    if let Some(f) = self.frames.last_mut() {
                        if i < f.locals.len() {
                            f.locals[i] = old;
                        }
                    }
                }
                Loc::Global(i) => self.globals[i] = old,
                Loc::Mem(i) => self.memory[i] = old,
            }
        }
        match u.frame {
            FrameDelta::None => {}
            FrameDelta::Pushed => {
                self.frames.pop();
            }
            FrameDelta::Popped(f) => self.frames.push(f),
        }
        if u.printed {
            self.output.pop();
        }
        self.halted = u.halted_before;
        self.pc = u.pc_before;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(code: Vec<Op>) -> Program {
        let n = code.len();
        Program {
            code,
            line_of: vec![1; n],
            source: vec!["<test>".to_string()],
            labels: vec![None; n],
            globals: DEFAULT_GLOBALS,
            memory: DEFAULT_MEMORY,
        }
    }

    #[test]
    fn arithmetic_ops() {
        let p = prog(vec![Op::Push(6), Op::Push(7), Op::Mul, Op::Print, Op::Halt]);
        let mut vm = Vm::new(&p);
        while vm.step().is_some() {}
        assert_eq!(vm.output, vec!["42".to_string()]);
    }

    #[test]
    fn div_and_mod_by_zero_are_zero() {
        let p = prog(vec![Op::Push(5), Op::Push(0), Op::Div, Op::Push(5), Op::Push(0), Op::Mod, Op::Halt]);
        let mut vm = Vm::new(&p);
        while vm.step().is_some() {}
        assert_eq!(vm.stack, vec![0, 0]);
    }

    #[test]
    fn globals_and_memory_roundtrip() {
        let p = prog(vec![
            Op::Push(99),
            Op::StoreG(3),
            Op::LoadG(3),
            Op::Push(7),
            Op::Push(123),
            Op::StoreMem, // mem[7] = 123
            Op::Push(7),
            Op::LoadMem,
            Op::Halt,
        ]);
        let mut vm = Vm::new(&p);
        while vm.step().is_some() {}
        assert_eq!(vm.globals[3], 99);
        assert_eq!(vm.memory[7], 123);
        assert_eq!(vm.stack, vec![99, 123]);
    }

    #[test]
    fn call_and_ret_unwind() {
        // main: push 10, call double, print, halt ; double: load0, dup, add, ret
        let p = prog(vec![
            Op::Push(10),      // 0
            Op::Call(5, 1, 1), // 1 -> double
            Op::Print,         // 2
            Op::Halt,          // 3
            Op::Halt,          // 4 padding
            Op::Load(0),       // 5 double
            Op::Dup,           // 6
            Op::Add,           // 7
            Op::Ret,           // 8
        ]);
        let mut vm = Vm::new(&p);
        while vm.step().is_some() {}
        assert_eq!(vm.output, vec!["20".to_string()]);
    }

    #[test]
    fn every_step_is_reversible() {
        let p = prog(vec![
            Op::Push(3),
            Op::Push(4),
            Op::Add,
            Op::StoreG(0),
            Op::LoadG(0),
            Op::Push(2),
            Op::Mul,
            Op::Print,
            Op::Halt,
        ]);
        let mut vm = Vm::new(&p);
        let before = vm.snapshot();
        let u = vm.step().unwrap();
        vm.undo(u);
        assert_eq!(vm.snapshot(), before);
    }
}

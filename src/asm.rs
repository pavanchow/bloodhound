//! A tiny two-pass assembler for the Bloodhound VM.
//!
//! The text format is line oriented. A line may hold a label definition, a
//! directive, an instruction, a comment, or be blank. Instructions map back to
//! the source line they came from, which is what gives the debugger source-level
//! stepping and breakpoints.
//!
//! Grammar (informal):
//! - `; comment` to end of line
//! - `.globals N` and `.memory N` size the machine
//! - `name:` defines a label at the address of the next instruction
//! - `push 5`, `add`, `jmp loop`, `call double 1 1`, `store 0`, ... instructions
//!
//! Jump and call targets may be integer addresses or label names.

use crate::vm::{Op, Program, DEFAULT_GLOBALS, DEFAULT_MEMORY};
use std::collections::HashMap;

/// An assembly error tied to the 1-based source line that caused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsmError {
    /// The 1-based source line the error was found on.
    pub line: u32,
    /// A human readable description.
    pub message: String,
}

impl std::fmt::Display for AsmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for AsmError {}

fn strip_comment(line: &str) -> &str {
    match line.find(';') {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Assemble source text into a [`Program`].
pub fn assemble(src: &str) -> Result<Program, AsmError> {
    let source: Vec<String> = src.lines().map(|s| s.to_string()).collect();

    let mut globals = DEFAULT_GLOBALS;
    let mut memory = DEFAULT_MEMORY;

    // Pass 1: resolve labels to instruction addresses and count instructions.
    let mut labels: HashMap<String, usize> = HashMap::new();
    let mut label_at_addr: Vec<Option<String>> = Vec::new();
    let mut instr_count = 0usize;
    let mut pending_label: Option<String> = None;

    for (idx, raw) in source.iter().enumerate() {
        let lineno = (idx + 1) as u32;
        let content = strip_comment(raw).trim();
        if content.is_empty() {
            continue;
        }
        if let Some(name) = content.strip_suffix(':') {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(err(lineno, "empty label"));
            }
            if labels.contains_key(&name) {
                return Err(err(lineno, format!("duplicate label `{name}`")));
            }
            labels.insert(name.clone(), instr_count);
            pending_label = Some(name);
            continue;
        }
        if let Some(rest) = content.strip_prefix('.') {
            apply_directive(rest, lineno, &mut globals, &mut memory)?;
            continue;
        }
        // An instruction line consumes any pending label.
        label_at_addr.push(pending_label.take());
        instr_count += 1;
    }

    // Pass 2: encode instructions, resolving operands.
    let mut code: Vec<Op> = Vec::with_capacity(instr_count);
    let mut line_of: Vec<u32> = Vec::with_capacity(instr_count);

    for (idx, raw) in source.iter().enumerate() {
        let lineno = (idx + 1) as u32;
        let content = strip_comment(raw).trim();
        if content.is_empty() || content.ends_with(':') || content.starts_with('.') {
            continue;
        }
        let op = parse_instr(content, lineno, &labels)?;
        code.push(op);
        line_of.push(lineno);
    }

    if code.is_empty() {
        return Err(err(1, "program has no instructions"));
    }

    Ok(Program {
        code,
        line_of,
        source,
        labels: label_at_addr,
        globals,
        memory,
    })
}

fn apply_directive(rest: &str, lineno: u32, globals: &mut usize, memory: &mut usize) -> Result<(), AsmError> {
    let mut it = rest.split_whitespace();
    let name = it.next().unwrap_or("");
    let val = it.next();
    match name {
        "globals" => *globals = parse_usize(val, lineno, "globals")?,
        "memory" => *memory = parse_usize(val, lineno, "memory")?,
        other => return Err(err(lineno, format!("unknown directive `.{other}`"))),
    }
    Ok(())
}

fn parse_instr(content: &str, lineno: u32, labels: &HashMap<String, usize>) -> Result<Op, AsmError> {
    let mut it = content.split_whitespace();
    let mnem = it.next().unwrap_or("");
    let args: Vec<&str> = it.collect();

    let op = match mnem {
        "push" => Op::Push(int_arg(&args, 0, lineno)?),
        "pop" => Op::Pop,
        "dup" => Op::Dup,
        "add" => Op::Add,
        "sub" => Op::Sub,
        "mul" => Op::Mul,
        "div" => Op::Div,
        "mod" => Op::Mod,
        "neg" => Op::Neg,
        "lt" => Op::Lt,
        "gt" => Op::Gt,
        "le" => Op::Le,
        "ge" => Op::Ge,
        "eq" => Op::Eq,
        "ne" => Op::Ne,
        "load" => Op::Load(uint_arg(&args, 0, lineno)?),
        "store" => Op::Store(uint_arg(&args, 0, lineno)?),
        "loadg" => Op::LoadG(uint_arg(&args, 0, lineno)?),
        "storeg" => Op::StoreG(uint_arg(&args, 0, lineno)?),
        "loadmem" => Op::LoadMem,
        "storemem" => Op::StoreMem,
        "jmp" => Op::Jmp(addr_arg(&args, 0, lineno, labels)?),
        "jz" => Op::Jz(addr_arg(&args, 0, lineno, labels)?),
        "jnz" => Op::Jnz(addr_arg(&args, 0, lineno, labels)?),
        "call" => {
            let target = addr_arg(&args, 0, lineno, labels)?;
            let nargs = uint_arg(&args, 1, lineno)?;
            let nlocals = uint_arg(&args, 2, lineno)?;
            Op::Call(target, nargs, nlocals)
        }
        "ret" => Op::Ret,
        "print" => Op::Print,
        "halt" => Op::Halt,
        other => return Err(err(lineno, format!("unknown mnemonic `{other}`"))),
    };
    Ok(op)
}

fn int_arg(args: &[&str], i: usize, lineno: u32) -> Result<i64, AsmError> {
    let s = args.get(i).ok_or_else(|| err(lineno, "missing integer operand"))?;
    s.parse::<i64>().map_err(|_| err(lineno, format!("invalid integer `{s}`")))
}

fn uint_arg(args: &[&str], i: usize, lineno: u32) -> Result<usize, AsmError> {
    let s = args.get(i).ok_or_else(|| err(lineno, "missing operand"))?;
    s.parse::<usize>().map_err(|_| err(lineno, format!("invalid slot index `{s}`")))
}

fn addr_arg(args: &[&str], i: usize, lineno: u32, labels: &HashMap<String, usize>) -> Result<usize, AsmError> {
    let s = args.get(i).ok_or_else(|| err(lineno, "missing target operand"))?;
    if let Ok(n) = s.parse::<usize>() {
        return Ok(n);
    }
    labels
        .get(*s)
        .copied()
        .ok_or_else(|| err(lineno, format!("unknown label `{s}`")))
}

fn parse_usize(val: Option<&str>, lineno: u32, what: &str) -> Result<usize, AsmError> {
    let s = val.ok_or_else(|| err(lineno, format!("directive `.{what}` needs a value")))?;
    s.parse::<usize>().map_err(|_| err(lineno, format!("invalid `.{what}` value `{s}`")))
}

fn err(line: u32, message: impl Into<String>) -> AsmError {
    AsmError {
        line,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_source_and_lines() {
        let src = "\
; a program
.globals 4
.memory 8
start:
  push 2
  push 3
  add
  print
  halt
";
        let p = assemble(src).unwrap();
        assert_eq!(p.globals, 4);
        assert_eq!(p.memory, 8);
        assert_eq!(p.code, vec![Op::Push(2), Op::Push(3), Op::Add, Op::Print, Op::Halt]);
        // Instruction addresses map back to the correct 1-based source lines.
        assert_eq!(p.line_of, vec![5, 6, 7, 8, 9]);
        // The `start` label attaches to the first instruction (address 0).
        assert_eq!(p.labels[0].as_deref(), Some("start"));
    }

    #[test]
    fn label_targets_resolve() {
        let src = "\
loop:
  push 0
  jz loop
  halt
";
        let p = assemble(src).unwrap();
        assert_eq!(p.code[1], Op::Jz(0));
    }

    #[test]
    fn call_syntax() {
        let src = "\
  call helper 2 3
  halt
helper:
  ret
";
        let p = assemble(src).unwrap();
        assert_eq!(p.code[0], Op::Call(2, 2, 3));
    }

    #[test]
    fn errors_report_lines() {
        let e = assemble("  push\n").unwrap_err();
        assert_eq!(e.line, 1);
        let e2 = assemble("  jmp nowhere\n  halt\n").unwrap_err();
        assert_eq!(e2.line, 1);
        assert!(e2.message.contains("nowhere"));
    }

    #[test]
    fn disassembly_matches_source_ops() {
        let p = assemble("  push 5\n  neg\n  print\n  halt\n").unwrap();
        let text: Vec<String> = p.code.iter().map(|o| o.to_string()).collect();
        assert_eq!(text, vec!["push 5", "neg", "print", "halt"]);
    }
}

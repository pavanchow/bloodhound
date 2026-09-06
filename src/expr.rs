//! A tiny, safe expression language for conditional breakpoints and data
//! watches.
//!
//! The language has integer constants, the machine variables `pc`, `depth` and
//! `top` (the operand stack top, 0 when the stack is empty), the indexed reads
//! `globals[e]` and `memory[e]`, arithmetic (`+ - * / %`), comparisons
//! (`== != < <= > >=`), logic (`&& || !`, with short circuit), unary minus, and
//! parentheses. Arithmetic uses the same wrapping 64-bit semantics as the VM.
//! Evaluation is total: division or remainder by zero yields 0, an index into
//! globals out of range reads 0, and a memory index is reduced into range with
//! Euclidean remainder, mirroring `loadmem`. Comparisons and logic yield 1 or
//! 0, and a condition is true when it evaluates to nonzero.
//!
//! Malformed text is rejected cleanly at parse time with the source position.
//! Evaluation only reads machine state and can never mutate it.

use crate::vm::{Snapshot, Vm};
use std::fmt;

/// A parse error, with a human readable message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExprError {
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExprError {}

fn err(message: impl Into<String>) -> ExprError {
    ExprError {
        message: message.into(),
    }
}

/// The machine state an expression is evaluated against. Built from a VM or a
/// snapshot, it is read-only, so evaluation can never mutate anything.
#[derive(Clone, Debug)]
pub struct EvalCtx<'a> {
    /// The program counter.
    pub pc: usize,
    /// The number of active call frames.
    pub depth: usize,
    /// The operand stack top (0 when the stack is empty).
    pub top: i64,
    /// The global slots.
    pub globals: &'a [i64],
    /// The linear memory cells.
    pub memory: &'a [i64],
}

impl<'a> EvalCtx<'a> {
    /// Build the context for the current state of a VM.
    #[must_use]
    pub fn of_vm(vm: &'a Vm) -> EvalCtx<'a> {
        EvalCtx {
            pc: vm.pc,
            depth: vm.depth(),
            top: vm.stack.last().copied().unwrap_or(0),
            globals: &vm.globals,
            memory: &vm.memory,
        }
    }

    /// Build the context for a recorded snapshot.
    #[must_use]
    pub fn of_snapshot(snap: &'a Snapshot) -> EvalCtx<'a> {
        EvalCtx {
            pc: snap.pc,
            depth: snap.frames.len(),
            top: snap.stack.last().copied().unwrap_or(0),
            globals: &snap.globals,
            memory: &snap.memory,
        }
    }
}

/// A parsed expression tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// An integer constant.
    Const(i64),
    /// The program counter.
    Pc,
    /// The call depth.
    Depth,
    /// The operand stack top.
    Top,
    /// `globals[e]`, 0 when the index is out of range.
    Global(Box<Expr>),
    /// `memory[e]`, reduced into range like `loadmem`.
    Memory(Box<Expr>),
    /// Logical not, 1 when the operand is zero.
    Not(Box<Expr>),
    /// Arithmetic negation, wrapping.
    Neg(Box<Expr>),
    /// Wrapping addition.
    Add(Box<Expr>, Box<Expr>),
    /// Wrapping subtraction.
    Sub(Box<Expr>, Box<Expr>),
    /// Wrapping multiplication.
    Mul(Box<Expr>, Box<Expr>),
    /// Division, 0 when the divisor is 0.
    Div(Box<Expr>, Box<Expr>),
    /// Remainder, 0 when the divisor is 0.
    Mod(Box<Expr>, Box<Expr>),
    /// 1 when a < b.
    Lt(Box<Expr>, Box<Expr>),
    /// 1 when a <= b.
    Le(Box<Expr>, Box<Expr>),
    /// 1 when a > b.
    Gt(Box<Expr>, Box<Expr>),
    /// 1 when a >= b.
    Ge(Box<Expr>, Box<Expr>),
    /// 1 when a == b.
    Eq(Box<Expr>, Box<Expr>),
    /// 1 when a != b.
    Ne(Box<Expr>, Box<Expr>),
    /// Logical and, short circuit, 1 or 0.
    And(Box<Expr>, Box<Expr>),
    /// Logical or, short circuit, 1 or 0.
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Int(i64),
    Pc,
    Depth,
    Top,
    Globals,
    Memory,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    LParen,
    RParen,
    LBracket,
    RBracket,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, ExprError> {
    let mut toks = Vec::new();
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let text: String = bytes[start..i].iter().collect();
            let v: i64 = text.parse().map_err(|_| {
                err(format!("integer literal `{text}` is out of range"))
            })?;
            toks.push(Tok::Int(v));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                i += 1;
            }
            let word: String = bytes[start..i].iter().collect();
            let tok = match word.as_str() {
                "pc" => Tok::Pc,
                "depth" => Tok::Depth,
                "top" => Tok::Top,
                "globals" => Tok::Globals,
                "memory" => Tok::Memory,
                other => {
                    return Err(err(format!(
                        "unknown variable `{other}` (available: pc, depth, top, globals[e], memory[e])"
                    )))
                }
            };
            toks.push(tok);
            continue;
        }
        let two: String = bytes[i..(i + 2).min(bytes.len())].iter().collect();
        let (tok, len) = match two.as_str() {
            "==" => (Tok::EqEq, 2),
            "!=" => (Tok::Ne, 2),
            "<=" => (Tok::Le, 2),
            ">=" => (Tok::Ge, 2),
            "&&" => (Tok::AndAnd, 2),
            "||" => (Tok::OrOr, 2),
            _ => match c {
                '+' => (Tok::Plus, 1),
                '-' => (Tok::Minus, 1),
                '*' => (Tok::Star, 1),
                '/' => (Tok::Slash, 1),
                '%' => (Tok::Percent, 1),
                '<' => (Tok::Lt, 1),
                '>' => (Tok::Gt, 1),
                '!' => (Tok::Bang, 1),
                '(' => (Tok::LParen, 1),
                ')' => (Tok::RParen, 1),
                '[' => (Tok::LBracket, 1),
                ']' => (Tok::RBracket, 1),
                other => return Err(err(format!("unexpected character `{other}`"))),
            },
        };
        i += len;
        toks.push(tok);
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).copied();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, t: Tok, what: &str) -> Result<(), ExprError> {
        match self.bump() {
            Some(got) if got == t => Ok(()),
            Some(got) => Err(err(format!("expected {what}, found `{got:?}`"))),
            None => Err(err(format!("expected {what}, found end of expression"))),
        }
    }

    fn parse_or(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&Tok::OrOr) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_cmp()?;
        while self.peek() == Some(&Tok::AndAnd) {
            self.bump();
            let rhs = self.parse_cmp()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Some(Tok::EqEq) => Expr::Eq as fn(_, _) -> _,
                Some(Tok::Ne) => Expr::Ne as fn(_, _) -> _,
                Some(Tok::Lt) => Expr::Lt as fn(_, _) -> _,
                Some(Tok::Le) => Expr::Le as fn(_, _) -> _,
                Some(Tok::Gt) => Expr::Gt as fn(_, _) -> _,
                Some(Tok::Ge) => Expr::Ge as fn(_, _) -> _,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_add()?;
            lhs = op(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_mul()?;
        loop {
            match self.peek() {
                Some(Tok::Plus) => {
                    self.bump();
                    let rhs = self.parse_mul()?;
                    lhs = Expr::Add(Box::new(lhs), Box::new(rhs));
                }
                Some(Tok::Minus) => {
                    self.bump();
                    let rhs = self.parse_mul()?;
                    lhs = Expr::Sub(Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(Tok::Star) => {
                    self.bump();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::Mul(Box::new(lhs), Box::new(rhs));
                }
                Some(Tok::Slash) => {
                    self.bump();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::Div(Box::new(lhs), Box::new(rhs));
                }
                Some(Tok::Percent) => {
                    self.bump();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::Mod(Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ExprError> {
        match self.peek() {
            Some(Tok::Minus) => {
                self.bump();
                Ok(Expr::Neg(Box::new(self.parse_unary()?)))
            }
            Some(Tok::Bang) => {
                self.bump();
                Ok(Expr::Not(Box::new(self.parse_unary()?)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ExprError> {
        match self.bump() {
            Some(Tok::Int(v)) => Ok(Expr::Const(v)),
            Some(Tok::Pc) => Ok(Expr::Pc),
            Some(Tok::Depth) => Ok(Expr::Depth),
            Some(Tok::Top) => Ok(Expr::Top),
            Some(Tok::Globals) => {
                self.expect(Tok::LBracket, "`[` after `globals`")?;
                let idx = self.parse_or()?;
                self.expect(Tok::RBracket, "`]`")?;
                Ok(Expr::Global(Box::new(idx)))
            }
            Some(Tok::Memory) => {
                self.expect(Tok::LBracket, "`[` after `memory`")?;
                let idx = self.parse_or()?;
                self.expect(Tok::RBracket, "`]`")?;
                Ok(Expr::Memory(Box::new(idx)))
            }
            Some(Tok::LParen) => {
                let e = self.parse_or()?;
                self.expect(Tok::RParen, "`)`")?;
                Ok(e)
            }
            Some(got) => Err(err(format!("unexpected token `{got:?}`"))),
            None => Err(err("unexpected end of expression")),
        }
    }
}

impl Expr {
    /// Parse a complete expression. Trailing input is an error.
    ///
    /// # Errors
    ///
    /// Returns an [`ExprError`] describing the first problem: an unknown
    /// variable, an out-of-range integer literal, an unexpected or missing
    /// token, or trailing input after a complete expression.
    pub fn parse(src: &str) -> Result<Expr, ExprError> {
        let toks = tokenize(src)?;
        if toks.is_empty() {
            return Err(err("empty expression"));
        }
        let mut p = Parser { toks, pos: 0 };
        let e = p.parse_or()?;
        if let Some(got) = p.bump() {
            return Err(err(format!("unexpected trailing input `{got:?}`")));
        }
        Ok(e)
    }

    /// Evaluate the expression against a read-only context. Total: division or
    /// remainder by zero yields 0, globals out of range read 0, and a memory
    /// index is reduced into range with Euclidean remainder.
    #[must_use]
    pub fn eval(&self, ctx: &EvalCtx<'_>) -> i64 {
        match self {
            Expr::Const(v) => *v,
            Expr::Pc => i64::try_from(ctx.pc).unwrap_or(i64::MAX),
            Expr::Depth => i64::try_from(ctx.depth).unwrap_or(i64::MAX),
            Expr::Top => ctx.top,
            Expr::Global(e) => {
                let i = e.eval(ctx);
                usize::try_from(i)
                    .ok()
                    .and_then(|idx| ctx.globals.get(idx))
                    .copied()
                    .unwrap_or(0)
            }
            Expr::Memory(e) => {
                let i = e.eval(ctx);
                let idx = mem_index(i, ctx.memory.len());
                ctx.memory.get(idx).copied().unwrap_or(0)
            }
            Expr::Not(e) => i64::from(e.eval(ctx) == 0),
            Expr::Neg(e) => e.eval(ctx).wrapping_neg(),
            Expr::Add(a, b) => a.eval(ctx).wrapping_add(b.eval(ctx)),
            Expr::Sub(a, b) => a.eval(ctx).wrapping_sub(b.eval(ctx)),
            Expr::Mul(a, b) => a.eval(ctx).wrapping_mul(b.eval(ctx)),
            Expr::Div(a, b) => {
                let (x, y) = (a.eval(ctx), b.eval(ctx));
                if y == 0 {
                    0
                } else {
                    x.wrapping_div(y)
                }
            }
            Expr::Mod(a, b) => {
                let (x, y) = (a.eval(ctx), b.eval(ctx));
                if y == 0 {
                    0
                } else {
                    x.wrapping_rem(y)
                }
            }
            Expr::Lt(a, b) => bool_val(a.eval(ctx) < b.eval(ctx)),
            Expr::Le(a, b) => bool_val(a.eval(ctx) <= b.eval(ctx)),
            Expr::Gt(a, b) => bool_val(a.eval(ctx) > b.eval(ctx)),
            Expr::Ge(a, b) => bool_val(a.eval(ctx) >= b.eval(ctx)),
            Expr::Eq(a, b) => bool_val(a.eval(ctx) == b.eval(ctx)),
            Expr::Ne(a, b) => bool_val(a.eval(ctx) != b.eval(ctx)),
            Expr::And(a, b) => i64::from(a.eval(ctx) != 0 && b.eval(ctx) != 0),
            Expr::Or(a, b) => i64::from(a.eval(ctx) != 0 || b.eval(ctx) != 0),
        }
    }

    /// Evaluate as a condition: nonzero means true.
    #[must_use]
    pub fn eval_cond(&self, ctx: &EvalCtx<'_>) -> bool {
        self.eval(ctx) != 0
    }
}

/// Reduce a memory index into range the same way the VM's `loadmem` does.
fn mem_index(i: i64, len: usize) -> usize {
    // Memory is capped at `MAX_DATA_CELLS` (2^20 cells), so the length fits an
    // i64 with a vast margin and the Euclidean-reduced index, which lies in
    // 0..len, always fits a usize.
    #![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let n = len as i64;
    if n == 0 {
        0
    } else {
        i.rem_euclid(n) as usize
    }
}

fn bool_val(b: bool) -> i64 {
    i64::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(globals: &'a [i64], memory: &'a [i64]) -> EvalCtx<'a> {
        EvalCtx {
            pc: 7,
            depth: 2,
            top: -3,
            globals,
            memory,
        }
    }

    fn ev(src: &str, globals: &[i64], memory: &[i64]) -> i64 {
        Expr::parse(src).expect(src).eval(&ctx(globals, memory))
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("2 + 3 * 4", &[], &[]), 14);
        assert_eq!(ev("(2 + 3) * 4", &[], &[]), 20);
        assert_eq!(ev("10 - 2 - 3", &[], &[]), 5);
        assert_eq!(ev("-top", &[], &[]), 3);
        assert_eq!(ev("- - 5", &[], &[]), 5);
        assert_eq!(ev("7 % 3", &[], &[]), 1);
        assert_eq!(ev("10 / 0", &[], &[]), 0);
        assert_eq!(ev("10 % 0", &[], &[]), 0);
        assert_eq!(ev("0 - 9223372036854775807 - 1", &[], &[]), i64::MIN);
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(ev("1 < 2", &[], &[]), 1);
        assert_eq!(ev("2 <= 1", &[], &[]), 0);
        assert_eq!(ev("3 == 3", &[], &[]), 1);
        assert_eq!(ev("3 != 3", &[], &[]), 0);
        assert_eq!(ev("1 && 0", &[], &[]), 0);
        assert_eq!(ev("1 || 0", &[], &[]), 1);
        assert_eq!(ev("!0", &[], &[]), 1);
        assert_eq!(ev("!(1 && 0) || 0", &[], &[]), 1);
        assert_eq!(ev("1 < 2 == 1", &[], &[]), 1);
    }

    #[test]
    fn machine_variables_and_indexing() {
        let g = [10, 0, 30];
        let m = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(ev("pc", &g, &m), 7);
        assert_eq!(ev("depth", &g, &m), 2);
        assert_eq!(ev("top", &g, &m), -3);
        assert_eq!(ev("globals[0] + globals[2]", &g, &m), 40);
        assert_eq!(ev("globals[9]", &g, &m), 0, "out of range reads 0");
        assert_eq!(ev("globals[0 - 1]", &g, &m), 0, "negative global index reads 0");
        assert_eq!(ev("memory[3]", &g, &m), 4);
        assert_eq!(ev("memory[8]", &g, &m), 1, "memory wraps like loadmem");
        assert_eq!(ev("memory[-1]", &g, &m), 8, "negative memory wraps");
        assert_eq!(ev("memory[pc % 8]", &g, &m), 8);
        assert_eq!(ev("top + globals[depth - 2]", &g, &m), -3 + 10);
    }

    #[test]
    fn parse_errors() {
        let bad = [
            "",
            "1 +",
            "(1",
            "1)",
            "()",
            "1 2",
            "foo",
            "globals",
            "globals[x]",
            "globals[1",
            "pc & 1",
            "1 ? 2 : 3",
            "99999999999999999999",
            "pc pc",
            "&&",
        ];
        for src in bad {
            let res = Expr::parse(src);
            assert!(res.is_err(), "expected `{src}` to be rejected");
            assert!(!res.unwrap_err().message.is_empty());
        }
        // Accepted forms that must not error.
        for src in ["pc", "  pc  ", "globals[0]", "-pc + 1", "!pc", "((pc))"] {
            assert!(Expr::parse(src).is_ok(), "expected `{src}` to parse");
        }
    }
}

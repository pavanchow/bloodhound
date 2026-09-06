//! The Bloodhound command line debugger.
//!
//! Usage:
//!   bloodhound            Load the factorial sample into an interactive REPL.
//!   bloodhound <name>     Load a built-in sample by name (factorial, `sum_loop`, memory).
//!   bloodhound file <p>   Load an assembly file from path `p`.
//!   bloodhound list       List the built-in samples.
//!   bloodhound demo       Run a scripted demonstration and exit.
//!
//! REPL commands are listed by typing `help`.

use bloodhound::asm::assemble;
use bloodhound::debugger::{Debugger, StopReason, WatchLoc};
use bloodhound::expr::Expr;
use bloodhound::samples;
use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => repl_from_source("factorial", samples::FACTORIAL),
        Some("list") => {
            println!("built-in samples:");
            for (name, _) in samples::ALL {
                println!("  {name}");
            }
        }
        Some("demo") => run_demo(),
        Some("file") => match args.get(1) {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(src) => repl_from_source(path, &src),
                Err(e) => eprintln!("cannot read {path}: {e}"),
            },
            None => eprintln!("usage: bloodhound file <path>"),
        },
        Some(name) => match samples::by_name(name) {
            Some(src) => repl_from_source(name, src),
            None => eprintln!("unknown sample `{name}` (try: bloodhound list)"),
        },
    }
}

fn build(name: &str, src: &str) -> Option<Debugger> {
    match assemble(src) {
        Ok(p) => Some(Debugger::new(p)),
        Err(e) => {
            eprintln!("assembly error in {name}: {e}");
            None
        }
    }
}

fn repl_from_source(name: &str, src: &str) {
    let Some(mut d) = build(name, src) else {
        return;
    };
    println!("Bloodhound time-travel debugger. Loaded `{name}`.");
    println!("Type `help` for commands, `src` to see the program.\n");
    render(&d);

    let stdin = io::stdin();
    loop {
        print!("(bh) ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !dispatch(&mut d, line) {
            break;
        }
    }
}

/// Returns false to quit.
#[allow(clippy::too_many_lines)] // a flat match over the REPL command words is the table
fn dispatch(d: &mut Debugger, line: &str) -> bool {
    // `... if <expr>` attaches a condition to break / breaki / watch.
    let (head, cond_src) = match line.find(" if ") {
        Some(i) => (&line[..i], Some(line[i + 4..].trim())),
        None => (line, None),
    };
    let cond = match cond_src {
        Some(text) => match Expr::parse(text) {
            Ok(e) => Some(e),
            Err(e) => {
                println!("bad condition: {e}");
                return true;
            }
        },
        None => None,
    };
    let mut it = head.split_whitespace();
    let cmd = it.next().unwrap_or("");
    let arg = it.next();
    match cmd {
        "help" | "h" => print_help(),
        "quit" | "q" => return false,
        "src" | "list" => print_source(d),
        "reset" => {
            d.reset();
            render(d);
        }
        "c" | "continue" => {
            let r = d.cont();
            report(d, &r);
        }
        "rc" => {
            let r = d.run_back();
            report(d, &r);
        }
        "stepi" | "si" => {
            let r = d.step_instr();
            report(d, &r);
        }
        "step" | "s" => {
            let r = d.step_line();
            report(d, &r);
        }
        "next" | "n" => {
            let r = d.step_over();
            report(d, &r);
        }
        "out" | "finish" => {
            let r = d.step_out();
            report(d, &r);
        }
        "back" | "b" => {
            if d.backward() {
                render(d);
            } else {
                println!("already at the start (step 0)");
            }
        }
        "goto" | "g" => match arg.and_then(|s| s.parse::<usize>().ok()) {
            Some(n) => {
                d.goto(n);
                render(d);
            }
            None => println!("usage: goto <step>"),
        },
        "break" | "bp" => match arg.and_then(|s| s.parse::<u32>().ok()) {
            Some(l) => {
                let hit = match cond {
                    Some(e) => d.add_break_line_cond(l, e),
                    None => d.add_break_line(l),
                };
                match hit {
                    Some(a) => println!("breakpoint at line {l} (addr {a}){}", cond_suffix(cond_src)),
                    None => println!("no instruction on line {l}"),
                }
            }
            None => println!("usage: break <line> [if <expr>]"),
        },
        "breaki" => match arg.and_then(|s| s.parse::<usize>().ok()) {
            Some(a) => {
                match cond {
                    Some(e) => d.add_break_cond(a, e),
                    None => d.add_break(a),
                }
                println!("breakpoint at addr {a}{}", cond_suffix(cond_src));
            }
            None => println!("usage: breaki <addr> [if <expr>]"),
        },
        "delete" | "d" => match arg.and_then(|s| s.parse::<u32>().ok()) {
            Some(l) => match d.line_to_addr(l) {
                Some(a) if d.remove_break(a) => println!("removed breakpoint on line {l}"),
                _ => println!("no breakpoint on line {l}"),
            },
            None => println!("usage: delete <line>"),
        },
        "watch" | "w" => match arg.and_then(parse_watch) {
            Some(loc) => {
                match cond {
                    Some(e) => d.add_watch_cond(loc, e),
                    None => d.add_watch(loc),
                }
                println!("watching {}{}", watch_name(loc), cond_suffix(cond_src));
            }
            None => println!("usage: watch g<idx> | m<idx> | l<idx> [if <expr>]"),
        },
        "eval" => {
            let src = head.trim_start()["eval".len()..].trim();
            match Expr::parse(src) {
                Ok(e) => {
                    let v = e.eval(&d.eval_ctx());
                    println!("= {v}");
                }
                Err(e) => println!("bad expression: {e}"),
            }
        },
        "bt" | "where" => print_backtrace(d),
        "print" | "p" => print_state(d),
        other => println!("unknown command `{other}` (try `help`)"),
    }
    true
}

fn cond_suffix(cond_src: Option<&str>) -> String {
    match cond_src {
        Some(text) => format!(" if {text}"),
        None => String::new(),
    }
}

fn report(d: &Debugger, reason: &StopReason) {
    match &reason {
        StopReason::Breakpoint(a) => println!("stopped at breakpoint (addr {a}, line {})", d.current_line()),
        StopReason::Watchpoint(hit) => println!(
            "watchpoint {}: {} -> {} at step {}",
            watch_name(hit.loc),
            hit.old,
            hit.new,
            hit.step
        ),
        StopReason::Halted => println!("program halted"),
        StopReason::Limit => println!("step limit reached"),
        StopReason::Start => println!("reached the start of history"),
        StopReason::Stepped | StopReason::Line => {}
    }
    render(d);
}

fn render(d: &Debugger) {
    let pc = d.pc();
    let line = d.current_line();
    let op = d
        .vm()
        .current_op()
        .map_or_else(|| "<end>".to_string(), ToString::to_string);
    let status = if d.halted() { " [halted]" } else { "" };
    println!(
        "step {:<4} line {:<3} pc {:<3} {}{}",
        d.step_count(),
        line,
        pc,
        op,
        status
    );
    if line > 0 {
        if let Some(text) = d.program.source.get((line - 1) as usize) {
            println!("  >> {}", text.trim_end());
        }
    }
    let stack = d.vm().stack.clone();
    println!("  stack {stack:?}");
    println!("  locals {:?}", d.locals());
}

fn print_state(d: &Debugger) {
    render(d);
    println!("  globals {:?}", d.vm().globals);
    let mem: Vec<(usize, i64)> = d
        .vm()
        .memory
        .iter()
        .enumerate()
        .filter(|(_, &v)| v != 0)
        .map(|(i, &v)| (i, v))
        .collect();
    println!("  memory (nonzero) {mem:?}");
    println!("  output {:?}", d.vm().output);
    if !d.breakpoints().is_empty() {
        println!("  breakpoints (addr) {:?}", d.breakpoints());
    }
}

fn print_backtrace(d: &Debugger) {
    println!("call stack (innermost last):");
    for (i, f) in d.backtrace().iter().enumerate() {
        println!(
            "  #{i} {} return_pc={} call_line={} locals={:?}",
            f.func, f.return_pc, f.call_line, f.locals
        );
    }
}

fn print_source(d: &Debugger) {
    // Line numbers fit a u32: a source with 2^32 lines cannot be materialized.
    #![allow(clippy::cast_possible_truncation)]
    let cur = d.current_line();
    for (i, text) in d.program.source.iter().enumerate() {
        let lineno = (i + 1) as u32;
        let marker = if lineno == cur { "->" } else { "  " };
        let bp = if d
            .line_to_addr(lineno)
            .is_some_and(|a| d.breakpoints().contains(&a))
        {
            "*"
        } else {
            " "
        };
        println!("{marker}{bp}{lineno:>3} | {}", text.trim_end());
    }
}

fn parse_watch(s: &str) -> Option<WatchLoc> {
    let (kind, rest) = s.split_at(1);
    let idx: usize = rest.parse().ok()?;
    match kind {
        "g" => Some(WatchLoc::Global(idx)),
        "m" => Some(WatchLoc::Mem(idx)),
        "l" => Some(WatchLoc::Local(idx)),
        _ => None,
    }
}

fn watch_name(loc: WatchLoc) -> String {
    match loc {
        WatchLoc::Global(i) => format!("global[{i}]"),
        WatchLoc::Mem(i) => format!("mem[{i}]"),
        WatchLoc::Local(i) => format!("local[{i}]"),
    }
}

fn print_help() {
    println!(
        "commands:
  src                 show the program with the current line marked
  c                   continue to next breakpoint or watchpoint
  rc                  reverse-continue back to the previous breakpoint
  s / step            step to the next source line (into calls)
  n / next            step over (skip calls)
  out / finish        step out of the current function
  stepi / si          step a single instruction
  back / b            step backward one instruction (time travel)
  goto <step>         jump to an absolute step index (time travel)
  break <line> [if <expr>]   set a breakpoint, optionally conditional
  breaki <addr> [if <expr>]  set an address breakpoint, optionally conditional
  delete <line>       remove a breakpoint on a source line
  watch g<i>|m<i>|l<i> [if <expr>]  watch a global, cell, or local
  eval <expr>         evaluate an expression against the current state
  bt / where          show the call stack
  p / print           show full machine state
  reset               restart at step 0
  q / quit            exit

expressions use pc, depth, top (the stack top), globals[e], memory[e],
integers, + - * / %, == != < <= > >=, && || ! and parentheses"
    );
}

fn run_demo() {
    println!("=== Bloodhound scripted demo: recursive factorial(5) ===\n");
    let d = build("factorial", samples::FACTORIAL);
    let Some(mut d) = d else { return };

    print_source(&d);
    println!();

    // Breakpoint on the multiply line inside `recurse`.
    let mul_line = u32::try_from(
        d.program
            .source
            .iter()
            .position(|l| l.contains("n * fact"))
            .map_or(0, |i| i + 1),
    )
    .unwrap_or(0);
    let addr = d.add_break_line(mul_line).unwrap();
    println!("set breakpoint on line {mul_line} (addr {addr}), the `mul` in recurse\n");

    // Watch the global result slot.
    d.add_watch(WatchLoc::Global(0));
    println!("watching global[0] (the result)\n");

    println!("continue #1:");
    let r = d.cont();
    report(&d, &r);
    println!("  backtrace here:");
    print_backtrace(&d);
    let saved_step = d.step_count();
    let saved = d.snapshot();
    println!();

    println!("continue #2 (deeper recursion):");
    let r = d.cont();
    report(&d, &r);
    println!();

    println!("time travel: goto step {saved_step} (back to the first breakpoint hit):");
    d.goto(saved_step);
    render(&d);
    assert_eq!(d.snapshot(), saved, "reverse execution reconstructed the exact state");
    println!("  state matches the earlier snapshot exactly (reverse execution verified)\n");

    println!("single reverse step from here:");
    let before = d.snapshot();
    d.step_instr();
    let after_fwd = d.snapshot();
    d.backward();
    let back = d.snapshot();
    println!("  step forward changed state: {}", before != after_fwd);
    println!("  step back restored it:      {}", back == before);
    render(&d);
    println!();

    println!("run to the end, then reverse-continue to the breakpoint:");
    let r = d.cont();
    report(&d, &r);
    println!("  output at halt: {:?}", d.vm().output);
    let r = d.run_back();
    report(&d, &r);
    println!("  output after reverse-continue (the print un-happened): {:?}", d.vm().output);
    println!("=== demo complete ===");
}

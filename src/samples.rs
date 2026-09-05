//! Ready to load example programs, used by the CLI and the tests.

/// A recursive factorial. Exercises call frames, arguments, locals, returns,
/// branches, and a global result. `fact(5)` prints `120`.
pub const FACTORIAL: &str = "\
; recursive factorial of 5
.globals 4
.memory 16
main:
  push 5
  call fact 1 2
  storeg 0
  loadg 0
  print
  halt
fact:            ; local 0 = n
  load 0
  push 1
  le             ; n <= 1 ?
  jz recurse
  push 1
  ret
recurse:
  load 0         ; n
  load 0
  push 1
  sub            ; n - 1
  call fact 1 2
  mul            ; n * fact(n - 1)
  ret
";

/// A counting loop that accumulates a global sum. Good for watchpoints on
/// globals. Prints `15` (the sum of 1 through 5).
pub const SUM_LOOP: &str = "\
; sum of 1..5 into global 0
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
";

/// A short straight-line program that writes memory then reads it back.
pub const MEMORY_DEMO: &str = "\
; store and reload a value in linear memory
.globals 1
.memory 8
main:
  push 4         ; address
  push 77        ; value
  storemem       ; mem[4] = 77
  push 4
  loadmem
  print
  halt
";

/// The built-in samples as (name, source) pairs.
pub const ALL: &[(&str, &str)] = &[
    ("factorial", FACTORIAL),
    ("sum_loop", SUM_LOOP),
    ("memory", MEMORY_DEMO),
];

/// Look up a sample by name.
pub fn by_name(name: &str) -> Option<&'static str> {
    ALL.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::assemble;
    use crate::vm::Vm;

    fn run(src: &str) -> Vec<String> {
        let p = assemble(src).unwrap();
        let mut vm = Vm::new(&p);
        let mut n = 0;
        while vm.step().is_some() && n < 1_000_000 {
            n += 1;
        }
        vm.output
    }

    #[test]
    fn factorial_prints_120() {
        assert_eq!(run(FACTORIAL), vec!["120".to_string()]);
    }

    #[test]
    fn sum_loop_prints_15() {
        assert_eq!(run(SUM_LOOP), vec!["15".to_string()]);
    }

    #[test]
    fn memory_demo_prints_77() {
        assert_eq!(run(MEMORY_DEMO), vec!["77".to_string()]);
    }

    #[test]
    fn all_samples_assemble() {
        for (_, src) in ALL {
            assert!(assemble(src).is_ok());
        }
    }
}

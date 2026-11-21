// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `pcntl` extension
//! (and `posix` for `posix_kill()`).  Expected outputs come from
//! `/usr/bin/php`.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `code` (without `<?php`) and returns its output.
fn run(code: &str) -> String {
    let sink = Sink::default();
    let pcntl = php_ext_pcntl::PcntlExt;
    let posix = php_ext_posix::PosixExt;
    let exts: [&dyn Extension; 2] = [&pcntl, &posix];
    let opts = VmOptions { ini: vec![] };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

fn status_macros() {
    assert_eq!(
        run("var_dump(pcntl_wifexited(256), pcntl_wexitstatus(256), pcntl_wifsignaled(9), pcntl_wtermsig(9), pcntl_wifstopped(0x137f), pcntl_wstopsig(0x137f));"),
        "bool(true)\nint(1)\nbool(true)\nint(9)\nbool(true)\nint(19)\n"
    );
}

fn handlers_are_dispatched_with_siginfo() {
    assert_eq!(
        run(
            r#"pcntl_signal(SIGUSR1, function ($signo, $info) { echo "got $signo ", $info['signo'], "\n"; });
            posix_kill(posix_getpid(), SIGUSR1);
            echo "before\n";
            var_dump(pcntl_signal_dispatch());
            var_dump(pcntl_signal_get_handler(SIGUSR1) instanceof Closure, pcntl_signal_get_handler(SIGUSR2));
            pcntl_signal(SIGUSR1, SIG_DFL);"#
        ),
        "before\ngot 10 10\nbool(true)\nbool(true)\nint(0)\n"
    );
}

fn argument_errors() {
    assert_eq!(
        run(r#"foreach ([fn () => pcntl_signal(0, SIG_DFL), fn () => pcntl_signal(SIGUSR1, 5),
                fn () => pcntl_signal(SIGUSR1, 'nope'), fn () => pcntl_sigprocmask(9, [])] as $f) {
                try { $f(); } catch (Throwable $e) { echo $e::class, ': ', $e->getMessage(), "\n"; }
            }"#),
        "ValueError: pcntl_signal(): Argument #1 ($signal) must be greater than or equal to 1\n\
         ValueError: pcntl_signal(): Argument #2 ($handler) must be either SIG_DFL or SIG_IGN when an integer value is given\n\
         TypeError: pcntl_signal(): Argument #2 ($handler) must be of type callable|int, string given\n\
         ValueError: pcntl_sigprocmask(): Argument #1 ($mode) must be one of SIG_BLOCK, SIG_UNBLOCK, or SIG_SETMASK\n"
    );
}

fn async_signals_and_ticks() {
    assert_eq!(
        run(r#"pcntl_async_signals(true);
            pcntl_signal(SIGUSR1, function ($s) { echo "async $s\n"; });
            posix_kill(posix_getpid(), SIGUSR1);
            echo "after kill\n";
            pcntl_async_signals(false);
            pcntl_signal(SIGUSR1, function ($s) { echo "tick $s\n"; });
            declare(ticks = 1) {
                posix_kill(posix_getpid(), SIGUSR1);
                $x = 1;
                echo "after tick\n";
            }
            pcntl_signal(SIGUSR1, SIG_DFL);"#),
        "async 10\nafter kill\ntick 10\nafter tick\n"
    );
}

fn sigprocmask_and_sigtimedwait() {
    assert_eq!(
        run(r"pcntl_sigprocmask(SIG_BLOCK, [SIGUSR2], $old);
            posix_kill(posix_getpid(), SIGUSR2);
            var_dump(pcntl_sigtimedwait([SIGUSR2], $info, 1), $info['signo']);
            pcntl_sigprocmask(SIG_SETMASK, $old);
            var_dump(pcntl_sigtimedwait([SIGUSR2], $info, 0, 1000));"),
        "int(12)\nint(12)\nbool(false)\n"
    );
}

/// Signals are process-wide and a process-directed signal reaches any
/// thread not blocking it, so the scenarios run sequentially on the main
/// thread (this test target has `harness = false`), like the PHP CLI.
/// `pcntl_fork()` after the parent's timer thread started: the child's
/// own `set_time_limit(1)` must end its loop (php-src: `php_child_init()`
/// creates the child's timer). Before the fork guard, the child's arms
/// went to a set whose thread had stayed in the parent: the loop ran on.
/// Times in `posix_times()` ticks (`CLK_TCK` = 100 on Linux).
fn time_limit_in_a_forked_child() {
    let me = std::process::id();
    let out = run(r#"set_time_limit(5);
        $pid = pcntl_fork();
        if ($pid === 0) {
            set_time_limit(1);
            $t = posix_times()['ticks'];
            while (posix_times()['ticks'] - $t < 4 * 100) {}
            exit(0);
        }
        $t = posix_times()['ticks'];
        pcntl_waitpid($pid, $status);
        echo posix_times()['ticks'] - $t < 2.5 * 100 ? "stopped" : "ran on", "\n";"#);
    if std::process::id() != me {
        // The child: its script ended; the harness is the parent's.
        std::process::exit(0);
    }
    assert_eq!(out, "stopped\n");
}

fn main() {
    status_macros();
    handlers_are_dispatched_with_siginfo();
    argument_errors();
    sigprocmask_and_sigtimedwait();
    async_signals_and_ticks();
    time_limit_in_a_forked_child();
    println!("pcntl script scenarios: ok");
}

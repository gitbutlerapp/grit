//! `test-tool run-command` — mirrors `git/t/helper/test-run-command.c` (t0061).

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::thread;

const PRELOAD: &str = "preloaded output of a child\n";
const ASKING_STOP: &str = "asking for a quick stop\n";
const NO_JOBS: &str = "no further jobs available\n";

fn sq_quote_buf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    let mut rest = s;
    while !rest.is_empty() {
        let take = rest.find(['\'', '!']).unwrap_or(rest.len());
        out.push_str(&rest[..take]);
        rest = &rest[take..];
        if let Some(c) = rest.chars().next() {
            out.push_str("'\\");
            out.push(c);
            out.push('\'');
            rest = &rest[c.len_utf8()..];
        }
    }
    out.push('\'');
    out
}

fn sq_quote_buf_pretty(s: &str) -> String {
    const OK_PUNCT: &[u8] = b"+,-./:=@_^";
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || OK_PUNCT.contains(&(c as u8)))
    {
        return s.to_string();
    }
    sq_quote_buf(s)
}

fn trace_add_env(delta: &[String]) -> String {
    let mut map: BTreeMap<String, Option<String>> = BTreeMap::new();
    for entry in delta {
        if let Some((k, v)) = entry.split_once('=') {
            map.insert(k.to_string(), Some(v.to_string()));
        } else {
            map.insert(entry.clone(), None);
        }
    }
    let mut out = String::new();
    let mut printed_unset = false;
    for (var, val) in &map {
        if val.is_none() && std::env::var_os(var).is_some() {
            if !printed_unset {
                out.push_str(" unset");
                printed_unset = true;
            }
            out.push(' ');
            out.push_str(var);
        }
    }
    if printed_unset {
        out.push(';');
    }
    for (var, val) in &map {
        let Some(val) = val else {
            continue;
        };
        let old = std::env::var(var).ok();
        if old.as_deref() == Some(val.as_str()) {
            continue;
        }
        out.push(' ');
        out.push_str(var);
        out.push('=');
        out.push_str(&sq_quote_buf_pretty(val));
    }
    out
}

fn trace_start_command_line(argv: &[String]) {
    let Ok(trace_val) = std::env::var("GIT_TRACE") else {
        return;
    };
    if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
        return;
    }
    let mut line = String::from("trace: start_command:");
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(&sq_quote_buf_pretty(a));
    }
    line.push('\n');
    crate::write_git_trace(&trace_val, &line);
}

fn trace_run_command_line(env_delta: &[String], argv: &[String]) {
    let Ok(trace_val) = std::env::var("GIT_TRACE") else {
        return;
    };
    if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
        return;
    }
    let mut line = String::from("trace: run_command:");
    line.push_str(&trace_add_env(env_delta));
    for a in argv {
        line.push(' ');
        line.push_str(&sq_quote_buf_pretty(a));
    }
    line.push('\n');
    crate::write_git_trace(&trace_val, &line);
}

fn is_executable_file(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Ok(false);
    }
    Ok(meta.permissions().mode() & 0o100 != 0)
}

fn locate_in_path(file: &str) -> Option<PathBuf> {
    if file.contains('/') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    let path = path.to_string_lossy();
    for raw in path.split(':') {
        let base = if raw.is_empty() { "." } else { raw };
        let candidate = Path::new(base).join(file);
        if is_executable_file(&candidate).unwrap_or(false) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_program(argv0: &str) -> Result<PathBuf, std::io::Error> {
    if argv0.contains('/') {
        let p = Path::new(argv0);
        if !p.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not found",
            ));
        }
        return Ok(p.to_path_buf());
    }
    locate_in_path(argv0)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not in PATH"))
}

fn apply_env_delta(cmd: &mut Command, delta: &[String]) {
    let mut map: BTreeMap<String, Option<String>> = BTreeMap::new();
    for e in delta {
        if let Some((k, v)) = e.split_once('=') {
            map.insert(k.to_string(), Some(v.to_string()));
        } else {
            map.insert(e.clone(), None);
        }
    }
    for (k, v) in map {
        match v {
            Some(val) => {
                cmd.env(k, val);
            }
            None => {
                cmd.env_remove(&k);
            }
        }
    }
}

fn error_errno_cannot_exec(arg0: &str, err: &std::io::Error) {
    eprintln!("fatal: cannot exec '{arg0}': {err}");
}

fn error_errno_cannot_run(arg0: &str, err: &std::io::Error) {
    eprintln!("fatal: cannot run '{arg0}': {err}");
}

fn spawn_with_shell_fallback(
    program: &Path,
    argv: &[String],
    env_delta: &[String],
    use_stdin_pipe: bool,
) -> std::io::Result<std::process::Child> {
    let stdin = if use_stdin_pipe {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let stdout = Stdio::piped();
    let stderr = Stdio::piped();
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]);
    cmd.stdin(stdin);
    cmd.stdout(stdout);
    cmd.stderr(stderr);
    apply_env_delta(&mut cmd, env_delta);
    match cmd.spawn() {
        Ok(c) => Ok(c),
        Err(e) if e.raw_os_error() == Some(libc::ENOEXEC) => {
            let stdin2 = if use_stdin_pipe {
                Stdio::piped()
            } else {
                Stdio::null()
            };
            let mut sh = Command::new("/bin/sh");
            sh.arg(program);
            sh.args(&argv[1..]);
            sh.stdin(stdin2);
            sh.stdout(Stdio::piped());
            sh.stderr(Stdio::piped());
            apply_env_delta(&mut sh, env_delta);
            sh.spawn()
        }
        Err(e) => Err(e),
    }
}

fn feed_stdin_line(stdin: &mut ChildStdin, lines_left: &mut i32) -> std::io::Result<bool> {
    if *lines_left <= 0 {
        return Ok(true);
    }
    let line = format!("sample stdin {}\n", *lines_left - 1);
    *lines_left -= 1;
    match stdin.write_all(line.as_bytes()) {
        Ok(()) => Ok(*lines_left == 0),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e),
    }
}

fn run_processes_parallel_grouped(
    jobs: usize,
    argv: &[String],
    use_stdin: bool,
    no_jobs_mode: bool,
    task_finished_abort: bool,
) -> Result<()> {
    use std::sync::mpsc::{self, Receiver, Sender};

    if no_jobs_mode {
        std::io::stderr().write_all(NO_JOBS.as_bytes())?;
        return Ok(());
    }

    #[derive(Debug)]
    enum WorkerToMain {
        Stderr(usize, Vec<u8>),
        Finished(usize),
    }

    let (tx, rx): (Sender<WorkerToMain>, Receiver<WorkerToMain>) = mpsc::channel();
    let argv = Arc::new(argv.to_vec());

    let mut buffered_output: Vec<u8> = Vec::new();
    let mut output_owner: usize = 0;
    let mut tasks_spawned: usize = 0;
    let mut nr_running: usize = 0;
    let mut shutdown = false;
    let mut slot_busy: Vec<bool> = vec![false; jobs];
    let mut err_bufs: Vec<Vec<u8>> = vec![Vec::new(); jobs];
    let mut handles: Vec<Option<thread::JoinHandle<()>>> = (0..jobs).map(|_| None).collect();

    let find_free_slot = |busy: &[bool]| busy.iter().position(|b| !*b);

    let start_worker = |slot: usize,
                        argv: Arc<Vec<String>>,
                        use_stdin: bool,
                        tx: Sender<WorkerToMain>,
                        handles: &mut [Option<thread::JoinHandle<()>>]| {
        let txw = tx.clone();
        let av = Arc::clone(&argv);
        let h = thread::spawn(move || {
            let _ = txw.send(WorkerToMain::Stderr(slot, PRELOAD.as_bytes().to_vec()));
            let mut child =
                match spawn_with_shell_fallback(Path::new(&av[0]), av.as_ref(), &[], use_stdin) {
                    Ok(c) => c,
                    Err(_) => {
                        let _ = txw.send(WorkerToMain::Finished(slot));
                        return;
                    }
                };
            if use_stdin {
                if let Some(mut stdin) = child.stdin.take() {
                    let mut left = 2i32;
                    let _ = feed_stdin_line(&mut stdin, &mut left);
                    let _ = feed_stdin_line(&mut stdin, &mut left);
                }
            }
            let (Some(mut stderr), Some(mut stdout)) = (child.stderr.take(), child.stdout.take())
            else {
                let _ = txw.send(WorkerToMain::Finished(slot));
                return;
            };
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = txw.send(WorkerToMain::Stderr(slot, buf[..n].to_vec()));
                    }
                    Err(_) => break,
                }
            }
            // Git uses `stdout_to_stderr` for grouped parallel children; merge stdout into stderr.
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = txw.send(WorkerToMain::Stderr(slot, buf[..n].to_vec()));
                    }
                    Err(_) => break,
                }
            }
            let _ = child.wait();
            let _ = txw.send(WorkerToMain::Finished(slot));
        });
        handles[slot] = Some(h);
    };

    loop {
        while !shutdown && nr_running < jobs && tasks_spawned < 4 {
            let Some(si) = find_free_slot(&slot_busy) else {
                break;
            };
            tasks_spawned += 1;
            slot_busy[si] = true;
            err_bufs[si].clear();
            start_worker(si, Arc::clone(&argv), use_stdin, tx.clone(), &mut handles);
            nr_running += 1;
        }

        if nr_running == 0 && (tasks_spawned >= 4 || shutdown) {
            break;
        }

        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(WorkerToMain::Stderr(slot, chunk)) => {
                err_bufs[slot].extend_from_slice(&chunk);
                if slot == output_owner {
                    std::io::stderr().write_all(&err_bufs[slot])?;
                    err_bufs[slot].clear();
                }
            }
            Ok(WorkerToMain::Finished(slot)) => {
                if let Some(h) = handles[slot].take() {
                    let _ = h.join();
                }
                slot_busy[slot] = false;
                nr_running = nr_running.saturating_sub(1);

                if task_finished_abort {
                    buffered_output.extend_from_slice(ASKING_STOP.as_bytes());
                    shutdown = true;
                }

                if slot != output_owner {
                    buffered_output.append(&mut err_bufs[slot]);
                } else {
                    std::io::stderr().write_all(&err_bufs[slot])?;
                    err_bufs[slot].clear();
                    std::io::stderr().write_all(&buffered_output)?;
                    buffered_output.clear();
                    let mut next = output_owner;
                    for off in 0..jobs {
                        let j = (output_owner + off) % jobs;
                        if slot_busy[j] {
                            next = j;
                            break;
                        }
                    }
                    output_owner = next;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for h in handles.into_iter().flatten() {
        let _ = h.join();
    }

    std::io::stderr().write_all(&buffered_output)?;
    Ok(())
}

fn run_processes_parallel_ungrouped(
    jobs: usize,
    argv: &[String],
    use_stdin: bool,
    no_jobs_mode: bool,
    task_finished_abort: bool,
) -> Result<()> {
    if no_jobs_mode {
        eprint!("{NO_JOBS}");
        return Ok(());
    }

    let argv = Arc::new(argv.to_vec());
    let mut pending: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut tasks_spawned: usize = 0;
    let mut shutdown = false;

    while tasks_spawned < 4 || !pending.is_empty() {
        while pending.len() < jobs && tasks_spawned < 4 && !shutdown {
            tasks_spawned += 1;
            let av = Arc::clone(&argv);
            let use_stdin_f = use_stdin;
            pending.push(thread::spawn(move || {
                eprint!("{PRELOAD}");
                let Ok(mut child) =
                    spawn_with_shell_fallback(Path::new(&av[0]), av.as_ref(), &[], use_stdin_f)
                else {
                    return;
                };

                if use_stdin_f {
                    if let Some(mut stdin) = child.stdin.take() {
                        let mut left = 2i32;
                        let _ = feed_stdin_line(&mut stdin, &mut left);
                        let _ = feed_stdin_line(&mut stdin, &mut left);
                    }
                }

                let (Some(mut stderr), Some(mut stdout)) =
                    (child.stderr.take(), child.stdout.take())
                else {
                    let _ = child.wait();
                    return;
                };
                let out_h = thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match stdout.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _ = std::io::stdout().write_all(&buf[..n]);
                            }
                            Err(_) => break,
                        }
                    }
                });
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _ = std::io::stderr().write_all(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
                let _ = out_h.join();
                let _ = child.wait();
            }));
        }

        if pending.is_empty() {
            break;
        }

        let t = pending.remove(0);
        t.join().map_err(|_| anyhow::anyhow!("worker panicked"))?;
        if task_finished_abort {
            eprint!("{ASKING_STOP}");
            shutdown = true;
        }
    }

    Ok(())
}

/// Args after the `test-tool run-command` token (i.e. same slice Git's `cmd__run_command` sees as `argv[1..]`).
pub fn run(args: &[String]) -> Result<()> {
    let mut argv = args.to_vec();

    let mut env_delta: Vec<String> = Vec::new();
    while argv.len() >= 2 && argv[0] == "env" {
        env_delta.push(argv[1].clone());
        argv.drain(0..2);
    }

    let mut ungroup = false;
    if argv.first().map(|s| s.as_str()) == Some("--ungroup") {
        ungroup = true;
        argv.remove(0);
    }

    if argv.len() < 2 {
        bail!("check usage");
    }

    let mode = argv[0].clone();
    let tail = argv[1..].to_vec();

    match mode.as_str() {
        "start-command-ENOENT" => {
            if tail.is_empty() {
                bail!("check usage");
            }
            let target = &tail[0];
            match resolve_program(target) {
                Ok(program) => {
                    let mut cmd = Command::new(&program);
                    cmd.args(&tail[1..]);
                    apply_env_delta(&mut cmd, &env_delta);
                    match cmd.spawn() {
                        Ok(mut c) => {
                            let _ = c.wait();
                            std::process::exit(1);
                        }
                        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                            error_errno_cannot_exec(target, &e);
                            std::process::exit(0);
                        }
                        Err(e) => {
                            error_errno_cannot_run(target, &e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    error_errno_cannot_run(target, &e);
                    std::process::exit(0);
                }
                Err(e) => {
                    error_errno_cannot_run(target, &e);
                    std::process::exit(1);
                }
            }
        }
        "run-command" => {
            if tail.is_empty() {
                bail!("check usage");
            }
            let program = match resolve_program(&tail[0]) {
                Ok(p) => p,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    error_errno_cannot_run(&tail[0], &e);
                    std::process::exit(1);
                }
                Err(e) => {
                    error_errno_cannot_run(&tail[0], &e);
                    std::process::exit(1);
                }
            };
            let mut start_argv: Vec<String> = Vec::new();
            start_argv.push(program.to_string_lossy().into_owned());
            start_argv.extend_from_slice(&tail[1..]);
            trace_start_command_line(&start_argv);
            trace_run_command_line(&env_delta, &tail);
            let mut cmd = Command::new(&program);
            cmd.args(&tail[1..]);
            apply_env_delta(&mut cmd, &env_delta);
            match cmd.status() {
                Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                Err(e) if e.raw_os_error() == Some(libc::ENOEXEC) => {
                    let mut sh = Command::new("/bin/sh");
                    sh.arg(&program);
                    sh.args(&tail[1..]);
                    apply_env_delta(&mut sh, &env_delta);
                    match sh.status() {
                        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                        Err(e2) => {
                            error_errno_cannot_exec(&tail[0], &e2);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    error_errno_cannot_exec(&tail[0], &e);
                    std::process::exit(1);
                }
            }
        }
        "run-command-parallel"
        | "run-command-abort"
        | "run-command-no-jobs"
        | "run-command-stdin" => {
            if tail.len() < 2 {
                bail!("check usage");
            }
            let jobs: usize = tail[0]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid job count"))?;
            if jobs == 0 {
                bail!("invalid job count");
            }
            let cmd_argv = tail[1..].to_vec();
            let use_stdin = mode == "run-command-stdin";
            let no_jobs = mode == "run-command-no-jobs";
            let abort = mode == "run-command-abort";
            if ungroup {
                run_processes_parallel_ungrouped(jobs, &cmd_argv, use_stdin, no_jobs, abort)?;
            } else {
                run_processes_parallel_grouped(jobs, &cmd_argv, use_stdin, no_jobs, abort)?;
            }
        }
        other => bail!("check usage: unknown mode '{other}'"),
    }
    Ok(())
}

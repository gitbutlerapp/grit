//! `git instaweb` — start a local web server to browse the repository.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::repo::Repository;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::thread;
use std::time::{Duration, Instant};

use crate::commands::web_browse;
use crate::grit_exe::grit_executable;

const DEFAULT_PORT: u16 = 1234;

/// Parsed instaweb invocation (Git-compatible flags).
#[derive(Debug, Clone)]
struct Parsed {
    local: bool,
    port: Option<u16>,
    browser: Option<String>,
    httpd: Option<String>,
    action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Browse,
    Start,
    Stop,
    Restart,
}

/// Run `git instaweb` from raw argv (after the subcommand name).
pub fn run_from_argv(argv: &[String]) -> Result<()> {
    let parsed = parse_argv(argv)?;
    if let Some(httpd) = &parsed.httpd {
        if !httpd.is_empty() {
            bail!(
                "grit instaweb uses a built-in Rust web server; external httpd ({httpd}) is not supported"
            );
        }
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();

    let port = parsed
        .port
        .or_else(|| config_get_u16(&config, "instaweb.port"))
        .unwrap_or(DEFAULT_PORT);

    let bind_host = if parsed.local || config_get_bool(&config, "instaweb.local").unwrap_or(true) {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let bind = format!("{bind_host}:{port}");

    let state_dir = repo.git_dir.join("instaweb");
    let pid_file = state_dir.join("pid");
    let url = format!("http://127.0.0.1:{port}/");

    match parsed.action {
        Action::Stop => {
            stop_server(&pid_file)?;
            return Ok(());
        }
        Action::Start => {
            start_server(&repo, &bind, &state_dir, &pid_file)?;
            return Ok(());
        }
        Action::Restart => {
            let _ = stop_server(&pid_file);
            start_server(&repo, &bind, &state_dir, &pid_file)?;
            return Ok(());
        }
        Action::Browse => {
            if !server_running(&pid_file) {
                start_server(&repo, &bind, &state_dir, &pid_file)?;
            }
            if let Err(e) = open_browser(parsed.browser.as_deref(), &url, &config) {
                eprintln!("{e}");
                println!("{url}");
            }
        }
    }
    Ok(())
}

fn parse_argv(argv: &[String]) -> Result<Parsed> {
    let mut local = false;
    let mut port = None;
    let mut browser = None;
    let mut httpd = None;
    let mut action = Action::Browse;

    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            break;
        }
        if arg == "--local" || arg == "-l" {
            local = true;
            i += 1;
            continue;
        }
        if arg == "--start" || arg == "start" {
            action = Action::Start;
            i += 1;
            continue;
        }
        if arg == "--stop" || arg == "stop" {
            action = Action::Stop;
            i += 1;
            continue;
        }
        if arg == "--restart" || arg == "restart" {
            action = Action::Restart;
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--port=") {
            port = Some(parse_port(v)?);
            i += 1;
            continue;
        }
        if arg == "--port" || arg == "-p" {
            i += 1;
            port = Some(parse_port(argv.get(i).context("missing port value")?)?);
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--browser=") {
            browser = Some(v.to_string());
            i += 1;
            continue;
        }
        if arg == "--browser" || arg == "-b" {
            i += 1;
            browser = Some(argv.get(i).context("missing browser value")?.clone());
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--httpd=") {
            httpd = Some(v.to_string());
            i += 1;
            continue;
        }
        if arg == "--httpd" || arg == "-d" {
            i += 1;
            httpd = Some(argv.get(i).context("missing httpd value")?.clone());
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            bail!("unknown option: {arg}");
        }
        break;
    }
    Ok(Parsed {
        local,
        port,
        browser,
        httpd,
        action,
    })
}

fn parse_port(s: &str) -> Result<u16> {
    s.parse::<u16>()
        .with_context(|| format!("invalid port '{s}'"))
}

fn config_get_u16(config: &ConfigSet, key: &str) -> Option<u16> {
    config.get(key)?.parse().ok()
}

fn config_get_bool(config: &ConfigSet, key: &str) -> Option<bool> {
    match config.get(key)?.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn instaweb_server_exe() -> PathBuf {
    grit_executable()
        .parent()
        .map(|p| p.join("grit-instaweb"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("grit-instaweb"))
}

fn start_server(repo: &Repository, bind: &str, state_dir: &Path, pid_file: &Path) -> Result<()> {
    if server_running(pid_file) {
        eprintln!("Instance already running. Restarting...");
        let _ = stop_server(pid_file);
    }
    fs::create_dir_all(state_dir).with_context(|| format!("create {}", state_dir.display()))?;

    let exe = instaweb_server_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("--git-dir")
        .arg(&repo.git_dir)
        .arg("--bind")
        .arg(bind)
        .arg("--pid-file")
        .arg(pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(wt) = &repo.work_tree {
        cmd.arg("--work-tree").arg(wt);
    }

    cmd.spawn()
        .with_context(|| format!("failed to spawn {}", exe.display()))?;

    wait_until_ready(bind, pid_file)?;
    Ok(())
}

fn wait_until_ready(bind: &str, pid_file: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let host_port = bind
        .parse::<SocketAddr>()
        .or_else(|_| format!("127.0.0.1:{}", bind.rsplit(':').next().unwrap_or("1234")).parse())
        .context("parse bind address for readiness check")?;

    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&host_port, Duration::from_millis(200)).is_ok()
            && http_get_ok(host_port).unwrap_or(false)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = pid_file;
    bail!("instaweb server did not become ready at {bind}");
}

fn http_get_ok(addr: SocketAddr) -> Result<bool> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf)?;
    let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
    Ok(resp.starts_with("HTTP/1") && resp.contains(" 200 "))
}

fn server_running(pid_file: &Path) -> bool {
    read_pid(pid_file)
        .ok()
        .and_then(|pid| signal_process_alive(pid))
        .unwrap_or(false)
}

fn read_pid(path: &Path) -> Result<u32> {
    let s = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    s.trim()
        .parse()
        .with_context(|| format!("parse pid in {}", path.display()))
}

#[cfg(unix)]
fn signal_process_alive(pid: u32) -> Option<bool> {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => Some(true),
        Err(Errno::ESRCH) => Some(false),
        Err(Errno::EPERM) => Some(true),
        Err(_) => None,
    }
}

#[cfg(not(unix))]
fn signal_process_alive(pid: u32) -> Option<bool> {
    let _ = pid;
    None
}

fn stop_server(pid_file: &Path) -> Result<()> {
    if !pid_file.is_file() {
        return Ok(());
    }
    let pid = read_pid(pid_file).ok();
    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if signal_process_alive(pid) != Some(true) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = fs::remove_file(pid_file);
    Ok(())
}

fn open_browser(cli_browser: Option<&str>, url: &str, _config: &ConfigSet) -> Result<()> {
    web_browse::run(web_browse::Args {
        browser: cli_browser.map(str::to_owned),
        tool: None,
        config_var: Some("instaweb.browser".to_string()),
        url: vec![url.to_string()],
    })
}

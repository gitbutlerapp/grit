//! `test-tool rot13-filter` — Git filter protocol v2 test helper (see `git/t/helper/test-rot13-filter.c`).

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::Path;

const LARGE_PACKET_DATA_MAX: usize = 65520 - 4;

fn set_packet_header(len: usize, out: &mut [u8; 4]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[0] = HEX[(len >> 12) & 0xf];
    out[1] = HEX[(len >> 8) & 0xf];
    out[2] = HEX[(len >> 4) & 0xf];
    out[3] = HEX[len & 0xf];
}

fn write_packet<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > LARGE_PACKET_DATA_MAX {
        return Err(io::Error::other("packet too large"));
    }
    let total = payload.len() + 4;
    let mut hdr = [0u8; 4];
    set_packet_header(total, &mut hdr);
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

fn write_packet_line<W: Write>(w: &mut W, line: &str) -> io::Result<()> {
    let mut s = line.to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    write_packet(w, s.as_bytes())
}

fn write_flush<W: Write>(w: &mut W) -> io::Result<()> {
    w.write_all(b"0000")?;
    w.flush()
}

fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = r.read(&mut buf[off..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in pkt-line",
            ));
        }
        off += n;
    }
    Ok(())
}

/// Read 4-byte pkt header; returns `Ok(None)` on clean EOF before any byte (Git closes stdin to stop filter).
fn read_packet_header<R: Read>(r: &mut R) -> io::Result<Option<[u8; 4]>> {
    let mut hdr = [0u8; 4];
    let mut off = 0usize;
    while off < 4 {
        let n = r.read(&mut hdr[off..])?;
        if n == 0 {
            if off == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in pkt-line",
            ));
        }
        off += n;
    }
    Ok(Some(hdr))
}

fn read_packet_payload<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let Some(hdr) = read_packet_header(r)? else {
        return Ok(None);
    };
    let hex =
        std::str::from_utf8(&hdr).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let total = usize::from_str_radix(hex, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad pkt header"))?;
    if total == 0 {
        return Ok(None);
    }
    if total < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad pkt len"));
    }
    let len = total - 4;
    let mut payload = vec![0u8; len];
    read_exact(r, &mut payload)?;
    Ok(Some(payload))
}

fn read_packet_line<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let Some(p) = read_packet_payload(r)? else {
        return Ok(None);
    };
    Ok(Some(
        String::from_utf8_lossy(&p)
            .trim_end_matches('\n')
            .to_string(),
    ))
}

fn read_packetized<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = read_packet_payload(r)? {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn rot13_bytes(data: &mut [u8]) {
    for b in data.iter_mut() {
        *b = match *b {
            xa @ b'a'..=b'z' => b'a' + (xa - b'a' + 13) % 26,
            xa @ b'A'..=b'Z' => b'A' + (xa - b'A' + 13) % 26,
            other => other,
        };
    }
}

struct DelayEntry {
    count: i32,
    requested: u8,
    output: Option<Vec<u8>>,
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let mut always_delay = false;
    let mut log_path: Option<String> = None;
    let mut caps: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if a == "--always-delay" {
            always_delay = true;
            i += 1;
        } else if let Some(p) = a.strip_prefix("--log=") {
            log_path = Some(p.to_string());
            i += 1;
        } else if a == "--log" {
            i += 1;
            let Some(p) = args.get(i) else {
                anyhow::bail!("test-tool rot13-filter: --log needs a path");
            };
            log_path = Some(p.clone());
            i += 1;
        } else if a.starts_with('-') {
            anyhow::bail!("test-tool rot13-filter: unknown option '{a}'");
        } else {
            caps.push(a.clone());
            i += 1;
        }
    }

    let log_path = log_path.ok_or_else(|| {
        anyhow::anyhow!(
            "usage: test-tool rot13-filter [--always-delay] --log=<path> <capabilities>..."
        )
    })?;
    if caps.is_empty() {
        anyhow::bail!(
            "usage: test-tool rot13-filter [--always-delay] --log=<path> <capabilities>..."
        );
    }

    let mut has_clean = false;
    let mut has_smudge = false;
    for c in &caps {
        match c.as_str() {
            "clean" => has_clean = true,
            "smudge" => has_smudge = true,
            _ => {}
        }
    }

    let log_parent = Path::new(&log_path).parent();
    if let Some(p) = log_parent {
        if !p.as_os_str().is_empty() {
            std::fs::create_dir_all(p)?;
        }
    }
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let mut delay: HashMap<String, DelayEntry> = HashMap::new();
    let mut add = |path: &str, count: i32| {
        delay.insert(
            path.to_string(),
            DelayEntry {
                count,
                requested: 0,
                output: None,
            },
        );
    };
    add("test-delay10.a", 1);
    add("test-delay11.a", 1);
    add("test-delay20.a", 2);
    add("test-delay10.b", 1);
    add("missing-delay.a", 1);
    add("invalid-delay.a", 1);

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    writeln!(log, "START")?;

    // Client init
    let Some(l) = read_packet_line(&mut stdin)? else {
        anyhow::bail!("expected git-filter-client");
    };
    if l != "git-filter-client" {
        anyhow::bail!("bad initialize: '{l}'");
    }
    let Some(l) = read_packet_line(&mut stdin)? else {
        anyhow::bail!("expected version");
    };
    if l != "version=2" {
        anyhow::bail!("bad version: '{l}'");
    }
    if read_packet_line(&mut stdin)?.is_some() {
        anyhow::bail!("expected flush after version");
    }

    write_packet_line(&mut stdout, "git-filter-server")?;
    write_packet_line(&mut stdout, "version=2")?;
    write_flush(&mut stdout)?;
    stdout.flush()?;

    let mut remote: HashSet<String> = HashSet::new();
    while let Some(line) = read_packet_line(&mut stdin)? {
        if let Some(v) = line.strip_prefix("capability=") {
            remote.insert(v.to_string());
        }
    }
    for need in ["clean", "smudge", "delay"] {
        if !remote.contains(need) {
            anyhow::bail!("required '{need}' capability not available from remote");
        }
    }
    for c in &caps {
        if !remote.contains(c.as_str()) {
            anyhow::bail!("our capability '{c}' is not available from remote");
        }
        write_packet_line(&mut stdout, &format!("capability={c}"))?;
    }
    write_flush(&mut stdout)?;
    stdout.flush()?;

    writeln!(log, "init handshake complete")?;

    loop {
        let Some(cmd_line) = read_packet_line(&mut stdin)? else {
            writeln!(log, "STOP")?;
            break;
        };
        let command = cmd_line
            .strip_prefix("command=")
            .ok_or_else(|| anyhow::anyhow!("expected command="))?
            .to_string();
        write!(log, "IN: {command}")?;

        if command == "list_available_blobs" {
            if read_packet_line(&mut stdin)?.is_some() {
                anyhow::bail!("bad list_available_blobs end");
            }
            let mut path_items: Vec<String> = Vec::new();
            for (path, entry) in delay.iter_mut() {
                // Match C `test-rot13-filter.c`: skip only when `requested` is unset (0).
                // After `status=delayed`, `requested` is 2 but the path must still appear in
                // `list_available_blobs` once `count` reaches 0.
                if entry.requested == 0 {
                    continue;
                }
                entry.count -= 1;
                if path == "invalid-delay.a" {
                    write_packet_line(&mut stdout, "pathname=unfiltered")?;
                } else if path == "missing-delay.a" {
                    // omit
                } else if entry.count == 0 {
                    path_items.push(path.clone());
                    write_packet_line(&mut stdout, &format!("pathname={path}"))?;
                }
            }
            path_items.sort();
            for p in &path_items {
                write!(log, " {p}")?;
            }
            write_flush(&mut stdout)?;
            stdout.flush()?;
            writeln!(log, " [OK]")?;
            write_packet_line(&mut stdout, "status=success")?;
            write_flush(&mut stdout)?;
            stdout.flush()?;
            continue;
        }

        let Some(pn_line) = read_packet_line(&mut stdin)? else {
            anyhow::bail!("unexpected EOF while expecting pathname");
        };
        let pathname = pn_line
            .strip_prefix("pathname=")
            .ok_or_else(|| anyhow::anyhow!("expected pathname="))?
            .to_string();
        write!(log, " {pathname}")?;

        while let Some(buf) = read_packet_line(&mut stdin)? {
            if buf == "can-delay=1" {
                if always_delay {
                    // Match C `test-rot13-filter.c`: `always_delay` calls `add_delay_entry` for any
                    // path that receives `can-delay=1`, so `list_available_blobs` can see it.
                    delay
                        .entry(pathname.clone())
                        .and_modify(|e| {
                            if e.requested == 0 {
                                e.requested = 1;
                            }
                        })
                        .or_insert(DelayEntry {
                            count: 1,
                            requested: 1,
                            output: None,
                        });
                } else if let Some(entry) = delay.get_mut(&pathname) {
                    if entry.requested == 0 {
                        entry.requested = 1;
                    }
                }
            } else if buf.starts_with("ref=")
                || buf.starts_with("treeish=")
                || buf.starts_with("blob=")
            {
                write!(log, " {buf}")?;
            } else {
                anyhow::bail!("Unknown message '{buf}'");
            }
        }

        let mut input = read_packetized(&mut stdin)?;
        write!(log, " {} [OK] -- ", input.len())?;

        let output: Vec<u8> = if let Some(entry) = delay.get_mut(&pathname) {
            if let Some(ref o) = entry.output {
                o.clone()
            } else if pathname == "error.r" || pathname == "abort.r" {
                Vec::new()
            } else if (command == "clean" && has_clean) || (command == "smudge" && has_smudge) {
                rot13_bytes(&mut input);
                input
            } else {
                anyhow::bail!("bad command '{command}'");
            }
        } else if pathname == "error.r" || pathname == "abort.r" {
            Vec::new()
        } else if (command == "clean" && has_clean) || (command == "smudge" && has_smudge) {
            rot13_bytes(&mut input);
            input
        } else {
            anyhow::bail!("bad command '{command}'");
        };

        if pathname == "error.r" {
            writeln!(log, "[ERROR]")?;
            write_packet_line(&mut stdout, "status=error")?;
            write_flush(&mut stdout)?;
            stdout.flush()?;
            continue;
        }
        if pathname == "abort.r" {
            writeln!(log, "[ABORT]")?;
            write_packet_line(&mut stdout, "status=abort")?;
            write_flush(&mut stdout)?;
            stdout.flush()?;
            continue;
        }

        if command == "smudge" {
            if let Some(entry) = delay.get_mut(&pathname) {
                if entry.requested == 1 {
                    writeln!(log, "[DELAYED]")?;
                    write_packet_line(&mut stdout, "status=delayed")?;
                    write_flush(&mut stdout)?;
                    stdout.flush()?;
                    entry.requested = 2;
                    entry.output = Some(output.clone());
                    continue;
                }
            }
        }

        write_packet_line(&mut stdout, "status=success")?;
        write_flush(&mut stdout)?;
        stdout.flush()?;

        let fail_suffix = format!("{command}-write-fail.r");
        if pathname == fail_suffix {
            writeln!(log, "[WRITE FAIL]")?;
            anyhow::bail!("{command} write error");
        }

        write!(log, "OUT: {} ", output.len())?;
        let mut nr_packets = 0usize;
        let mut off = 0usize;
        while off < output.len() {
            let end = (off + LARGE_PACKET_DATA_MAX).min(output.len());
            write_packet(&mut stdout, &output[off..end])?;
            nr_packets += 1;
            off = end;
        }
        write_flush(&mut stdout)?;
        stdout.flush()?;
        for _ in 0..nr_packets {
            write!(log, ".")?;
        }
        writeln!(log, " [OK]")?;
        // Git's C helper sends an extra flush after the data segment (see `test-rot13-filter.c`).
        write_flush(&mut stdout)?;
        stdout.flush()?;
    }

    Ok(())
}

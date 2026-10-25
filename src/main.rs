// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! mosh-win: a native Windows client for mosh (the mobile shell).
//!
//! Usage: mosh-win [options] [user@]host [-- command...]
//!
//! Starts `mosh-server` over ssh (Windows OpenSSH by default), then connects
//! to it over UDP. Wire-compatible with mosh 1.3/1.4 servers.

use std::io::{BufRead, BufReader};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use mosh_win::client::{self, ClientConfig, Headless};
use mosh_win::crypto;
use mosh_win::overlay::DisplayPreference;

const USAGE: &str = "\
Usage: mosh-win [options] [user@]host [--] [command...]
       mosh-win --client IP PORT        (key in the MOSH_KEY environment variable)

Options:
  -a, --predict=always       always use local echo
  -n, --predict=never        never use local echo
      --predict=adaptive     local echo for slower links [default]
      --predict=experimental aggressively echo even when incorrect
  -o, --predict-overwrite    prediction overwrites instead of inserting
  -p, --port=PORT[:PORT2]    server-side UDP port or range
      --ssh=COMMAND          ssh command to run, e.g. \"ssh -F C:\\path\\config\"
                             [default: $MOSH_WIN_SSH, or ssh]
      --server=COMMAND       mosh server on remote machine [default: mosh-server]
      --locale=NAME=VALUE    fallback locale for the server [default: LANG=C.UTF-8]
      --remote-ip=local|remote
                             how to find the server's IP: resolve the ssh host name
                             locally [default], or use the address the server saw
  -h, --help                 this message

Escape sequence: Ctrl-^ .  quits.  Ctrl-^ ^  sends a literal Ctrl-^.";

struct Args {
    predict: DisplayPreference,
    predict_overwrite: bool,
    port: Option<String>,
    ssh: String,
    server: String,
    locale: String,
    remote_ip_remote: bool,
    client: Option<(String, String)>,
    host: Option<String>,
    command: Vec<String>,
    headless_script: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        predict: DisplayPreference::Adaptive,
        predict_overwrite: false,
        port: None,
        ssh: std::env::var("MOSH_WIN_SSH").unwrap_or_else(|_| "ssh".into()),
        server: "mosh-server".into(),
        locale: "LANG=C.UTF-8".into(),
        remote_ip_remote: false,
        client: None,
        host: None,
        command: Vec::new(),
        headless_script: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if a.host.is_some() {
            if arg != "--" || !a.command.is_empty() {
                a.command.push(arg);
            }
            continue;
        }
        let (name, inline_val) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let value = |it: &mut dyn Iterator<Item = String>| -> Result<String> {
            inline_val.clone().or_else(|| it.next()).ok_or_else(|| anyhow!("{name} needs a value"))
        };
        match name.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-a" => a.predict = DisplayPreference::Always,
            "-n" => a.predict = DisplayPreference::Never,
            "--predict" => {
                a.predict = match value(&mut it)?.as_str() {
                    "adaptive" => DisplayPreference::Adaptive,
                    "always" => DisplayPreference::Always,
                    "never" => DisplayPreference::Never,
                    "experimental" => DisplayPreference::Experimental,
                    other => bail!("unknown prediction mode {other}"),
                }
            }
            "-o" | "--predict-overwrite" => a.predict_overwrite = true,
            "-p" | "--port" => a.port = Some(value(&mut it)?),
            "--ssh" => a.ssh = value(&mut it)?,
            "--server" => a.server = value(&mut it)?,
            "--locale" => a.locale = value(&mut it)?,
            "--remote-ip" => {
                a.remote_ip_remote = match value(&mut it)?.as_str() {
                    "local" => false,
                    "remote" => true,
                    other => bail!("unknown --remote-ip mode {other}"),
                }
            }
            "--headless-script" => a.headless_script = Some(value(&mut it)?),
            "--client" => {
                let ip = it.next().ok_or_else(|| anyhow!("--client needs IP and PORT"))?;
                let port = it.next().ok_or_else(|| anyhow!("--client needs IP and PORT"))?;
                a.client = Some((ip, port));
            }
            s if s.starts_with('-') && s != "-" => bail!("unknown option {s}\n\n{USAGE}"),
            _ => a.host = Some(arg),
        }
    }
    if a.host.is_none() && a.client.is_none() {
        bail!("{USAGE}");
    }
    Ok(a)
}

/// POSIX shell quoting for the remote command line.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Split the --ssh value into program and arguments; double quotes group words.
fn split_command(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_word = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_word = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            c => {
                cur.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(cur);
    }
    words
}

fn ssh_command(ssh: &str) -> Result<Command> {
    let words = split_command(ssh);
    let (prog, rest) = words.split_first().ok_or_else(|| anyhow!("empty --ssh command"))?;
    let mut cmd = Command::new(prog);
    cmd.args(rest);
    Ok(cmd)
}

/// Ask ssh what host name it would connect to (honours ssh config).
fn ssh_hostname(ssh: &str, host: &str) -> Option<String> {
    let out = ssh_command(ssh).ok()?.args(["-G", host]).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("hostname ").map(|h| h.trim().to_string()))
}

fn resolve(host: &str) -> Option<IpAddr> {
    (host, 0).to_socket_addrs().ok()?.map(|a| a.ip()).next()
}

struct ServerInfo {
    port: u16,
    key: String,
    ssh_connection_ip: Option<IpAddr>,
}

fn start_server(args: &Args, host: &str) -> Result<ServerInfo> {
    let mut server_cmd = vec![args.server.clone(), "new".into(), "-c".into(), "256".into(), "-s".into()];
    server_cmd.extend(["-l".into(), args.locale.clone()]);
    if let Some(p) = &args.port {
        server_cmd.extend(["-p".into(), p.clone()]);
    }
    if !args.command.is_empty() {
        server_cmd.push("--".into());
        server_cmd.extend(args.command.iter().cloned());
    }
    let quoted: Vec<String> = server_cmd.iter().map(|s| shell_quote(s)).collect();
    let remote = format!("echo \"MOSH SSH_CONNECTION $SSH_CONNECTION\"; exec {}", quoted.join(" "));

    let mut child = ssh_command(&args.ssh)?
        .args(["-n", "-tt", host, "--", &remote])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {}", args.ssh))?;

    let stdout = child.stdout.take().unwrap();
    let mut info = ServerInfo { port: 0, key: String::new(), ssh_connection_ip: None };
    // other output (server banner, "Connection closed") is only shown if startup fails
    let mut other_output = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap_or_default();
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("MOSH SSH_CONNECTION ") {
            info.ssh_connection_ip = rest.split_whitespace().nth(2).and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MOSH CONNECT ") {
            let mut parts = rest.split_whitespace();
            info.port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            info.key = parts.next().unwrap_or("").to_string();
        } else if !line.is_empty() {
            other_output.push(line.to_string());
        }
    }
    let status = child.wait()?;
    if info.port == 0 || info.key.is_empty() {
        for line in other_output {
            eprintln!("{line}");
        }
        bail!("Did not find mosh server startup message. (Have you installed mosh on your server?) [ssh exited with {status}]");
    }
    Ok(info)
}

fn load_headless_script(path: &str) -> Result<Headless> {
    // Each line: "<delay_ms> <text>" with \r \n \e \xNN escapes; "<delay_ms> DUMP" prints the screen.
    let text = std::fs::read_to_string(path)?;
    let mut script = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (delay, rest) = line.split_once(' ').unwrap_or((line, ""));
        let delay: u64 = delay.parse()?;
        if rest == "DUMP" {
            script.push((delay, Vec::new()));
            continue;
        }
        let mut bytes = Vec::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                let mut b = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                continue;
            }
            match chars.next() {
                Some('r') => bytes.push(b'\r'),
                Some('n') => bytes.push(b'\n'),
                Some('e') => bytes.push(0x1b),
                Some('x') => {
                    let h: String = chars.by_ref().take(2).collect();
                    bytes.push(u8::from_str_radix(&h, 16)?);
                }
                Some(o) => bytes.push(o as u8),
                None => {}
            }
        }
        script.push((delay, bytes));
    }
    Ok(Headless { size: (80, 24), script })
}

fn run() -> Result<bool> {
    let args = parse_args()?;
    // check before starting a server we could not use
    if args.headless_script.is_none() && !mosh_win::console::is_console() {
        bail!("stdin/stdout must be a Windows console (run from Windows Terminal, cmd or PowerShell)");
    }

    let (addr, key) = if let Some((ip, port)) = &args.client {
        let key = std::env::var("MOSH_KEY").map_err(|_| anyhow!("MOSH_KEY environment variable not found."))?;
        let ip: IpAddr = ip.parse().context("bad IP address")?;
        (SocketAddr::new(ip, port.parse().context("bad port")?), key)
    } else {
        let host = args.host.as_deref().unwrap();
        let local_ip = if args.remote_ip_remote {
            None
        } else {
            let name = ssh_hostname(&args.ssh, host)
                .unwrap_or_else(|| host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host).to_string());
            resolve(&name)
        };
        let info = start_server(&args, host)?;
        let ip = local_ip
            .or(info.ssh_connection_ip)
            .ok_or_else(|| anyhow!("could not determine the server's IP address"))?;
        (SocketAddr::new(ip, info.port), info.key)
    };

    let key = crypto::parse_key(&key)?;
    let headless = match &args.headless_script {
        Some(p) => Some(load_headless_script(p)?),
        None => None,
    };
    client::run(ClientConfig {
        addr,
        key,
        predict: args.predict,
        predict_overwrite: args.predict_overwrite,
        headless,
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("mosh-win: {e:#}");
            ExitCode::from(255)
        }
    }
}

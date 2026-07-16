use anyhow::{Context, Result, anyhow};
use std::io::{self, Write};

use crate::{
    backend::ssh,
    session::config::{ConfigStore, Session},
};

const USAGE: &str = r#"ashell client mode

Usage:
  ashell client list
  ashell client exec --session <name|id|host> <command...>
  ashell client logs --session <name|id|host> [--lines <n>] <path>
  ashell client --session <name|id|host> -- <command...>

Examples:
  ashell client list
  ashell client exec --session prod uptime
  ashell client exec --session prod "systemctl status nginx --no-pager"
  ashell client logs --session prod --lines 200 /var/log/nginx/error.log
"#;

#[derive(Debug, Clone)]
enum ClientCommand {
    List,
    Exec {
        session: String,
        command: String,
    },
    Logs {
        session: String,
        path: String,
        lines: u32,
    },
    Help,
}

pub fn is_client_mode() -> bool {
    std::env::args().nth(1).as_deref() == Some("client")
}

pub fn attach_parent_console() {
    #[cfg(windows)]
    unsafe {
        let _ = windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

pub fn run_blocking() -> i32 {
    run_blocking_from(std::env::args().skip(2))
}

pub fn run_blocking_from(args: impl IntoIterator<Item = String>) -> i32 {
    match run_blocking_inner(args.into_iter().collect()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err:#}");
            1
        }
    }
}

fn run_blocking_inner(args: Vec<String>) -> Result<i32> {
    let command = parse_args(args)?;
    match command {
        ClientCommand::Help => {
            print!("{USAGE}");
            Ok(0)
        }
        ClientCommand::List => {
            let config = ConfigStore::load().context("load ashell config")?;
            print_sessions(config.sessions());
            Ok(0)
        }
        ClientCommand::Exec { session, command } => run_exec(&session, &command),
        ClientCommand::Logs {
            session,
            path,
            lines,
        } => {
            let command = format!("tail -n {lines} -- {}", shell_quote(&path));
            run_exec(&session, &command)
        }
    }
}

fn run_exec(selector: &str, command: &str) -> Result<i32> {
    let config = ConfigStore::load().context("load ashell config")?;
    let session = resolve_session(config.sessions(), selector)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start async runtime")?;
    let output = runtime.block_on(ssh::exec_ssh_command_streaming(
        session,
        command,
        |data| {
            let mut stdout = io::stdout().lock();
            stdout.write_all(data)?;
            stdout.flush()
        },
        |data| {
            let mut stderr = io::stderr().lock();
            stderr.write_all(data)?;
            stderr.flush()
        },
    ))?;

    Ok(output.exit_status.unwrap_or(0).min(i32::MAX as u32) as i32)
}

fn parse_args(args: Vec<String>) -> Result<ClientCommand> {
    if args.is_empty() {
        return Ok(ClientCommand::Help);
    }

    match args[0].as_str() {
        "-h" | "--help" | "help" => Ok(ClientCommand::Help),
        "list" | "sessions" => Ok(ClientCommand::List),
        "exec" | "run" => parse_exec_args(&args[1..]),
        "logs" | "log" | "tail" => parse_logs_args(&args[1..]),
        "--session" | "-s" => parse_shorthand_exec(&args),
        unknown => Err(anyhow!("unknown client command '{unknown}'\n\n{USAGE}")),
    }
}

fn parse_exec_args(args: &[String]) -> Result<ClientCommand> {
    let (session, rest) = take_session(args)?;
    let command = command_from_parts(&rest)?;
    Ok(ClientCommand::Exec { session, command })
}

fn parse_logs_args(args: &[String]) -> Result<ClientCommand> {
    let (session, mut rest) = take_session(args)?;
    let mut lines = 100;
    let mut path = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--lines" | "-n" => {
                let value = rest
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("missing value for {}", rest[i]))?;
                lines = value
                    .parse::<u32>()
                    .with_context(|| format!("invalid line count '{value}'"))?;
                rest.drain(i..=i + 1);
            }
            value => {
                path = Some(value.to_string());
                i += 1;
            }
        }
    }

    let path = path.ok_or_else(|| anyhow!("missing log path\n\n{USAGE}"))?;
    Ok(ClientCommand::Logs {
        session,
        path,
        lines,
    })
}

fn parse_shorthand_exec(args: &[String]) -> Result<ClientCommand> {
    let (session, rest) = take_session(args)?;
    let command = command_from_parts(&rest)?;
    Ok(ClientCommand::Exec { session, command })
}

fn take_session(args: &[String]) -> Result<(String, Vec<String>)> {
    let mut session = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" | "-s" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("missing value for {}", args[i]))?;
                session = Some(value.to_string());
                i += 2;
            }
            "--" => {
                rest.extend(args[i + 1..].iter().cloned());
                break;
            }
            value => {
                rest.push(value.to_string());
                i += 1;
            }
        }
    }

    let session = session.ok_or_else(|| anyhow!("missing --session <name|id|host>\n\n{USAGE}"))?;
    Ok((session, rest))
}

fn command_from_parts(parts: &[String]) -> Result<String> {
    let command = parts.join(" ").trim().to_string();
    if command.is_empty() {
        Err(anyhow!("missing remote command\n\n{USAGE}"))
    } else {
        Ok(command)
    }
}

fn resolve_session<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session> {
    let selector_lower = selector.to_ascii_lowercase();
    let matches = sessions
        .iter()
        .filter(|session| {
            session.id == selector
                || session.name.eq_ignore_ascii_case(selector)
                || session.host.eq_ignore_ascii_case(selector)
                || format!("{}@{}", session.user, session.host).eq_ignore_ascii_case(selector)
                || session.name.to_ascii_lowercase().contains(&selector_lower)
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [session] => Ok(session),
        [] => Err(anyhow!(
            "no saved session matches '{selector}'. Run `ashell client list` to see saved sessions"
        )),
        _ => Err(anyhow!(
            "multiple sessions match '{selector}'. Use the exact id or name:\n{}",
            matches
                .iter()
                .map(|session| format!(
                    "  {}  {}@{}  {}",
                    session.id, session.user, session.host, session.name
                ))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

fn print_sessions(sessions: &[Session]) {
    if sessions.is_empty() {
        println!("No saved SSH sessions.");
        return;
    }

    println!("{:<36}  {:<24}  {:<7}  {}", "ID", "NAME", "PORT", "TARGET");
    for session in sessions {
        println!(
            "{:<36}  {:<24}  {:<7}  {}@{}",
            session.id, session.name, session.port, session.user, session.host
        );
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '+'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exec() {
        let command = parse_args(vec![
            "exec".into(),
            "--session".into(),
            "prod".into(),
            "uptime".into(),
        ])
        .unwrap();

        match command {
            ClientCommand::Exec { session, command } => {
                assert_eq!(session, "prod");
                assert_eq!(command, "uptime");
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn parses_logs() {
        let command = parse_args(vec![
            "logs".into(),
            "-s".into(),
            "prod".into(),
            "-n".into(),
            "50".into(),
            "/var/log/app.log".into(),
        ])
        .unwrap();

        match command {
            ClientCommand::Logs {
                session,
                path,
                lines,
            } => {
                assert_eq!(session, "prod");
                assert_eq!(path, "/var/log/app.log");
                assert_eq!(lines, 50);
            }
            _ => panic!("wrong command"),
        }
    }
}

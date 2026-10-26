use std::io::{self, BufRead};

use crate::simulation::SimulationHandle;

const NON_ASCII_LITERAL_ERROR: &str =
    "literal message text must be ASCII; use \\xHH for other bytes";

#[derive(Debug, PartialEq, Eq)]
enum ConsoleCommand<'a> {
    Empty,
    Help,
    Send { monitor: &'a str, message: Vec<u8> },
}

fn hex_digit(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

fn parse_send_message(message: &str) -> Result<Vec<u8>, String> {
    let Some(quoted) = message.strip_prefix('"') else {
        if !message.is_ascii() {
            return Err(NON_ASCII_LITERAL_ERROR.into());
        }
        return Ok(message.as_bytes().to_vec());
    };

    let mut parsed = Vec::new();
    let mut chars = quoted.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if chars.as_str().trim().is_empty() {
                    return Ok(parsed);
                }
                return Err("unexpected text after quoted message".into());
            }
            '\\' => match chars.next() {
                Some('"') => parsed.push(b'"'),
                Some('\\') => parsed.push(b'\\'),
                Some('x') => {
                    let high = chars.next().and_then(hex_digit);
                    let low = chars.next().and_then(hex_digit);
                    let (Some(high), Some(low)) = (high, low) else {
                        return Err("\\x escape requires exactly two hexadecimal digits".into());
                    };
                    parsed.push((high << 4) | low);
                }
                Some(_) => {
                    return Err(
                        "quoted messages support only escaped quotes, backslashes, and \\xHH bytes"
                            .into(),
                    );
                }
                None => return Err("unterminated escape in quoted message".into()),
            },
            _ => {
                if !ch.is_ascii() {
                    return Err(NON_ASCII_LITERAL_ERROR.into());
                }
                parsed.push(ch as u8);
            }
        }
    }
    Err("unterminated quoted message".into())
}

fn parse_command(line: &str) -> Result<ConsoleCommand<'_>, String> {
    let line = line.trim_start();
    if line.is_empty() {
        return Ok(ConsoleCommand::Empty);
    }
    let end = line.find(char::is_whitespace).unwrap_or(line.len());
    let (verb, rest) = line.split_at(end);
    match verb {
        "help" if rest.trim().is_empty() => Ok(ConsoleCommand::Help),
        "send" => {
            let rest = rest.trim_start();
            let end = rest
                .find(char::is_whitespace)
                .ok_or_else(|| "usage: send <monitor> <message>".to_owned())?;
            let (monitor, message) = rest.split_at(end);
            if monitor.is_empty() {
                return Err("usage: send <monitor> <message>".into());
            }
            let message = parse_send_message(message.trim_start())?;
            Ok(ConsoleCommand::Send { monitor, message })
        }
        _ => Err(format!(
            "unknown console command '{verb}'; type help for usage"
        )),
    }
}

pub(crate) fn run_console(simulation: SimulationHandle) {
    eprintln!("console: send <monitor> <message> (type help for usage)");
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("console input error: {error}");
                break;
            }
        };
        match parse_command(&line) {
            Ok(ConsoleCommand::Empty) => {}
            Ok(ConsoleCommand::Help) => {
                eprintln!(
                    "send <monitor> <message>  send ASCII text plus LF to that serial monitor"
                );
                eprintln!(r#"send <monitor> "<message>"  preserve spaces; escapes: \", \\, \xHH"#);
                eprintln!("help                       show these commands");
            }
            Ok(ConsoleCommand::Send { monitor, message }) => {
                let mut bytes = message;
                bytes.push(b'\n');
                if let Err(error) = simulation.send_serial(monitor, &bytes) {
                    eprintln!("console: {error}");
                }
            }
            Err(error) => eprintln!("console: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsoleCommand, parse_command};

    #[test]
    fn send_ignores_separator_spacing_and_selects_monitor() {
        assert_eq!(
            parse_command("send alpha hello world"),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: b"hello world".to_vec()
            })
        );
        assert_eq!(
            parse_command("  send\talpha  padded  "),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: b"padded  ".to_vec()
            })
        );
        assert_eq!(
            parse_command("send alpha "),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: Vec::new()
            })
        );
        assert!(parse_command("send alpha").is_err());
        assert_eq!(parse_command("help"), Ok(ConsoleCommand::Help));
    }

    #[test]
    fn quoted_send_preserves_edge_spaces_and_decodes_escapes() {
        assert_eq!(
            parse_command("send alpha  \"  padded  \"  "),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: b"  padded  ".to_vec()
            })
        );
        assert_eq!(
            parse_command(r#"send alpha "say \"hi\" and \\""#),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: b"say \"hi\" and \\".to_vec()
            })
        );
        assert_eq!(
            parse_command("send alpha \"\""),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: Vec::new()
            })
        );
    }

    #[test]
    fn quoted_send_accepts_raw_hex_bytes() {
        assert_eq!(
            parse_command(r#"send alpha "A\x00\x7f\xFF""#),
            Ok(ConsoleCommand::Send {
                monitor: "alpha",
                message: vec![b'A', 0, 0x7f, 0xff]
            })
        );
    }

    #[test]
    fn quoted_send_rejects_incomplete_or_ambiguous_input() {
        assert!(parse_command("send alpha \"unfinished").is_err());
        assert!(parse_command("send alpha \"hello\" extra").is_err());
        assert!(parse_command(r#"send alpha "bad\q""#).is_err());
        assert!(parse_command("send alpha \"trailing\\").is_err());
        assert!(parse_command(r#"send alpha "\x""#).is_err());
        assert!(parse_command(r#"send alpha "\x0""#).is_err());
        assert!(parse_command(r#"send alpha "\x0G""#).is_err());
    }

    #[test]
    fn send_rejects_non_ascii_literal_text() {
        assert!(parse_command("send alpha café").is_err());
        assert!(parse_command("send alpha \"café\"").is_err());
    }
}

use std::path::PathBuf;

use tokn_session_client::Source;

#[derive(Debug)]
pub struct Cli {
  pub command: Command,
}

#[derive(Debug)]
pub enum Command {
  Help,
  List {
    source: Source,
    session_dir: Option<PathBuf>,
    limit: usize,
  },
  Show {
    source: Source,
    session: String,
    format: Format,
    scope: ShowScope,
    session_dir: Option<PathBuf>,
  },
  Browse {
    source: Source,
    session: Option<String>,
    session_dir: Option<PathBuf>,
  },
  Create {
    source: Source,
    prompt: String,
    executor: Option<String>,
    cwd: Option<PathBuf>,
  },
  Append {
    source: Source,
    target: AppendTarget,
    prompt: String,
    executor: Option<String>,
    cwd: Option<PathBuf>,
  },
}

#[derive(Debug)]
pub enum AppendTarget {
  Continue,
  Session(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
  Pretty,
  Jsonl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowScope {
  SelfOnly,
  Tree,
}

pub fn parse(args: Vec<String>) -> Result<Cli, String> {
  let Some((command, args)) = args.split_first() else {
    return Err(help());
  };
  let options = match command.as_str() {
    "--help" | "-h" | "help" => return Ok(Cli { command: Command::Help }),
    "list" | "show" | "browse" | "create" | "append" => match parse_options(args)? {
      ParsedOptions::Help => return Ok(Cli { command: Command::Help }),
      ParsedOptions::Options(options) => options,
    },
    other => return Err(format!("unknown command `{other}`\n\n{}", help())),
  };
  let Options {
    source,
    format,
    session_dir,
    limit,
    executor,
    cwd,
    append_target,
    scope,
    mut positionals,
  } = options;

  let command = match command.as_str() {
    "list" => {
      reject_append_target("list", append_target)?;
      reject_show_scope("list", scope)?;
      if !positionals.is_empty() {
        return Err("list does not accept positional arguments".to_string());
      }
      Command::List {
        source,
        session_dir,
        limit,
      }
    }
    "show" => {
      reject_append_target("show", append_target)?;
      let scope = scope.unwrap_or(ShowScope::SelfOnly);
      if matches!((scope, format), (ShowScope::Tree, Format::Jsonl)) {
        return Err("show --scope tree currently supports only --format pretty".to_string());
      }
      if positionals.len() != 1 {
        return Err("show requires exactly one session id or path".to_string());
      }
      Command::Show {
        source,
        session: positionals.remove(0),
        format,
        scope,
        session_dir,
      }
    }
    "browse" => {
      reject_append_target("browse", append_target)?;
      reject_show_scope("browse", scope)?;
      if positionals.len() > 1 {
        return Err("browse accepts at most one session id or path".to_string());
      }
      Command::Browse {
        source,
        session: positionals.pop(),
        session_dir,
      }
    }
    "create" => {
      reject_append_target("create", append_target)?;
      reject_show_scope("create", scope)?;
      if positionals.len() != 1 {
        return Err("create requires exactly one prompt".to_string());
      }
      Command::Create {
        source,
        prompt: positionals.remove(0),
        executor,
        cwd,
      }
    }
    "append" => {
      reject_show_scope("append", scope)?;
      if positionals.len() != 1 {
        return Err("append requires exactly one prompt".to_string());
      }
      let target = append_target.ok_or_else(|| "append requires --continue or --session <id>".to_string())?;
      Command::Append {
        source,
        target,
        prompt: positionals.remove(0),
        executor,
        cwd,
      }
    }
    _ => unreachable!("command was validated before parsing options"),
  };
  Ok(Cli { command })
}

struct Options {
  source: Source,
  format: Format,
  session_dir: Option<PathBuf>,
  limit: usize,
  executor: Option<String>,
  cwd: Option<PathBuf>,
  append_target: Option<AppendTarget>,
  scope: Option<ShowScope>,
  positionals: Vec<String>,
}

enum ParsedOptions {
  Help,
  Options(Options),
}

fn parse_options(args: &[String]) -> Result<ParsedOptions, String> {
  let mut source = Source::Pi;
  let mut format = Format::Pretty;
  let mut session_dir = None;
  let mut limit = 20;
  let mut executor = None;
  let mut cwd = None;
  let mut append_target = None;
  let mut scope = None;
  let mut positionals = Vec::new();
  let mut index = 0;

  while index < args.len() {
    match args[index].as_str() {
      "--source" => {
        index += 1;
        let value = args.get(index).ok_or_else(|| "--source requires a value".to_string())?;
        source = parse_source(value)?;
      }
      "--format" => {
        index += 1;
        let value = args.get(index).ok_or_else(|| "--format requires a value".to_string())?;
        format = parse_format(value)?;
      }
      "--session-dir" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--session-dir requires a value".to_string())?;
        session_dir = Some(PathBuf::from(value));
      }
      "--limit" => {
        index += 1;
        let value = args.get(index).ok_or_else(|| "--limit requires a value".to_string())?;
        limit = value
          .parse::<usize>()
          .map_err(|_| format!("invalid --limit value `{value}`"))?;
      }
      "--executor" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--executor requires a value".to_string())?;
        executor = Some(value.to_string());
      }
      "--cwd" => {
        index += 1;
        let value = args.get(index).ok_or_else(|| "--cwd requires a value".to_string())?;
        cwd = Some(PathBuf::from(value));
      }
      "--continue" => {
        if append_target.is_some() {
          return Err("append accepts only one of --continue or --session <id>".to_string());
        }
        append_target = Some(AppendTarget::Continue);
      }
      "--session" => {
        if append_target.is_some() {
          return Err("append accepts only one of --continue or --session <id>".to_string());
        }
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--session requires a value".to_string())?;
        append_target = Some(AppendTarget::Session(value.to_string()));
      }
      "--scope" => {
        index += 1;
        let value = args.get(index).ok_or_else(|| "--scope requires a value".to_string())?;
        scope = Some(parse_show_scope(value)?);
      }
      "--help" | "-h" => return Ok(ParsedOptions::Help),
      value if value.starts_with('-') => return Err(format!("unknown option `{value}`")),
      value => positionals.push(value.to_string()),
    }
    index += 1;
  }

  Ok(ParsedOptions::Options(Options {
    source,
    format,
    session_dir,
    limit,
    executor,
    cwd,
    append_target,
    scope,
    positionals,
  }))
}

fn parse_source(value: &str) -> Result<Source, String> {
  match value {
    "pi" => Ok(Source::Pi),
    "codex" => Ok(Source::Codex),
    "opencode" => Ok(Source::OpenCode),
    "zcode" => Ok(Source::ZCode),
    "workbuddy" => Ok(Source::WorkBuddy),
    "dsh" => Ok(Source::Dsh),
    _ => Err(format!("unknown source `{value}`")),
  }
}

fn reject_append_target(command: &str, target: Option<AppendTarget>) -> Result<(), String> {
  if target.is_some() {
    return Err(format!(
      "--continue and --session are only valid for append, not {command}"
    ));
  }
  Ok(())
}

fn parse_format(value: &str) -> Result<Format, String> {
  match value {
    "pretty" => Ok(Format::Pretty),
    "jsonl" => Ok(Format::Jsonl),
    _ => Err(format!("unknown format `{value}`")),
  }
}

fn parse_show_scope(value: &str) -> Result<ShowScope, String> {
  match value {
    "self" => Ok(ShowScope::SelfOnly),
    "tree" => Ok(ShowScope::Tree),
    _ => Err(format!("unknown show scope `{value}`")),
  }
}

fn reject_show_scope(command: &str, scope: Option<ShowScope>) -> Result<(), String> {
  if scope.is_some() {
    return Err(format!("--scope is only valid for show, not {command}"));
  }
  Ok(())
}

pub fn help() -> String {
  "usage:
  tokn-session list [--source pi|codex|opencode|zcode|workbuddy|dsh] [--session-dir <dir>]
  tokn-session list [--source pi|codex|opencode|zcode|workbuddy|dsh] [--limit <n>]
  tokn-session show [--source pi|codex|opencode|zcode|workbuddy|dsh] [--format pretty|jsonl] [--scope self|tree] [--session-dir <dir>] <session-id-or-path>
  tokn-session browse [--source pi|codex|opencode|zcode|workbuddy|dsh] [--session-dir <dir>] [session-id-or-path]
  tokn-session create [--source pi|codex|opencode] [--executor <command>] [--cwd <dir>] <prompt>
  tokn-session append [--source pi|codex|opencode] [--executor <command>] [--cwd <dir>] (--continue|--session <id>) <prompt>"
    .to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn show_defaults_to_self_scope() {
    let cli = parse(vec!["show".to_string(), "session".to_string()]).expect("show should parse");

    assert!(matches!(
      cli.command,
      Command::Show {
        scope: ShowScope::SelfOnly,
        ..
      }
    ));
  }

  #[test]
  fn show_accepts_tree_scope_for_pretty_output() {
    let cli = parse(vec![
      "show".to_string(),
      "--scope".to_string(),
      "tree".to_string(),
      "session".to_string(),
    ])
    .expect("tree show should parse");

    assert!(matches!(
      cli.command,
      Command::Show {
        scope: ShowScope::Tree,
        format: Format::Pretty,
        ..
      }
    ));
  }

  #[test]
  fn tree_scope_rejects_ambiguous_jsonl_output() {
    let error = parse(vec![
      "show".to_string(),
      "--scope".to_string(),
      "tree".to_string(),
      "--format".to_string(),
      "jsonl".to_string(),
      "session".to_string(),
    ])
    .expect_err("tree jsonl should be rejected");

    assert_eq!(error, "show --scope tree currently supports only --format pretty");
  }

  #[test]
  fn self_scope_keeps_jsonl_available() {
    let cli = parse(vec![
      "show".to_string(),
      "--format".to_string(),
      "jsonl".to_string(),
      "session".to_string(),
    ])
    .expect("self jsonl should parse");

    assert!(matches!(
      cli.command,
      Command::Show {
        scope: ShowScope::SelfOnly,
        format: Format::Jsonl,
        ..
      }
    ));
  }

  #[test]
  fn scope_is_rejected_outside_show() {
    let error = parse(vec!["list".to_string(), "--scope".to_string(), "self".to_string()])
      .expect_err("list scope should be rejected");

    assert_eq!(error, "--scope is only valid for show, not list");
  }

  #[test]
  fn list_accepts_zcode_as_a_historical_source() {
    let cli =
      parse(vec!["list".to_string(), "--source".to_string(), "zcode".to_string()]).expect("zcode list should parse");

    assert!(matches!(
      cli.command,
      Command::List {
        source: Source::ZCode,
        ..
      }
    ));
  }

  #[test]
  fn list_accepts_workbuddy_as_a_historical_source() {
    let cli = parse(vec![
      "list".to_string(),
      "--source".to_string(),
      "workbuddy".to_string(),
    ])
    .expect("workbuddy list should parse");

    assert!(matches!(
      cli.command,
      Command::List {
        source: Source::WorkBuddy,
        ..
      }
    ));
  }

  #[test]
  fn browse_accepts_workbuddy_as_a_historical_source() {
    let cli = parse(vec![
      "browse".to_string(),
      "--source".to_string(),
      "workbuddy".to_string(),
      "wb-chat-basic".to_string(),
    ])
    .expect("workbuddy browse should parse");

    assert!(matches!(
      cli.command,
      Command::Browse {
        source: Source::WorkBuddy,
        session: Some(session),
        ..
      } if session == "wb-chat-basic"
    ));
  }
}

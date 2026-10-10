mod args;
mod browser;

use args::{AppendTarget, Command, Format, ShowScope};
use browser::{browse_session, browse_sessions};
use tokn_session_client::{AgentClient, AppendAction, AppendSessionRequest, CreateSessionRequest};
use tokn_session_render::{render_pretty, render_session_jsonl, render_session_list, render_session_tree};

fn main() {
  if let Err(err) = run() {
    eprintln!("error: {err}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), String> {
  let cli = args::parse(std::env::args().skip(1).collect())?;

  match cli.command {
    Command::Help => {
      println!("{}", args::help());
      Ok(())
    }
    Command::List {
      source,
      session_dir,
      limit,
    } => {
      let mut sessions = AgentClient::list_sessions(source, session_dir)?;
      if limit > 0 {
        sessions.truncate(limit);
      }
      print!("{}", render_session_list(&sessions));
      Ok(())
    }
    Command::Show {
      source,
      session,
      format,
      scope,
      session_dir,
    } => {
      match scope {
        ShowScope::SelfOnly => {
          let loaded = AgentClient::load_session(source, session_dir, &session)?;
          match format {
            Format::Pretty => print!("{}", render_pretty(&loaded)),
            Format::Jsonl => print!("{}", render_session_jsonl(&loaded)?),
          }
        }
        ShowScope::Tree => {
          let loaded = AgentClient::load_session_tree(source, session_dir, &session)?;
          print!("{}", render_session_tree(&loaded));
        }
      }
      Ok(())
    }
    Command::Browse {
      source,
      session,
      session_dir,
    } => {
      if let Some(session) = session {
        let loaded = AgentClient::load_session(source, session_dir, &session)?;
        return browse_session(&loaded);
      }

      let sessions = AgentClient::list_sessions(source, session_dir.clone())?;
      browse_sessions(sessions, |session| {
        AgentClient::load_session(source, session_dir.clone(), session)
      })
    }
    Command::Create {
      source,
      prompt,
      executor,
      cwd,
    } => AgentClient::create_session(CreateSessionRequest {
      source,
      executor,
      cwd,
      prompt,
    }),
    Command::Append {
      source,
      target,
      prompt,
      executor,
      cwd,
    } => {
      let action = match target {
        AppendTarget::Continue => AppendAction::Continue { prompt },
        AppendTarget::Session(session_id) => AppendAction::Session { session_id, prompt },
      };
      AgentClient::append_session(AppendSessionRequest {
        source,
        executor,
        cwd,
        action,
      })
    }
  }
}

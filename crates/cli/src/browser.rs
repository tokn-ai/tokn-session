use std::collections::BTreeSet;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tokn_session_core::{AgentEvent, LoadedSession, SessionHistoryStatus, SessionRef};
use tokn_session_render::{EventDisplay, display_event};

pub fn browse_session(session: &LoadedSession) -> Result<(), String> {
  ensure_browsable_history(session)?;
  let mut terminal = BrowserTerminal::enter()?;
  let result = EventBrowser::new(session).run(&mut terminal.terminal);
  terminal.leave()?;
  result
}

pub fn browse_sessions<F>(sessions: Vec<SessionRef>, mut load_session: F) -> Result<(), String>
where
  F: FnMut(&str) -> Result<LoadedSession, String>,
{
  let mut terminal = BrowserTerminal::enter()?;
  let result = SessionBrowser::new(sessions).run(&mut terminal.terminal, &mut load_session);
  terminal.leave()?;
  result
}

struct BrowserTerminal {
  terminal: Terminal<CrosstermBackend<Stdout>>,
  restored: bool,
}

impl BrowserTerminal {
  fn enter() -> Result<Self, String> {
    enable_raw_mode().map_err(|err| format!("failed to enable raw mode: {err}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|err| format!("failed to enter alternate screen: {err}"))?;
    let terminal =
      Terminal::new(CrosstermBackend::new(stdout)).map_err(|err| format!("failed to create terminal: {err}"))?;
    Ok(Self {
      terminal,
      restored: false,
    })
  }

  fn leave(&mut self) -> Result<(), String> {
    if self.restored {
      return Ok(());
    }
    disable_raw_mode().map_err(|err| format!("failed to disable raw mode: {err}"))?;
    execute!(self.terminal.backend_mut(), LeaveAlternateScreen)
      .map_err(|err| format!("failed to leave alternate screen: {err}"))?;
    self.restored = true;
    Ok(())
  }
}

impl Drop for BrowserTerminal {
  fn drop(&mut self) {
    let _ = self.leave();
  }
}

struct EventBrowser {
  session_id: String,
  rows: Vec<EventRow>,
  selected: usize,
  scroll: usize,
  expanded: BTreeSet<usize>,
}

struct SessionBrowser {
  sessions: Vec<SessionRef>,
  selected: usize,
  scroll: usize,
}

enum SessionBrowserAction {
  Continue,
  Quit,
  Open(String),
}

impl SessionBrowser {
  fn new(sessions: Vec<SessionRef>) -> Self {
    Self {
      sessions,
      selected: 0,
      scroll: 0,
    }
  }

  fn run<F>(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>, load_session: &mut F) -> Result<(), String>
  where
    F: FnMut(&str) -> Result<LoadedSession, String>,
  {
    loop {
      terminal
        .draw(|frame| self.draw(frame))
        .map_err(|err| format!("failed to draw session browser: {err}"))?;

      if !event::poll(Duration::from_millis(200)).map_err(|err| format!("failed to poll events: {err}"))? {
        continue;
      }

      let Event::Key(key) = event::read().map_err(|err| format!("failed to read event: {err}"))? else {
        continue;
      };
      match self.handle_key(key) {
        SessionBrowserAction::Continue => {}
        SessionBrowserAction::Quit => return Ok(()),
        SessionBrowserAction::Open(id) => {
          let loaded = load_session(&id)?;
          ensure_browsable_history(&loaded)?;
          EventBrowser::new(&loaded).run(terminal)?;
        }
      }
    }
  }

  fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    let help = format!(
      "Sessions  {}/{}  j/k move  Enter open  q quit",
      self.selected.saturating_add(1),
      self.sessions.len()
    );
    frame.render_widget(
      Paragraph::new(help).block(Block::default().borders(Borders::BOTTOM)),
      chunks[0],
    );

    let height = chunks[1].height as usize;
    self.keep_selected_visible(height);
    frame.render_widget(Paragraph::new(self.visible_lines(height)), chunks[1]);
  }

  fn handle_key(&mut self, key: KeyEvent) -> SessionBrowserAction {
    match key.code {
      KeyCode::Char('q') | KeyCode::Esc => SessionBrowserAction::Quit,
      KeyCode::Enter => self
        .sessions
        .get(self.selected)
        .map(|session| SessionBrowserAction::Open(session.id.clone()))
        .unwrap_or(SessionBrowserAction::Continue),
      KeyCode::Char('j') | KeyCode::Down => {
        self.select_next();
        SessionBrowserAction::Continue
      }
      KeyCode::Char('k') | KeyCode::Up => {
        self.select_previous();
        SessionBrowserAction::Continue
      }
      KeyCode::Char('g') | KeyCode::Home => {
        self.select_first();
        SessionBrowserAction::Continue
      }
      KeyCode::Char('G') | KeyCode::End => {
        self.select_last();
        SessionBrowserAction::Continue
      }
      KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
        self.page_down();
        SessionBrowserAction::Continue
      }
      KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
        self.page_up();
        SessionBrowserAction::Continue
      }
      _ => SessionBrowserAction::Continue,
    }
  }

  fn select_next(&mut self) {
    if self.selected + 1 < self.sessions.len() {
      self.selected += 1;
    }
  }

  fn select_previous(&mut self) {
    self.selected = self.selected.saturating_sub(1);
  }

  fn select_first(&mut self) {
    self.selected = 0;
  }

  fn select_last(&mut self) {
    if !self.sessions.is_empty() {
      self.selected = self.sessions.len() - 1;
    }
  }

  fn page_down(&mut self) {
    self.selected = (self.selected + 10).min(self.sessions.len().saturating_sub(1));
  }

  fn page_up(&mut self) {
    self.selected = self.selected.saturating_sub(10);
  }

  fn keep_selected_visible(&mut self, height: usize) {
    if height == 0 {
      return;
    }
    if self.selected < self.scroll {
      self.scroll = self.selected;
    }
    if self.selected >= self.scroll + height {
      self.scroll = self.selected.saturating_sub(height - 1);
    }
  }

  fn visible_lines(&self, height: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for index in self.scroll..self.sessions.len() {
      if lines.len() >= height {
        break;
      }
      let session = &self.sessions[index];
      let cursor = if index == self.selected { ">" } else { " " };
      let text = format!(
        "{cursor} {:04} {:<36} {:<25} {:>6}  {}",
        index + 1,
        truncate(&session.id, 36),
        truncate(session.timestamp.as_deref().unwrap_or("-"), 25),
        session.message_count,
        session.cwd.as_deref().unwrap_or("-"),
      );
      if index == self.selected {
        lines.push(Line::from(Span::styled(
          text,
          Style::default().add_modifier(Modifier::REVERSED),
        )));
      } else {
        lines.push(Line::from(text));
      }
    }
    lines
  }
}

impl EventBrowser {
  fn new(session: &LoadedSession) -> Self {
    Self {
      session_id: session.reference.id.clone(),
      rows: session.events.iter().enumerate().map(EventRow::new).collect(),
      selected: 0,
      scroll: 0,
      expanded: BTreeSet::new(),
    }
  }

  fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), String> {
    loop {
      terminal
        .draw(|frame| self.draw(frame))
        .map_err(|err| format!("failed to draw browser: {err}"))?;

      if !event::poll(Duration::from_millis(200)).map_err(|err| format!("failed to poll events: {err}"))? {
        continue;
      }

      if let Event::Key(key) = event::read().map_err(|err| format!("failed to read event: {err}"))? {
        if self.handle_key(key) {
          return Ok(());
        }
      }
    }
  }

  fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    let help = format!(
      "Session {}  {}/{}  j/k move  h/l collapse/expand  Enter toggle  z solo  C collapse all  q quit",
      self.session_id,
      self.selected.saturating_add(1),
      self.rows.len()
    );
    frame.render_widget(
      Paragraph::new(help).block(Block::default().borders(Borders::BOTTOM)),
      chunks[0],
    );

    let height = chunks[1].height as usize;
    self.keep_selected_visible(height);
    let lines = self.visible_lines(height);
    frame.render_widget(Paragraph::new(lines), chunks[1]);
  }

  fn handle_key(&mut self, key: KeyEvent) -> bool {
    match key.code {
      KeyCode::Char('q') | KeyCode::Esc => return true,
      KeyCode::Char('j') | KeyCode::Down => self.select_next(),
      KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
      KeyCode::Char('g') | KeyCode::Home => self.select_first(),
      KeyCode::Char('G') | KeyCode::End => self.select_last(),
      KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => self.page_down(),
      KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.page_up(),
      KeyCode::Char('h') => self.collapse_selected(),
      KeyCode::Char('l') => self.expand_selected(),
      KeyCode::Char('z') => self.solo_selected(),
      KeyCode::Char('C') => self.expanded.clear(),
      KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected(),
      _ => {}
    }
    false
  }

  fn select_next(&mut self) {
    if self.selected + 1 < self.rows.len() {
      self.selected += 1;
    }
  }

  fn select_previous(&mut self) {
    self.selected = self.selected.saturating_sub(1);
  }

  fn select_first(&mut self) {
    self.selected = 0;
  }

  fn select_last(&mut self) {
    if !self.rows.is_empty() {
      self.selected = self.rows.len() - 1;
    }
  }

  fn page_down(&mut self) {
    self.selected = (self.selected + 10).min(self.rows.len().saturating_sub(1));
  }

  fn page_up(&mut self) {
    self.selected = self.selected.saturating_sub(10);
  }

  fn collapse_selected(&mut self) {
    self.expanded.remove(&self.selected);
  }

  fn expand_selected(&mut self) {
    self.expanded.insert(self.selected);
  }

  fn toggle_selected(&mut self) {
    if !self.expanded.remove(&self.selected) {
      self.expanded.insert(self.selected);
    }
  }

  fn solo_selected(&mut self) {
    self.expanded.clear();
    self.expanded.insert(self.selected);
  }

  fn keep_selected_visible(&mut self, height: usize) {
    if height == 0 {
      return;
    }
    if self.selected < self.scroll {
      self.scroll = self.selected;
    }
    while self.selected_header_offset() >= height && self.scroll < self.selected {
      self.scroll += 1;
    }
  }

  fn selected_header_offset(&self) -> usize {
    (self.scroll..self.selected).map(|index| self.row_height(index)).sum()
  }

  fn row_height(&self, index: usize) -> usize {
    let detail_lines = if self.expanded.contains(&index) {
      self.rows[index].detail_line_count()
    } else {
      0
    };
    1 + detail_lines
  }

  fn visible_lines(&self, height: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for index in self.scroll..self.rows.len() {
      if lines.len() >= height {
        break;
      }
      let row = &self.rows[index];
      let selected = index == self.selected;
      let expanded = self.expanded.contains(&index);
      lines.push(row.header_line(index, selected, expanded));
      if expanded {
        for detail in row.detail_lines() {
          if lines.len() >= height {
            break;
          }
          lines.push(detail);
        }
      }
    }
    lines
  }
}

struct EventRow {
  kind: &'static str,
  summary: String,
  detail: String,
}

impl EventRow {
  fn new((index, event): (usize, &AgentEvent)) -> Self {
    let _ = index;
    Self::from(display_event(event))
  }

  fn header_line(&self, index: usize, selected: bool, expanded: bool) -> Line<'static> {
    let cursor = if selected { ">" } else { " " };
    let fold = if expanded { "-" } else { "+" };
    let text = format!("{cursor} {:04} {fold} {:<9} {}", index + 1, self.kind, self.summary);
    if selected {
      Line::from(Span::styled(text, Style::default().add_modifier(Modifier::REVERSED)))
    } else {
      Line::from(text)
    }
  }

  fn detail_lines(&self) -> Vec<Line<'static>> {
    self
      .detail
      .trim_matches('\n')
      .lines()
      .map(|line| Line::from(format!("      {line}")))
      .collect()
  }

  fn detail_line_count(&self) -> usize {
    self.detail.trim_matches('\n').lines().count()
  }
}

impl From<EventDisplay> for EventRow {
  fn from(display: EventDisplay) -> Self {
    Self {
      kind: display.kind,
      summary: display.summary,
      detail: display.detail,
    }
  }
}

fn ensure_browsable_history(session: &LoadedSession) -> Result<(), String> {
  if session.history_status == SessionHistoryStatus::SubagentBodyUnavailable {
    return Err(format!(
      "subagent session `{}` body is unavailable because no trigger-turn boundary was recorded",
      session.reference.id
    ));
  }
  Ok(())
}

fn truncate(value: &str, max: usize) -> String {
  let mut output = String::new();
  for character in value.chars().take(max) {
    output.push(character);
  }
  output
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::*;

  #[test]
  fn keep_selected_visible_accounts_for_expanded_rows() {
    let mut browser = EventBrowser {
      session_id: "session".to_string(),
      rows: vec![
        test_row("row 1", "one\ntwo\nthree\nfour\nfive"),
        test_row("row 2", ""),
        test_row("row 3", ""),
      ],
      selected: 2,
      scroll: 0,
      expanded: BTreeSet::from([0]),
    };

    browser.keep_selected_visible(3);

    assert_eq!(browser.scroll, 1);
    assert!(browser.selected_header_offset() < 3);
  }

  #[test]
  fn unavailable_subagent_history_is_not_browsed_as_an_empty_session() {
    let session = LoadedSession {
      reference: SessionRef {
        model_baseline: None,
        id: "child".to_string(),
        parent_session_id: Some("parent".to_string()),
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        title: None,
        preview: None,
        path: PathBuf::from("child.jsonl"),
        cwd: None,
        timestamp: None,
        message_count: 0,
      },
      events: Vec::new(),
      history_status: SessionHistoryStatus::SubagentBodyUnavailable,
    };

    assert_eq!(
      ensure_browsable_history(&session).expect_err("unavailable body should be rejected"),
      "subagent session `child` body is unavailable because no trigger-turn boundary was recorded"
    );
  }

  fn test_row(summary: &str, detail: &str) -> EventRow {
    EventRow {
      kind: "test",
      summary: summary.to_string(),
      detail: detail.to_string(),
    }
  }
}

//! Associate recorded goal work and its outcome with one explicit Codex turn.
//! The prompt can precede task_started. The goal clock belongs to the goal
//! receipt and can differ from the work disclosure's elapsed time.
use super::{AgentEvent, EventSummary, MessageDelivery, Phase, Provider, Role, TimelineEntry};
use crate::model::GoalPresentation;
use std::collections::{BTreeMap, HashMap};
use tokn_session_core::LifecycleScope;

#[derive(Default)]
pub(super) struct GoalProjection {
  // Half-open source ranges in turn order. Some turns have multiple work
  // disclosures (for example an outstanding question between two operations).
  work: BTreeMap<usize, usize>,
  replies: HashMap<usize, GoalPresentation>,
}

struct Turn<'a> {
  id: &'a str,
  session: Option<&'a str>,
  start: usize,
  is_goal: bool,
  reply: Option<usize>,
  outcome: Option<GoalPresentation>,
}

impl GoalProjection {
  pub(super) fn new(events: &[AgentEvent]) -> Self {
    let mut projection = Self::default();
    let mut turn: Option<Turn<'_>> = None;
    for (index, event) in events.iter().enumerate().filter(|(_, e)| !e.is_hidden()) {
      match event {
        AgentEvent::SessionStarted(_) => {
          projection.finish(turn.take(), index);
        }
        AgentEvent::Lifecycle(l) if l.provider == Provider::Codex && matches!(l.scope, LifecycleScope::Turn) => {
          if l.phase == Phase::Started {
            if turn
              .as_ref()
              .is_some_and(|t| t.id == l.turn_id && t.session == l.session_id.as_deref())
            {
              continue;
            }
            projection.finish(turn.take(), index);
            turn = Some(Turn {
              id: &l.turn_id,
              session: l.session_id.as_deref(),
              start: index,
              is_goal: l
                .native
                .pointer("/turn_attribution/turn_trigger")
                .and_then(|v| v.as_str())
                == Some("goal"),
              reply: None,
              outcome: None,
            });
          } else if l.phase == Phase::Finished
            && turn
              .as_ref()
              .is_some_and(|t| t.id == l.turn_id && t.session == l.session_id.as_deref())
          {
            projection.finish(turn.take(), index + 1);
          }
        }
        AgentEvent::GoalUpdated(update) if update.provider == Provider::Codex => {
          let Some(t) = &mut turn else { continue };
          if update.turn_id.as_deref() != Some(t.id) || update.session_id.as_deref() != t.session || t.session.is_none()
          {
            continue;
          }
          let Some(goal) = update.goal.as_ref().filter(|goal| !goal.is_null()) else {
            t.outcome = None;
            continue;
          };
          if goal.get("threadId").is_some_and(|id| id.as_str() != t.session)
            || goal["objective"].as_str().is_none_or(|s| s.trim().is_empty())
          {
            continue;
          }
          match goal["status"].as_str() {
            Some("complete") => {
              t.outcome = Some(GoalPresentation::Completed {
                duration_ms: goal["timeUsedSeconds"]
                  .as_u64()
                  .map(|s| (u128::from(s) * 1000).to_string()),
              });
            }
            Some(_) => t.outcome = None,
            None => {}
          }
        }
        AgentEvent::Message(m) if m.role == Role::User && turn.as_ref().is_some_and(|t| t.reply.is_some()) => {
          // A prompt may precede the next turn-start record.
          projection.finish(turn.take(), index);
        }
        AgentEvent::Message(m)
          if m.provider == Provider::Codex && m.role == Role::Assistant && m.delivery == MessageDelivery::Final =>
        {
          if let Some(t) = turn.as_mut().filter(|t| t.session == m.session_id.as_deref()) {
            t.reply = Some(index);
          }
        }
        _ => {}
      }
    }
    projection.finish(turn, events.len());
    projection
  }

  fn finish(&mut self, turn: Option<Turn<'_>>, end: usize) {
    let Some(t) = turn else { return };
    if t.is_goal || t.outcome.is_some() {
      self.work.insert(t.start, end);
      if let (Some(reply), Some(outcome)) = (t.reply, t.outcome) {
        self.replies.insert(reply, outcome);
      }
    }
  }

  pub(super) fn is_goal_work(&self, index: usize) -> bool {
    self
      .work
      .range(..=index)
      .next_back()
      .is_some_and(|(_, end)| index < *end)
  }

  pub(super) fn summary(&self, entry: &TimelineEntry, mut summary: EventSummary) -> EventSummary {
    match entry {
      TimelineEntry::Event { source_event_index } => {
        if let Some(outcome) = self.replies.get(source_event_index) {
          summary.goal = Some(outcome.clone());
        }
      }
      TimelineEntry::Trajectory { trajectory } => {
        if trajectory.goal.is_some() || self.is_goal_work(trajectory.start_source_event_index) {
          summary.goal = Some(GoalPresentation::Work);
        }
      }
      TimelineEntry::ToolOperation { .. } => {}
    }
    summary
  }
}

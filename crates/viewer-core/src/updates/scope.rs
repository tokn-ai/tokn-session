//! Selection is orthogonal to richness. Unloaded work is represented by inner
//! group summaries; complete child membership is delivered only for interests.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScope {
  #[default]
  LatestTurn,
  Retained,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UpdateScope {
  #[serde(default)]
  pub history: HistoryScope,
  /// Pin the initial turn so live appends extend, rather than slide, the view.
  #[serde(default)]
  pub turn_key: Option<String>,
  #[serde(default)]
  pub group_keys: Vec<String>,
}

fn key(value: &Value) -> &str {
  value["event_key"].as_str().unwrap_or("")
}

fn group_summary(members: &[Value]) -> Value {
  let mut summary = members[0].clone();
  let mut counts = BTreeMap::<&str, usize>::new();
  let mut running = false;
  let mut failed = false;
  for member in members {
    let kind = member["tool"]["kind"]
      .as_str()
      .unwrap_or(member["type"].as_str().unwrap_or("event"));
    let category = match kind {
      "shell" => "command",
      "file_read" => "file read",
      "file_edit" => "file edit",
      "file_write" => "file write",
      "search" | "web" => "search",
      "reasoning" => "reasoning",
      "agent_activity" | "task" => "agent activity",
      "terminal" => "terminal interaction",
      "code_execution" => "code block",
      _ => "event",
    };
    *counts.entry(category).or_default() += 1;
    running |= matches!(member["tool"]["status"].as_str(), Some("pending" | "running"));
    failed |= member["is_error"] == true
      || member["type"] == "error"
      || member["tool"]["status"] == "failed"
      || member["tool"]["exit_code"].as_i64().is_some_and(|code| code != 0);
  }
  let text = counts
    .into_iter()
    .map(|(kind, count)| {
      format!(
        "{count} {}",
        match (kind, count) {
          ("search", n) if n != 1 => "searches".into(),
          ("agent activity", n) if n != 1 => "agent activities".into(),
          (_, 1) => kind.to_owned(),
          _ => format!("{kind}s"),
        }
      )
    })
    .collect::<Vec<_>>()
    .join(", ");
  summary["event_key"] = json!(format!("activity:{}", key(&members[0])));
  summary["type"] = json!("activity_group");
  summary["title"] = json!("Activity");
  summary["summary"] = json!(text);
  summary["summary_truncated"] = json!(false);
  summary["phase"] = json!(if running { "running" } else { "finished" });
  summary["is_error"] = json!(failed);
  summary["is_hidden"] = json!(false);
  summary["is_bookkeeping"] = json!(members.iter().all(|item| item["is_bookkeeping"] == true));
  summary["child_keys"] = json!(members.iter().map(key).collect::<Vec<_>>());
  for field in [
    "tool",
    "trajectory",
    "usage",
    "reasoning",
    "agent_activity",
    "compaction",
    "role",
    "slot_key",
  ] {
    summary[field] = Value::Null;
  }
  summary
}

pub(super) fn project(
  mut page: Value,
  semantic: Vec<Value>,
  mut events: Vec<Value>,
  scope: &mut UpdateScope,
  include_all: bool,
) -> (Value, Vec<Value>, Vec<Value>) {
  let start = if scope.history == HistoryScope::LatestTurn {
    scope
      .turn_key
      .as_ref()
      .and_then(|anchor| semantic.iter().position(|item| key(item) == anchor))
      .or_else(|| {
        semantic
          .iter()
          .rposition(|item| item["type"] == "message" && item["role"] == "user")
      })
      .unwrap_or(0)
  } else {
    0
  };
  if scope.history == HistoryScope::LatestTurn {
    scope.turn_key = semantic.get(start).map(|item| key(item).to_owned());
  }
  let selected = &semantic[start..];
  let allowed: HashSet<_> = selected.iter().map(key).collect();
  let by_key: HashMap<_, _> = selected.iter().map(|item| (key(item), item)).collect();
  let interests: HashSet<_> = scope.group_keys.iter().cloned().collect();
  let mut delivered = selected
    .iter()
    .filter(|item| {
      matches!(
        item["type"].as_str(),
        Some("message" | "question_request" | "question_reply" | "error")
      )
    })
    .cloned()
    .collect::<Vec<_>>();
  let mut grouped = HashSet::new();
  let mut roots = Vec::new();
  for mut root in page["events"].as_array().cloned().unwrap_or_default() {
    if root["type"] != "trajectory" {
      if allowed.contains(key(&root)) {
        roots.push(root);
      }
      continue;
    }
    let children: Vec<_> = root["child_keys"]
      .as_array()
      .into_iter()
      .flatten()
      .filter_map(|id| by_key.get(id.as_str()?))
      .map(|item| (*item).clone())
      .collect();
    if children.is_empty() {
      continue;
    }
    let mut outer_keys = Vec::<String>::new();
    let mut block = Vec::new();
    let flush = |block: &mut Vec<Value>, delivered: &mut Vec<Value>, outer_keys: &mut Vec<String>| {
      if block.is_empty() {
        return;
      }
      let group = group_summary(block);
      let group_key = key(&group).to_owned();
      outer_keys.push(group_key.clone());
      if include_all || interests.contains(group_key.as_str()) {
        delivered.extend(block.iter().cloned());
      }
      delivered.push(group);
      block.clear();
    };
    for child in children {
      grouped.insert(key(&child).to_owned());
      if child["type"] == "message" {
        flush(&mut block, &mut delivered, &mut outer_keys);
        outer_keys.push(key(&child).to_owned());
      } else {
        block.push(child);
      }
    }
    flush(&mut block, &mut delivered, &mut outer_keys);
    root["child_keys"] = json!(outer_keys);
    roots.push(root);
  }
  // Preserve unfamiliar standalone shapes and attention context outside work.
  for item in selected {
    if !grouped.contains(key(item)) && !delivered.iter().any(|known| key(known) == key(item)) {
      delivered.push(item.clone());
    }
  }
  let outstanding: HashSet<_> = page["outstanding_questions"]
    .as_array()
    .into_iter()
    .flatten()
    .filter_map(|question| question["event_key"].as_str())
    .collect();
  for item in &semantic[..start] {
    if outstanding.contains(key(item)) {
      roots.push(item.clone());
      delivered.push(item.clone());
    }
  }
  // All keeps native records within the selected source range, independently
  // of semantic tool correlation and work interests.
  if start > 0 {
    let source_index = |key: &str| {
      key
        .rsplit_once("event.v1.")
        .and_then(|(_, index)| usize::from_str_radix(index, 16).ok())
    };
    if let Some(before) = selected.first().and_then(|item| source_index(key(item))) {
      events.retain(|event| source_index(key(event)).is_some_and(|index| index >= before));
    }
  }
  let positions: HashMap<_, _> = semantic
    .iter()
    .enumerate()
    .map(|(index, item)| (key(item), index))
    .collect();
  delivered.sort_by_key(|item| {
    positions
      .get(key(item).strip_prefix("activity:").unwrap_or(key(item)))
      .copied()
      .unwrap_or(usize::MAX)
  });
  let available: HashSet<_> = delivered
    .iter()
    .filter(|item| item["type"] == "activity_group")
    .map(key)
    .collect();
  scope.group_keys.retain(|id| available.contains(id.as_str()));
  page["events"] = json!(roots);
  page["scope"] = serde_json::to_value(scope).expect("serializable scope");
  (page, delivered, events)
}

#[cfg(test)]
mod tests {
  use super::*;
  fn message(id: &str, role: &str) -> Value {
    json!({"event_key":id,"type":"message","role":role,"summary":id})
  }
  fn tool(id: &str) -> Value {
    json!({"event_key":id,"type":"tool_call","tool":{"kind":"shell","status":"completed"},"summary":"private command"})
  }
  fn fixture() -> (Value, Vec<Value>) {
    let semantic = vec![
      message("old", "user"),
      tool("old-tool"),
      message("new", "user"),
      tool("a"),
      tool("b"),
      message("commentary", "assistant"),
      tool("c"),
      message("final", "assistant"),
    ];
    let page = json!({"events":[message("old", "user"), {"event_key":"old-work","type":"trajectory","child_keys":["old-tool"]},message("new", "user"), {"event_key":"work","type":"trajectory","child_keys":["a","b","commentary","c"]},message("final", "assistant")],"outstanding_questions":[]});
    (page, semantic)
  }
  #[test]
  fn latest_turn_defers_complete_inner_groups_and_preserves_messages() {
    let (page, semantic) = fixture();
    let mut scope = UpdateScope::default();
    let (page, items, _) = project(page, semantic, vec![], &mut scope, false);
    assert_eq!(scope.turn_key.as_deref(), Some("new"));
    assert_eq!(
      items.iter().map(key).collect::<Vec<_>>(),
      ["new", "activity:a", "commentary", "activity:c", "final"]
    );
    assert_eq!(items[1]["child_keys"], json!(["a", "b"]));
    assert_eq!(
      page["events"][1]["child_keys"],
      json!(["activity:a", "commentary", "activity:c"])
    );
    assert!(!items.iter().any(|item| item["type"] == "tool_call"));
  }
  #[test]
  fn interests_deliver_one_whole_group_and_history_can_expand_independently() {
    let (page, semantic) = fixture();
    let mut scope = UpdateScope {
      history: HistoryScope::Retained,
      group_keys: vec!["activity:a".into()],
      ..Default::default()
    };
    let (_, items, _) = project(page, semantic, vec![], &mut scope, false);
    assert!(items.iter().any(|item| key(item) == "old"));
    assert!(items.iter().any(|item| key(item) == "a"));
    assert!(items.iter().any(|item| key(item) == "b"));
    assert!(!items.iter().any(|item| key(item) == "c" || key(item) == "old-tool"));
  }
  #[test]
  fn pinned_turn_survives_new_user_messages_and_retains_outstanding_attention() {
    let (mut page, mut semantic) = fixture();
    semantic.insert(0, json!({"event_key":"question","type":"question_request"}));
    semantic.push(message("next", "user"));
    page["outstanding_questions"] = json!([{"event_key":"question"}]);
    page["events"].as_array_mut().unwrap().push(message("next", "user"));
    let mut scope = UpdateScope {
      turn_key: Some("new".into()),
      ..Default::default()
    };
    let (_, items, _) = project(page, semantic, vec![], &mut scope, false);
    assert!(items.iter().any(|item| key(item) == "question"));
    assert!(items.iter().any(|item| key(item) == "new"));
    assert!(items.iter().any(|item| key(item) == "next"));
    assert_eq!(scope.turn_key.as_deref(), Some("new"));
  }
  #[test]
  fn all_includes_complete_membership_without_group_interests() {
    let (page, semantic) = fixture();
    let (_, items, _) = project(page, semantic, vec![], &mut UpdateScope::default(), true);
    assert!(items.iter().any(|item| key(item) == "a"));
    assert!(items.iter().any(|item| key(item) == "b"));
    assert!(items.iter().any(|item| key(item) == "c"));
    assert!(!items.iter().any(|item| key(item) == "old-tool"));
  }
}

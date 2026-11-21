//! Standalone std-only ACP peer, compiled by adapter tests with the active Rust toolchain.
use std::io::{self, BufRead, Write};

fn send(value: &str) {
  println!("{value}");
  io::stdout().flush().unwrap();
}
fn field<'a>(line: &'a str, field: &str) -> &'a str {
  let needle = format!("\"{field}\":");
  let start = line.find(&needle).expect("required fixture field") + needle.len();
  let value = line[start..].trim_start();
  if let Some(value) = value.strip_prefix('"') { &value[..value.find('"').unwrap()] }
  else { &value[..value.find([',', '}']).unwrap_or(value.len())] }
}
fn reply(id: &str, value: &str) { send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{value}}}")); }
fn update(value: &str) { send(&format!("{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"fixture-session\",\"update\":{value}}}}}")); }
fn permission(id: u32, tool: &str) {
  let vendor = if tool == "Bash" { String::new() } else { format!(",\"_meta\":{{\"qoder\":{{\"toolName\":\"{tool}\"}}}}") };
  send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"session/request_permission\",\"params\":{{\"sessionId\":\"fixture-session\",\"toolCall\":{{\"toolCallId\":\"fixture-tool\",\"title\":\"Write fixture\",\"rawInput\":{{\"command\":\"write fixture.txt\"}}{vendor}}},\"options\":[{{\"optionId\":\"native-once\",\"name\":\"Allow once\",\"kind\":\"allow_once\"}},{{\"optionId\":\"native-always\",\"name\":\"Always\",\"kind\":\"allow_always\"}},{{\"optionId\":\"native-deny\",\"name\":\"Deny\",\"kind\":\"reject_once\"}}]}}}}"));
}
fn config(current: &str, grouped: bool) -> String {
  let values = "[{\"value\":\"model-a\",\"name\":\"A\"},{\"value\":\"model-b\",\"name\":\"B\"}]";
  let options = if grouped { format!("[{{\"group\":\"fixture-group\",\"name\":\"Fixture group\",\"options\":{values}}}]") } else { values.into() };
  format!("[{{\"id\":\"opaque-engine-selector\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"{current}\",\"options\":{options}}},{{\"id\":\"future-option\",\"name\":\"Future option\",\"type\":\"future\"}}]")
}

fn main() {
  let scenario = std::env::var("ACP_FIXTURE_SCENARIO").unwrap_or_default();
  let mut prompt = String::new();
  let mut prompt_count = 0;
  for line in io::stdin().lock().lines() {
    let line = line.unwrap();
    if !line.contains("\"method\"") {
      let id = field(&line, "id");
      if scenario == "unknown" {
        assert_eq!(id, "70");
        assert!(line.contains("-32601"));
        reply(&prompt, "{\"stopReason\":\"end_turn\"}");
      } else if scenario == "plan" {
        assert!(line.contains("\"cancelled\""));
        reply(&prompt, "{\"stopReason\":\"end_turn\"}");
      } else if id == "72" || id == "73" || id == "74" {
        // An identical business request with a different envelope must not replace the original.
        assert!(line.contains("\"cancelled\""));
      } else {
        assert_eq!(id, "71");
        if scenario == "cancel" { assert!(line.contains("\"cancelled\"")); }
        else { assert!(line.contains("\"optionId\":\"native-once\"")); }
        if scenario != "cancel" {
          update("{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"fixture-tool\",\"status\":\"completed\"}");
          permission(73, "Bash"); // Retired IDs cannot resurrect.
        }
        reply(&prompt, if scenario == "cancel" { "{\"stopReason\":\"cancelled\"}" } else { "{\"stopReason\":\"end_turn\"}" });
      }
      continue;
    }
    let method = field(&line, "method");
    if let Ok(path) = std::env::var("ACP_FIXTURE_TRACE") {
      let mut trace = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
      writeln!(trace,"{method}").unwrap();
    }
    if method == "session/cancel" {
      assert!(!prompt.is_empty(), "Stop must never precede prompt dispatch");
      if scenario == "cancel" { permission(74, "Bash"); }
      if scenario != "cancel" { reply(&prompt, "{\"stopReason\":\"cancelled\"}"); }
      continue;
    }
    let id = field(&line, "id");
    match method {
      "initialize" => {
        assert!(line.contains("\"terminal\":false"));
        assert!(line.contains("\"readTextFile\":false"));
        assert!(!line.contains("elicitation"));
        reply(id, "{\"protocolVersion\":1,\"agentInfo\":{\"name\":\"fixture\",\"version\":\"1\"},\"agentCapabilities\":{\"loadSession\":true,\"sessionCapabilities\":{\"resume\":{}},\"promptCapabilities\":{\"image\":true}}}");
      }
      "session/new" | "session/resume" => {
        if method == "session/resume" { assert!(line.contains("\"sessionId\":\"fixture-session\"")); }
        let identity = if method == "session/new" { "\"sessionId\":\"fixture-session\"," } else { "" };
        if scenario == "legacy-model" {
          reply(id,&format!("{{{identity}\"models\":{{\"currentModelId\":\"model-a\",\"availableModels\":[{{\"modelId\":\"model-a\",\"name\":\"A\"}},{{\"modelId\":\"model-b\",\"name\":\"B\"}}]}}}}"));
        } else {
          let extra = if scenario == "prefer-standard" { ",\"models\":{\"invalidLegacyValue\":true}" }
            else if scenario == "incomplete-config-result" || scenario == "wrong-config-result" {
              ",\"models\":{\"currentModelId\":\"model-a\",\"availableModels\":[{\"modelId\":\"model-a\",\"name\":\"A\"},{\"modelId\":\"model-b\",\"name\":\"B\"}]}"
            } else { "" };
          reply(id,&format!("{{{identity}\"configOptions\":{}{extra}}}",config("model-a",scenario=="grouped-model")));
        }
        // A delayed lifecycle update cannot prove prompt admission or enter the live overlay.
        update("{\"sessionUpdate\":\"available_commands_update\",\"availableCommands\":[]}");
      }
      "session/set_model" => { assert_eq!(scenario,"legacy-model"); assert!(line.contains("\"modelId\":\"model-b\"")); reply(id,"{}"); }
      "session/set_config_option" => {
        assert_ne!(scenario,"legacy-model");
        assert!(line.contains("\"configId\":\"opaque-engine-selector\""));
        assert!(line.contains("\"value\":\"model-b\""));
        if scenario == "incomplete-config-result" { reply(id,"{}"); }
        else { reply(id,&format!("{{\"configOptions\":{}}}",config(if scenario=="wrong-config-result" { "model-a" } else { "model-b" },scenario=="grouped-model"))); }
      }
      "session/prompt" => {
        prompt_count += 1;
        assert_eq!(prompt_count, 1, "No automatic prompt retry");
        assert!(line.contains("\"prompt\":["));
        prompt = id.into();
        if scenario == "image" { assert!(line.contains("\"type\":\"image\"")); assert!(line.contains("\"mimeType\":\"image/png\"")); }
        if scenario == "lost" { return; }
        if scenario == "auth" { send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32000,\"message\":\"Authentication required\"}}}}")); continue; }
        if scenario == "zero" { reply(id, "{\"stopReason\":\"refusal\"}"); continue; }
        update("{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"fixture output\"}}");
        if scenario == "config-update" {
          update(&format!("{{\"sessionUpdate\":\"config_option_update\",\"configOptions\":{}}}",config("model-b",true)));
        }
        match scenario.as_str() {
          "permission" => { permission(71, "Bash"); permission(72, "Bash"); }
          "cancel" => permission(71, "Bash"),
          "plan" => permission(71, "ExitPlanMode"),
          "unknown" => send("{\"jsonrpc\":\"2.0\",\"id\":70,\"method\":\"fs/read_text_file\",\"params\":{}}"),
          "stop-order" => {},
          "legacy-model" => reply(id,"{\"stopReason\":\"end_turn\",\"userMessageId\":\"legacy-user-message\"}"),
          _ => reply(id, "{\"stopReason\":\"end_turn\"}"),
        }
      }
      _ => panic!("Unexpected native method: {method}"),
    }
  }
}

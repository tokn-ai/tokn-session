use crate::TestResult;
use axum::{
  Json, Router,
  extract::State,
  response::{IntoResponse, Sse, sse::Event},
  routing::{get, post},
};
use serde_json::{Value, json};
use std::{
  collections::VecDeque,
  convert::Infallible,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};
use tokio::{
  sync::{Notify, watch},
  task::JoinHandle,
};

#[derive(Clone, Default)]
pub struct ModelGate(Arc<Gate>);
#[derive(Default)]
struct Gate {
  released: AtomicBool,
  notify: Notify,
}
impl ModelGate {
  pub fn release(&self) {
    self.0.released.store(true, Ordering::Release);
    self.0.notify.notify_waiters();
  }
  async fn wait(&self) {
    loop {
      let notified = self.0.notify.notified();
      tokio::pin!(notified);
      notified.as_mut().enable();
      if self.0.released.load(Ordering::Acquire) {
        return;
      }
      notified.await;
    }
  }
}

pub enum ModelStep {
  Text {
    text: String,
    delay: Duration,
    gate: Option<ModelGate>,
  },
  ToolCall {
    name: String,
    arguments: Value,
  },
  Stall,
}
impl ModelStep {
  pub fn text(text: &str) -> Self {
    Self::delayed_text(text, Duration::ZERO)
  }
  pub fn delayed_text(text: &str, delay: Duration) -> Self {
    Self::Text {
      text: text.into(),
      delay,
      gate: None,
    }
  }
  pub fn tool_call(name: &str, arguments: Value) -> Self {
    Self::ToolCall {
      name: name.into(),
      arguments,
    }
  }
  pub fn stall() -> Self {
    Self::Stall
  }
  pub fn gated_text(text: &str) -> (Self, ModelGate) {
    let gate = ModelGate::default();
    (
      Self::Text {
        text: text.into(),
        delay: Duration::ZERO,
        gate: Some(gate.clone()),
      },
      gate,
    )
  }
}

struct Inner {
  steps: Mutex<VecDeque<ModelStep>>,
  requests: Mutex<Vec<Value>>,
  notify: Notify,
}
pub struct ModelServer {
  url: String,
  inner: Arc<Inner>,
  stop: watch::Sender<bool>,
  task: JoinHandle<()>,
}
impl ModelServer {
  pub async fn start(steps: Vec<ModelStep>) -> TestResult<Self> {
    let inner = Arc::new(Inner {
      steps: Mutex::new(steps.into()),
      requests: Mutex::new(Vec::new()),
      notify: Notify::new(),
    });
    let app = Router::new()
      .route("/v1/chat/completions", post(completion))
      .route("/chat/completions", post(completion))
      .route("/v1/responses", post(responses))
      .route("/responses", post(responses))
      .route(
        "/v1/models",
        get(|| async {
          Json(json!({"object":"list","data":[{"id":"rpc-test-model","object":"model","owned_by":"local"}]}))
        }),
      )
      .with_state(inner.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let (stop, mut receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
      let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
          let _ = receiver.changed().await;
        })
        .await;
    });
    Ok(Self { url, inner, stop, task })
  }
  pub fn url(&self) -> String {
    self.url.clone()
  }
  pub fn requests(&self) -> Vec<Value> {
    self.inner.requests.lock().unwrap().clone()
  }
  pub async fn wait_for_requests(&self, count: usize) -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
      loop {
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.inner.requests.lock().unwrap().len() >= count {
          return;
        }
        notified.await;
      }
    })
    .await
    .map_err(|_| {
      format!(
        "Expected {count} local model requests; received {}",
        self.requests().len()
      )
    })?;
    Ok(())
  }
  pub async fn close(self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}
impl Drop for ModelServer {
  fn drop(&mut self) {
    let _ = self.stop.send(true);
    self.task.abort();
  }
}

async fn completion(State(inner): State<Arc<Inner>>, Json(request): Json<Value>) -> axum::response::Response {
  let request_number = {
    let mut requests = inner.requests.lock().unwrap();
    requests.push(request.clone());
    requests.len()
  };
  inner.notify.notify_waiters();
  let Some(step) = inner.steps.lock().unwrap().pop_front() else {
    return (
      axum::http::StatusCode::INTERNAL_SERVER_ERROR,
      Json(json!({"error":{"message":"Local model script exhausted","type":"test_script_error"}})),
    )
      .into_response();
  };
  let model = request["model"].as_str().unwrap_or("rpc-test-model").to_owned();
  let stream = async_stream::stream! {
    let event = |delta: Value, finish: Value| Event::default().data(json!({"id":"chatcmpl-local-fixture","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string());
    yield Ok::<_, Infallible>(event(json!({"role":"assistant"}), Value::Null));
    match step {
      ModelStep::Text { text, delay, gate } => {
        if let Some(gate) = gate { yield Ok(event(json!({"content":"working "}), Value::Null)); gate.wait().await; }
        tokio::time::sleep(delay).await;
        yield Ok(event(json!({"content":text}), Value::Null));
        yield Ok(event(json!({}), json!("stop")));
      }
      ModelStep::ToolCall { name, arguments } => {
        yield Ok(event(json!({"tool_calls":[{"index":0,"id":format!("call_local_fixture_{request_number}"),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]}), Value::Null));
        yield Ok(event(json!({}), json!("tool_calls")));
      }
      ModelStep::Stall => {
        yield Ok(event(json!({"content":"working"}), Value::Null));
        std::future::pending::<()>().await;
      }
    }
    yield Ok(Event::default().data(json!({"id":"chatcmpl-local-fixture","object":"chat.completion.chunk","created":0,"model":model,"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}).to_string()));
    yield Ok(Event::default().data("[DONE]"));
  };
  Sse::new(stream).into_response()
}

/// Codex uses the Responses wire API. Only inference is scripted: the real
/// app-server still constructs prompts, executes tools and handles RPC input.
async fn responses(State(inner): State<Arc<Inner>>, Json(request): Json<Value>) -> axum::response::Response {
  let request_number = {
    let mut requests = inner.requests.lock().unwrap();
    requests.push(request);
    requests.len()
  };
  inner.notify.notify_waiters();
  let Some(step) = inner.steps.lock().unwrap().pop_front() else {
    return (
      axum::http::StatusCode::INTERNAL_SERVER_ERROR,
      Json(json!({"error":{"message":"Local model script exhausted","type":"test_script_error"}})),
    )
      .into_response();
  };
  let stream = async_stream::stream! {
    let event = |body: Value| Event::default()
      .event(body["type"].as_str().expect("Responses event type"))
      .data(body.to_string());
    let response_id = format!("resp_local_fixture_{request_number}");
    let item_id = format!("msg_local_fixture_{request_number}");
    yield Ok::<_, Infallible>(event(json!({"type":"response.created","response":{"id":response_id}})));
    match step {
      ModelStep::Text { text, delay, gate } => {
        yield Ok(event(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","id":item_id,"content":[]}})));
        let mut output = String::new();
        if let Some(gate) = gate {
          output.push_str("working ");
          yield Ok(event(json!({"type":"response.output_text.delta","item_id":item_id,"output_index":0,"content_index":0,"delta":"working "})));
          gate.wait().await;
        }
        tokio::time::sleep(delay).await;
        output.push_str(&text);
        yield Ok(event(json!({"type":"response.output_text.delta","item_id":item_id,"output_index":0,"content_index":0,"delta":text})));
        yield Ok(event(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","id":item_id,"content":[{"type":"output_text","text":output}]}})));
      }
      ModelStep::ToolCall { name, arguments } => {
        yield Ok(event(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":format!("call_local_fixture_{request_number}"),"name":name,"arguments":arguments.to_string()}})));
      }
      ModelStep::Stall => {
        yield Ok(event(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","id":item_id,"content":[]}})));
        yield Ok(event(json!({"type":"response.output_text.delta","item_id":item_id,"output_index":0,"content_index":0,"delta":"working"})));
        std::future::pending::<()>().await;
      }
    }
    yield Ok(event(json!({"type":"response.completed","response":{"id":response_id,"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}})));
  };
  Sse::new(stream).into_response()
}

#[cfg(test)]
mod tests {
  use super::*;
  use futures_util::StreamExt;

  #[tokio::test]
  async fn responses_stream_waits_for_gate_and_preserves_tool_arguments() -> TestResult {
    let (step, gate) = ModelStep::gated_text("finished");
    let arguments = json!({"questions":[{"id":"choice","question":"Choose","options":[]}]});
    let server = ModelServer::start(vec![
      step,
      ModelStep::tool_call("request_user_input", arguments.clone()),
    ])
    .await?;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let response = client
      .post(format!("{}/responses", server.url()))
      .json(&json!({"model":"fixture-model","input":[{"role":"user","content":"hello"}],"stream":true}))
      .send()
      .await?
      .error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut output = String::new();
    while !output.contains("working ") {
      let bytes = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await?
        .ok_or("Responses stream ended before the gate")??;
      output.push_str(std::str::from_utf8(&bytes)?);
    }
    assert!(!output.contains("response.completed"));
    gate.release();
    while let Some(bytes) = stream.next().await {
      output.push_str(std::str::from_utf8(&bytes?)?);
    }
    assert!(output.contains("working finished"));
    assert!(output.contains("event: response.completed"));

    let output = client
      .post(format!("{}/responses", server.url()))
      .json(&json!({"model":"fixture-model","input":[],"stream":true}))
      .send()
      .await?
      .error_for_status()?
      .text()
      .await?;
    let call: Value = output
      .lines()
      .filter_map(|line| line.strip_prefix("data: "))
      .map(serde_json::from_str::<Value>)
      .collect::<Result<Vec<_>, _>>()?
      .into_iter()
      .find(|value| value["item"]["type"] == "function_call")
      .ok_or("Responses stream omitted its function call")?;
    assert_eq!(call["item"]["name"], "request_user_input");
    assert_eq!(
      serde_json::from_str::<Value>(call["item"]["arguments"].as_str().unwrap())?,
      arguments
    );
    assert_eq!(server.requests().len(), 2);
    Ok(())
  }
}

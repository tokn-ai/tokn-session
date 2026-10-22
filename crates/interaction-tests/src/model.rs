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

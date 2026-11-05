//! One outstanding payload chunk per channel. A slow/detached UI cannot cause
//! the native event broadcast to discard encrypted response bytes silently.
use crate::host;
use serde_json::{Value, json};
use std::{
  cell::{Cell, RefCell},
  collections::BTreeMap,
  rc::Rc,
  task::{Poll, Waker},
};
use tokn_hub_client_core::protocol;

#[derive(Default)]
struct Pending {
  sequence: u64,
  acknowledged: bool,
  cancelled: bool,
  deadline: u64,
  transfer_deadline: u64,
  waker: Option<Waker>,
  cancel_waker: Option<Waker>,
}
thread_local! {
  static CHANNELS: RefCell<BTreeMap<String,Rc<RefCell<Pending>>>> = RefCell::new(BTreeMap::new());
  static MONITOR:Cell<bool>=const{Cell::new(false)};
}

pub struct Channel {
  id: String,
  next: u64,
  pending: Rc<RefCell<Pending>>,
}
impl Channel {
  pub fn new(id: String) -> Result<Self, String> {
    let pending = Rc::new(RefCell::new(Pending {
      transfer_deadline: host::unix_time()?.saturating_add(120),
      ..Pending::default()
    }));
    CHANNELS.with(|channels| {
      let mut channels = channels.borrow_mut();
      if channels.len() >= 16 || channels.contains_key(&id) {
        return Err("Too many active applet transfers".to_owned());
      }
      channels.insert(id.clone(), pending.clone());
      Ok(())
    })?;
    if let Err(error) = monitor() {
      CHANNELS.with(|channels| {
        channels.borrow_mut().remove(&id);
      });
      return Err(error);
    }
    Ok(Self { id, next: 0, pending })
  }
  pub fn cancellation(&self) -> impl std::future::Future<Output = ()> + 'static {
    let pending = self.pending.clone();
    std::future::poll_fn(move |cx| {
      let mut pending = pending.borrow_mut();
      if pending.cancelled {
        Poll::Ready(())
      } else {
        pending.cancel_waker = Some(cx.waker().clone());
        Poll::Pending
      }
    })
  }
  pub fn live(&self) {
    self.pending.borrow_mut().transfer_deadline = u64::MAX;
  }
  pub fn timeout(&self, seconds: u64) -> Result<(), String> {
    self.pending.borrow_mut().transfer_deadline = host::unix_time()?.saturating_add(seconds);
    Ok(())
  }
  pub async fn chunk(&mut self, event: &str, mut payload: Value, bytes: &[u8]) -> Result<(), String> {
    let sequence = self.next;
    self.next = self.next.checked_add(1).ok_or("Applet transfer sequence exhausted")?;
    {
      let mut pending = self.pending.borrow_mut();
      if pending.cancelled {
        return Err("Applet transfer was detached".into());
      }
      pending.sequence = sequence;
      pending.acknowledged = false;
      pending.deadline = host::unix_time()?.saturating_add(30);
    }
    payload["channel_id"] = json!(self.id);
    payload["sequence"] = json!(sequence);
    payload["data"] = json!(protocol::encode(bytes));
    apell_guest::emit(json!({"name":event,"payload":payload})).map_err(|error| error.message)?;
    let pending = self.pending.clone();
    std::future::poll_fn(move |cx| {
      let mut state = pending.borrow_mut();
      if state.cancelled {
        Poll::Ready(Err("Applet transfer was detached".to_owned()))
      } else if state.acknowledged {
        Poll::Ready(Ok(()))
      } else {
        state.waker = Some(cx.waker().clone());
        Poll::Pending
      }
    })
    .await
  }
}
fn monitor() -> Result<(), String> {
  if MONITOR.with(|monitor| monitor.replace(true)) {
    return Ok(());
  }
  if let Err(error) = apell_guest::spawn(async {
    loop {
      if CHANNELS.with(|channels| channels.borrow().is_empty()) {
        break;
      }
      let slept = host::delay(1000).await;
      let now = host::unix_time().unwrap_or(u64::MAX);
      CHANNELS.with(|channels| {
        for pending in channels.borrow().values() {
          let mut pending = pending.borrow_mut();
          if slept.is_err()
            || now >= pending.transfer_deadline
            || !pending.acknowledged && pending.deadline != 0 && now >= pending.deadline
          {
            pending.cancelled = true;
            if let Some(waker) = pending.waker.take() {
              waker.wake();
            }
            if let Some(waker) = pending.cancel_waker.take() {
              waker.wake();
            }
          }
        }
      });
    }
    MONITOR.with(|monitor| monitor.set(false));
  }) {
    MONITOR.with(|monitor| monitor.set(false));
    return Err(error.message);
  }
  Ok(())
}
pub fn cancel(id: &str) {
  if let Some(pending) = CHANNELS.with(|channels| channels.borrow().get(id).cloned()) {
    let mut pending = pending.borrow_mut();
    pending.cancelled = true;
    if let Some(waker) = pending.waker.take() {
      waker.wake();
    }
    if let Some(waker) = pending.cancel_waker.take() {
      waker.wake();
    }
  }
}
impl Drop for Channel {
  fn drop(&mut self) {
    CHANNELS.with(|channels| {
      channels.borrow_mut().remove(&self.id);
    });
  }
}

pub fn acknowledge(params: Value) -> Result<Value, String> {
  let id = host::string(&params, "channel_id")?;
  let sequence = params
    .get("sequence")
    .and_then(Value::as_u64)
    .ok_or("Invalid applet acknowledgement")?;
  let pending = CHANNELS
    .with(|channels| channels.borrow().get(id).cloned())
    .ok_or("Applet transfer is closed")?;
  let mut pending = pending.borrow_mut();
  if pending.sequence != sequence {
    return Err("Applet acknowledgement is out of order".into());
  }
  pending.acknowledged = true;
  if let Some(waker) = pending.waker.take() {
    waker.wake();
  }
  Ok(Value::Null)
}

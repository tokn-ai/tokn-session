//! Use GTK directly: rfd's GTK message dialogs do not honor a parent and pass
//! secondary text through printf. Property setters keep this review plain text.
use super::*;
use gtk::{glib, prelude::*};
use std::{cell::RefCell, rc::Rc};

pub(super) fn show(
  app: &AppHandle,
  parent: WebviewWindow,
  body: String,
  approval: Approval,
  slot: ReviewSlots,
  reply: oneshot::Sender<Decision>,
) {
  let _ = app.run_on_main_thread(move || {
    if !still_available(&parent, &approval) || parent.is_focused().ok() != Some(true) {
      let _ = reply.send(Decision::Decline);
      return;
    }
    let Ok(owner) = parent.gtk_window() else {
      let _ = reply.send(Decision::Decline);
      return;
    };
    let dialog = gtk::MessageDialog::builder()
      .transient_for(&owner)
      .modal(true)
      .destroy_with_parent(true)
      .message_type(gtk::MessageType::Warning)
      .text("Codex command approval")
      .secondary_text(&body)
      .secondary_use_markup(false)
      .use_markup(false)
      .build();
    dialog.add_button(DECLINE, gtk::ResponseType::Reject);
    dialog.add_button(ALLOW, gtk::ResponseType::Accept);
    dialog.add_button(CANCEL, gtk::ResponseType::Cancel);
    dialog.set_default_response(gtk::ResponseType::Reject);
    let completion = Rc::new(RefCell::new(Some((reply, slot))));
    let responded = completion.clone();
    let mut cancellation = approval.cancellation.clone();
    let (closed, mut closing) = tokio::sync::watch::channel(false);
    let closed_response = closed.clone();
    dialog.connect_response(move |dialog, response| {
      let decision = if still_available(&parent, &approval) {
        gtk_result(response)
      } else {
        Decision::Decline
      };
      if let Some((reply, _slot)) = responded.borrow_mut().take() {
        let _ = reply.send(decision);
      }
      closed_response.send_replace(true);
      // Destruction is final; no code reads widget state after this point.
      unsafe {
        dialog.destroy();
      }
    });
    let destroyed = completion.clone();
    dialog.connect_destroy(move |_| {
      if let Some((reply, _slot)) = destroyed.borrow_mut().take() {
        let _ = reply.send(Decision::Decline);
      }
      closed.send_replace(true);
    });
    let weak = dialog.downgrade();
    glib::MainContext::default().spawn_local(async move {
      tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
          if !*closing.borrow() {
            if let Some(dialog) = weak.upgrade() { dialog.response(gtk::ResponseType::Reject); }
          }
        },
        _ = async { while !*closing.borrow() { if closing.changed().await.is_err() { break; } } } => {},
      }
    });
    dialog.show();
  });
}

fn gtk_result(response: gtk::ResponseType) -> Decision {
  match response {
    gtk::ResponseType::Accept => Decision::AllowOnce,
    gtk::ResponseType::Cancel | gtk::ResponseType::DeleteEvent => Decision::Cancel,
    _ => Decision::Decline,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_explicit_gtk_accept_allows_once() {
    assert_eq!(gtk_result(gtk::ResponseType::Accept), Decision::AllowOnce);
    assert_eq!(gtk_result(gtk::ResponseType::Cancel), Decision::Cancel);
    assert_eq!(gtk_result(gtk::ResponseType::DeleteEvent), Decision::Cancel);
    for response in [
      gtk::ResponseType::None,
      gtk::ResponseType::Reject,
      gtk::ResponseType::Yes,
      gtk::ResponseType::No,
      gtk::ResponseType::Ok,
      gtk::ResponseType::Other(42),
    ] {
      assert_eq!(gtk_result(response), Decision::Decline);
    }
  }
}

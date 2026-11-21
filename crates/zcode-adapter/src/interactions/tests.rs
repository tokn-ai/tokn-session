use super::*;

fn permission(id: &str) -> Value {
  json!({
    "requestId":id, "sessionId":"sess_fixture", "turnId":"turn_fixture",
    "toolCallId":"call_fixture", "toolName":"Bash", "riskLevel":"medium",
    "reason":"Approve this command", "input":{"command":"cargo check"},
    "options":[
      {"kind":"allow_once","response":{"decision":"allow","reason":"Approved once"}},
      {"kind":"allow_always","response":{"decision":"allow","permissionUpdates":[{"type":"addRules"}]}},
      {"kind":"deny","response":{"decision":"deny","reason":"Declined"}}
    ]
  })
}

fn questions(id: &str) -> Value {
  json!({
    "requestId":id, "sessionId":"sess_fixture", "turnId":"turn_fixture",
    "toolCallId":"call_question", "toolName":"AskUserQuestion",
    "input":{"questions":[
      {"question":"Which features?", "header":"Features","multiSelect":true,"options":[
        {"label":"A","description":"First"},{"label":"B","description":"Second"}
      ]},
      {"question":"Which name?","header":"Name","options":[{"label":"One"},{"label":"Two"}]}
    ]},
    "questions":[
      {"question":"Which features?","header":"Features","multiSelect":true,"options":[
        {"label":"A","value":"A","description":"First"},{"label":"B","value":"B","description":"Second"}
      ]},
      {"question":"Which name?","header":"Name","options":[{"label":"One","value":"One"},{"label":"Two","value":"Two"}]}
    ]
  })
}

fn register(registry: &mut Interactions, envelope: &str, method: &str, params: &Value) -> Registration {
  registry.register(
    json!(envelope),
    method,
    params,
    Callback::parse(method, params).unwrap(),
  )
}

#[test]
fn reannouncement_updates_only_correlation_and_never_replays_a_dispatched_answer() {
  let mut registry = Interactions::default();
  let params = permission("perm_first");
  assert!(matches!(
    register(&mut registry, "server-1", "interaction/requestPermission", &params),
    Registration::New(_)
  ));
  assert!(matches!(
    register(&mut registry, "server-2", "interaction/requestPermission", &params),
    Registration::Reannounced
  ));
  assert_eq!(registry.list("sess_fixture").len(), 1);
  let (id, response) = registry
    .prepare_response(
      "sess_fixture",
      "perm_first",
      InteractionResponse::Permission {
        decision: PermissionDecision::AllowOnce,
      },
    )
    .unwrap();
  assert_eq!(id, "server-2");
  assert_eq!(response["decision"], "allow");
  assert!(matches!(
    register(&mut registry, "server-3", "interaction/requestPermission", &params),
    Registration::Reannounced
  ));
  assert_eq!(registry.list("sess_fixture")[0].native["response_pending"], true);
  assert_eq!(
    registry
      .prepare_response("sess_fixture", "perm_first", InteractionResponse::Reject)
      .unwrap_err()
      .kind,
    ErrorKind::Conflict
  );
  registry.retire("sess_fixture", Some("perm_first"), false);
  assert!(registry.list("sess_fixture").is_empty());
  assert!(matches!(
    register(&mut registry, "server-4", "interaction/requestPermission", &params),
    Registration::Declined
  ));
}

#[test]
fn native_expiry_retires_confirmed_requests_without_racing_initial_registration_or_reannouncement() {
  let mut registry = Interactions::default();
  let params = questions("perm_question");
  register(&mut registry, "server-1", "interaction/requestUserInput", &params);
  let cutoff = registry.revision;
  // The callback may precede its native permission-requested event.
  assert!(registry.reconcile("sess_fixture", &[], cutoff).is_empty());
  registry.observe_pending("sess_fixture", "perm_question");
  register(&mut registry, "server-2", "interaction/requestUserInput", &params);
  assert!(registry.reconcile("sess_fixture", &[], cutoff).is_empty());
  let retired = registry.reconcile("sess_fixture", &[], registry.revision);
  assert_eq!(retired, vec![("sess_fixture".into(), "perm_question".into())]);
  assert!(registry.list("sess_fixture").is_empty());
  assert!(matches!(
    register(&mut registry, "server-late", "interaction/requestUserInput", &params),
    Registration::Declined
  ));
}

#[test]
fn option_previews_remain_visible_beside_their_descriptions() {
  let mut params = questions("perm_preview");
  params["questions"][0]["options"][0]["preview"] = json!("A preview of the first implementation");
  params["input"]["questions"][0]["options"][0]["preview"] = json!("A preview of the first implementation");
  let callback = Callback::parse("interaction/requestUserInput", &params).unwrap();
  assert_eq!(
    callback.view.questions[0].options[0].description.as_deref(),
    Some("First\n\nA preview of the first implementation")
  );
}

#[test]
fn answers_preserve_multiple_choices_and_custom_text_as_a_complete_batch() {
  let callback = Callback::parse("interaction/requestUserInput", &questions("perm_question")).unwrap();
  assert!(callback.view.questions[0].multiple);
  let response = callback
    .respond(InteractionResponse::Answers {
      answers: vec![
        QuestionAnswer {
          question_id: "0".into(),
          answers: vec!["A".into(), "B".into()],
          custom: Some("Keep both local".into()),
        },
        QuestionAnswer {
          question_id: "1".into(),
          answers: Vec::new(),
          custom: Some("A custom name".into()),
        },
      ],
    })
    .unwrap();
  assert_eq!(
    response,
    json!({"action":"accept","content":{"answers":{
      "Which features?":"A, B","Which name?":"A custom name"
    },"annotations":{"Which features?":{"notes":"Keep both local"}}}})
  );
  assert!(
    callback
      .respond(InteractionResponse::Answers {
        answers: vec![QuestionAnswer {
          question_id: "0".into(),
          answers: vec!["unknown".into()],
          custom: None
        }]
      })
      .is_err()
  );
}

#[test]
fn per_use_approvals_do_not_apply_native_persistent_rules_or_authorize_another_session() {
  let params = permission("perm_first");
  let callback = Callback::parse("interaction/requestPermission", &params).unwrap();
  assert_eq!(
    callback.view.native["permission_decisions"],
    json!(["allow_once", "deny"])
  );
  assert!(
    callback.view.native["details"]
      .as_str()
      .unwrap()
      .contains("cargo check")
  );
  assert!(
    callback
      .respond(InteractionResponse::Permission {
        decision: PermissionDecision::AllowAlways
      })
      .is_err()
  );
  let mut registry = Interactions::default();
  register(&mut registry, "server-1", "interaction/requestPermission", &params);
  assert_eq!(
    registry
      .prepare_response("sess_other", "perm_first", InteractionResponse::Reject)
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  let (_, denied) = registry
    .prepare_response("sess_fixture", "perm_first", InteractionResponse::Reject)
    .unwrap();
  assert_eq!(denied["decision"], "deny");
}

#[test]
fn changed_reannouncements_and_mismatched_displayed_questions_are_rejected() {
  let mut registry = Interactions::default();
  let mut params = permission("perm_first");
  register(&mut registry, "server-1", "interaction/requestPermission", &params);
  params["input"]["command"] = json!("cargo test");
  assert!(matches!(
    register(&mut registry, "server-2", "interaction/requestPermission", &params),
    Registration::Declined
  ));
  let mut params = questions("perm_question");
  params["questions"][0]["options"][0]["value"] = json!("a different choice");
  assert!(Callback::parse("interaction/requestUserInput", &params).is_none());
  let mut params = questions("perm_question");
  params["input"]["questions"][0]["question"] = json!("A different question?");
  assert!(Callback::parse("interaction/requestUserInput", &params).is_none());
}

#[test]
fn plan_approval_maps_display_labels_to_the_native_token_and_preserves_feedback() {
  let params = json!({
    "requestId":"perm_plan","sessionId":"sess_fixture","toolName":"ExitPlanMode",
    "schema":{"interaction":"plan_approval","toolName":"ExitPlanMode"},
    "input":{"plan":"Implement the feature and run tests.","allowedPrompts":[{"tool":"Bash","prompt":"run tests"}]},
    "questions":[{"question":"Review this implementation plan.","header":"Plan","options":[{"label":"Approve","value":"approve"}]}]
  });
  let callback = Callback::parse("interaction/requestUserInput", &params).unwrap();
  assert!(
    callback.view.native["details"]
      .as_str()
      .unwrap()
      .contains("Implement the feature")
  );
  let approve = callback
    .respond(InteractionResponse::Answers {
      answers: vec![QuestionAnswer {
        question_id: "0".into(),
        answers: vec!["Approve".into()],
        custom: None,
      }],
    })
    .unwrap();
  assert_eq!(approve, json!({"action":"accept","content":{"answer":"approve"}}));
  let feedback = callback
    .respond(InteractionResponse::Answers {
      answers: vec![QuestionAnswer {
        question_id: "0".into(),
        answers: Vec::new(),
        custom: Some("Add a migration check.".into()),
      }],
    })
    .unwrap();
  assert_eq!(feedback["content"]["answer"], "Add a migration check.");
}

#[test]
fn root_completion_preserves_live_subagent_requests_and_prewrite_failures_can_be_released() {
  let mut registry = Interactions::default();
  let root = permission("perm_root");
  let mut child = permission("perm_child");
  child["origin"] = json!({"kind":"subagent","parentSessionId":"sess_fixture","childSessionId":"sess_child"});
  register(&mut registry, "server-1", "interaction/requestPermission", &root);
  register(&mut registry, "server-2", "interaction/requestPermission", &child);
  assert_eq!(
    registry.retire("sess_fixture", None, true),
    vec![("sess_fixture".into(), "perm_root".into())]
  );
  registry
    .prepare_response("sess_fixture", "perm_child", InteractionResponse::Reject)
    .unwrap();
  registry.release_response("sess_fixture", "perm_child");
  assert_eq!(registry.list("sess_fixture")[0].native["response_pending"], false);
  assert_eq!(registry.retire("sess_fixture", None, false).len(), 1);
}

#[test]
fn retained_inputs_and_display_details_are_bounded() {
  let mut params = permission("perm_first");
  params["input"] = json!({"content":"中".repeat(MAX_DETAILS_BYTES)});
  let callback = Callback::parse("interaction/requestPermission", &params).unwrap();
  let details = callback.view.native["details"].as_str().unwrap();
  assert!(details.len() <= MAX_DETAILS_BYTES);
  assert!(details.ends_with("[truncated]"));
  params["input"] = json!({"content":"x".repeat(MAX_CALLBACK_BYTES)});
  assert!(Callback::parse("interaction/requestPermission", &params).is_none());
}

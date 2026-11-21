use super::*;

fn image(bytes: &[u8]) -> ImageInput {
  ImageInput {
    mime_type: "image/png".into(),
    data: STANDARD.encode(bytes),
  }
}

#[test]
fn validates_complete_image_input_before_staging_and_fences_artifact_owners() {
  assert!(prepare(vec![image(b"fixture")]).is_ok());
  for input in [
    vec![image(b"")],
    vec![image(b"x"); MAX_IMAGES + 1],
    vec![ImageInput {
      mime_type: "image/png".into(),
      data: "not base64".into(),
    }],
    vec![ImageInput {
      mime_type: "image/svg+xml".into(),
      data: STANDARD.encode(b"<svg/>"),
    }],
    vec![ImageInput {
      mime_type: "image/png".into(),
      data: "A".repeat(4 * MAX_IMAGE_BYTES.div_ceil(3) + 4),
    }],
  ] {
    assert_eq!(prepare(input).err().unwrap().kind, ErrorKind::InvalidRequest);
  }
  assert!(valid_artifact_ref(
    "sess_fixture",
    "zcode-artifact://sess_fixture/tool-result-1"
  ));
  for value in [
    "zcode-artifact://sess_other/tool-result-1",
    "zcode-artifact://sess_fixture/",
    "zcode-artifact://sess_fixture/..",
    "zcode-artifact://sess_fixture/../../other",
    "file:///fixture/image.png",
    "https://example.invalid/image.png",
  ] {
    assert!(!valid_artifact_ref("sess_fixture", value));
  }
}

#[cfg(unix)]
async fn fixture(commit_mode: &str) -> ZCodeAdapter {
  let directory = std::env::current_dir().unwrap();
  let history_path = directory.join("images-fixture.sqlite");
  let snapshot = json!({
    "protocol":{"name":"ZCode Protocol","version":1},
    "session":{
      "sessionId":"sess_fixture","sessionKind":"interactive",
      "workspace":{"workspaceKey":directory,"workspacePath":directory}
    },
    "projection":{"sessionId":"sess_fixture","status":"idle","turnCount":1},
    "runtime":{"stateRevision":7},
    "settings":{"model":{
      "current":{"providerId":"fixture","modelId":"fixture-a","options":{"reasoningLevel":"low"}},
      "available":[
        {"ref":{"providerId":"fixture","modelId":"fixture-a"},"label":"Model A",
          "properties":{"inputFormat":{"supportsImage":true}},
          "reasoning":{"levels":[{"value":"low"},{"value":"high"}],"defaultLevel":"high"}},
        {"ref":{"providerId":"fixture","modelId":"fixture-b"},"label":"Model B",
          "properties":{"inputFormat":{"supportsImage":true}},
          "reasoning":{"levels":[{"value":"high"}],"defaultLevel":"high"}},
        {"ref":{"providerId":"fixture","modelId":"text-only"},"label":"Text only",
          "properties":{"inputFormat":{"supportsImage":false}}}
      ]
    }}
  });
  let mut current_snapshot = snapshot.clone();
  current_snapshot["settings"]["model"]["available"] = json!([snapshot["settings"]["model"]["available"][0]]);
  let script = r#"
    [ "$0" = app-server ] && [ "$1" = --stdio ] || exit 2
    printf '{"method":"startup/storageState","params":{"databaseKind":"session","phase":"ready","databaseId":"%s"}}\n' "$FIXTURE_DATABASE_ID"
    IFS= read -r request || exit 3
    printf '%s\n' '{"id":1,"result":{"sessionLifecycleWorkspace":true,"sessionWorkspaceRouting":true}}'
    chunks=0
    while IFS= read -r request; do
      id=${request#*'"id":'}
      id=${id%%,*}
      case "$request" in
        *'"method":"session/create"'*)
          case "$request" in
            *'"model":'*)
              case "$request" in *'"model":{"modelId":"fixture-b","options":{"reasoningLevel":"high"},"providerId":"fixture"}'*) ;; *) exit 15 ;; esac
              case "$request" in *'"thoughtLevel":"high"'*) ;; *) exit 16 ;; esac ;;
          esac
          session_id=${request#*'"sessionId":"'}
          session_id=${session_id%%\"*}
          printf '{"id":%s,"result":%s}\n' "$id" "$FIXTURE_FULL_SNAPSHOT" | sed "s/sess_fixture/$session_id/g" ;;
        *'"method":"session/subscribe"'*)
          session_id=${request#*'"sessionId":"'}
          session_id=${session_id%%\"*}
          printf '{"id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$session_id" ;;
        *'"method":"session/read"'*)
          printf '{"id":%s,"result":%s}\n' "$id" "$FIXTURE_SNAPSHOT" ;;
        *'"method":"v4/attachment/begin"'*)
          case "$request" in *'"workspace":'*) exit 4 ;; esac
          case "$request" in *'"totalBytes":524289'*'"totalChunks":2'*) ;; *) exit 5 ;; esac
          upload_id=${request#*'"uploadId":"'}
          upload_id=${upload_id%%\"*}
          printf '{"id":%s,"result":{"uploadId":"%s","state":"staging","nextChunkIndex":0}}\n' "$id" "$upload_id" ;;
        *'"method":"v4/attachment/chunk"'*)
          case "$request" in *'"chunkIndex":'"$chunks"*) ;; *) exit 6 ;; esac
          chunks=$((chunks + 1))
          printf '{"id":%s,"result":{"uploadId":"%s","nextChunkIndex":%s}}\n' "$id" "$upload_id" "$chunks" ;;
        *'"method":"v4/attachment/commit"'*)
          [ "$chunks" = 2 ] || exit 7
          if [ "$FIXTURE_COMMIT_MODE" = disconnect ]; then exit 0; fi
          owner=sess_fixture
          if [ "$FIXTURE_COMMIT_MODE" = wrong-owner ]; then owner=sess_other; fi
          printf '{"id":%s,"result":{"ref":"zcode-artifact://%s/tool-result-fixture"}}\n' "$id" "$owner" ;;
        *'"method":"v4/attachment/abort"'*)
          printf '{"id":%s,"result":{}}\n' "$id" ;;
        *'"method":"v4/command"'*'"commandId":"image-1"'*)
          [ "$FIXTURE_COMMIT_MODE" = success ] || exit 8
          case "$request" in *'"text":""'*) ;; *) exit 9 ;; esac
          case "$request" in *'"ref":"zcode-artifact://sess_fixture/tool-result-fixture"'*) ;; *) exit 10 ;; esac
          case "$request" in *'"modelSelection":{"modelId":"fixture-a","options":{"reasoningLevel":"low"},"providerId":"fixture"}'*) ;; *) exit 11 ;; esac
          case "$request" in *'"modelSelectionBasis":'*) exit 12 ;; esac
          printf '{"id":%s,"result":{"commandId":"image-1","status":"accepted","revisionAtDecision":8,"result":{"type":"inputAccepted","delivery":"queue","inputId":"image-1"}}}\n' "$id" ;;
        *'"method":"v4/command"'*'"commandId":"model-b"'*)
          case "$request" in *'"modelSelection":{"modelId":"fixture-b","options":{"reasoningLevel":"high"},"providerId":"fixture"}'*) ;; *) exit 13 ;; esac
          printf '{"id":%s,"result":{"commandId":"model-b","status":"accepted","revisionAtDecision":9,"result":{"type":"inputAccepted","delivery":"queue","inputId":"model-b"}}}\n' "$id" ;;
        *) exit 14 ;;
      esac
    done
  "#;
  ZCodeAdapter::launch(LaunchOptions {
    runtime_id: "fixture-runtime".into(),
    executable: "/bin/sh".into(),
    arguments: vec!["-c".into(), script.into()],
    directory,
    history_path: history_path.clone(),
    environment: BTreeMap::from([
      ("FIXTURE_DATABASE_ID".into(), database_hash(&history_path).unwrap()),
      ("FIXTURE_SNAPSHOT".into(), current_snapshot.to_string()),
      ("FIXTURE_FULL_SNAPSHOT".into(), snapshot.to_string()),
      ("FIXTURE_COMMIT_MODE".into(), commit_mode.into()),
    ]),
  })
  .await
  .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn discovers_resident_models_without_activation_and_stages_image_only_input_with_pinned_model() {
  let adapter = fixture("success").await;
  let target = SessionTarget {
    runtime_id: adapter.descriptor().runtime_id,
    instance_id: adapter.descriptor().instance_id,
    session_id: None,
  };
  let catalog = || InteractionRequest {
    request_id: "catalog-1".into(),
    target: target.clone(),
    command: Command::Models,
  };
  assert_eq!(
    adapter.execute(catalog()).await.unwrap_err().kind,
    ErrorKind::Unsupported
  );
  adapter.inner.sessions.lock().unwrap().insert("sess_fixture".into());
  let Reply::Models(cold_models) = adapter.execute(catalog()).await.unwrap() else {
    panic!("expected current-only cold catalog");
  };
  assert_eq!(cold_models.len(), 1);
  adapter.inner.sessions.lock().unwrap().remove("sess_fixture");
  adapter.create_session_with_id("sess_fixture", None).await.unwrap();
  let Reply::Models(models) = adapter.execute(catalog()).await.unwrap() else {
    panic!("expected resident catalog");
  };
  assert!(models[0].supports_images);
  assert_eq!(models.len(), 3);
  assert!(!models[2].supports_images);
  adapter
    .create_session_with_id(
      "sess_selected",
      Some(ModelIdentity {
        model_provider: Some("fixture".into()),
        model_id: "fixture-b".into(),
        display_name: None,
      }),
    )
    .await
    .unwrap();
  let mut unsupported = MessageInput {
    images: vec![image(b"fixture")],
    ..MessageInput::default()
  };
  unsupported.model = Some(ModelIdentity {
    model_provider: Some("fixture".into()),
    model_id: "text-only".into(),
    display_name: None,
  });
  assert_eq!(
    adapter
      .inner
      .message("sess_fixture", "unsupported-image", unsupported, DeliveryMode::Guide)
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Rejected
  );
  let input = MessageInput {
    images: vec![image(&vec![7; CHUNK_BYTES + 1])],
    ..MessageInput::default()
  };
  let Reply::Admission(receipt) = adapter
    .inner
    .message("sess_fixture", "image-1", input, DeliveryMode::Guide)
    .await
    .unwrap()
  else {
    panic!("expected image admission");
  };
  assert_eq!(receipt.disposition, InputDisposition::Accepted);
  assert_eq!(receipt.model.unwrap().model_id, "fixture-a");
  let input = MessageInput {
    text: "Use Model B for this follow-up".into(),
    model: Some(ModelIdentity {
      model_provider: Some("fixture".into()),
      model_id: "fixture-b".into(),
      display_name: None,
    }),
    ..MessageInput::default()
  };
  let Reply::Admission(receipt) = adapter
    .inner
    .message("sess_fixture", "model-b", input, DeliveryMode::Queue)
    .await
    .unwrap()
  else {
    panic!("expected model admission");
  };
  assert_eq!(receipt.disposition, InputDisposition::Queued);
  assert_eq!(receipt.model.unwrap().model_id, "fixture-b");
  assert!(!receipt.model_ignored);
  adapter.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn captured_catalog_survives_model_choices_but_retires_changed_capabilities_and_source_lifetime() {
  let adapter = fixture("success").await;
  adapter.create_session_with_id("sess_fixture", None).await.unwrap();
  let mut snapshot = adapter.inner.snapshot("sess_fixture").await.unwrap();
  assert_eq!(snapshot.settings.model.available.len(), 3);
  let mut next = snapshot.settings.model.available.remove(1);
  // Guide/Queue changes the active choice and its reasoning setting. The
  // complete catalog survives those ordinary per-input selections.
  next.selection.options = Some(wire::ModelOptions {
    reasoning_level: Some("high".into()),
  });
  snapshot.settings.model.current = Some(next.selection.clone());
  snapshot.settings.model.available = vec![next.clone()];
  adapter.inner.merge_model_catalog(&mut snapshot);
  assert_eq!(snapshot.settings.model.available.len(), 3);
  next.properties.as_mut().unwrap().input_format.supports_image = false;
  snapshot.settings.model.available = vec![next];
  adapter.inner.merge_model_catalog(&mut snapshot);
  assert_eq!(snapshot.settings.model.available.len(), 1);
  assert!(adapter.inner.model_catalog.lock().unwrap().is_none());

  adapter.create_session_with_id("sess_fixture", None).await.unwrap();
  adapter
    .inner
    .receive(json!({"method":"state.updated","params":{
      "sessionId":"sess_fixture","reason":"thought_level_changed"
    }}))
    .await
    .unwrap();
  assert!(adapter.inner.model_catalog.lock().unwrap().is_none());
  adapter.create_session_with_id("sess_fixture", None).await.unwrap();
  adapter
    .inner
    .receive(json!({"method":"session/event","params":{
      "sessionId":"sess_fixture","type":"session.closed"
    }}))
    .await
    .unwrap();
  assert!(adapter.inner.model_catalog.lock().unwrap().is_none());
  adapter.create_session_with_id("sess_fixture", None).await.unwrap();
  adapter.close().await.unwrap();
  assert!(adapter.inner.model_catalog.lock().unwrap().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn attachment_failure_and_uncertain_commit_never_claim_prompt_admission() {
  for mode in ["wrong-owner", "disconnect"] {
    let adapter = fixture(mode).await;
    adapter.inner.sessions.lock().unwrap().insert("sess_fixture".into());
    let input = MessageInput {
      images: vec![image(&vec![7; CHUNK_BYTES + 1])],
      ..MessageInput::default()
    };
    let error = adapter
      .inner
      .message("sess_fixture", "image-1", input, DeliveryMode::Guide)
      .await
      .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rejected);
    assert!(error.message.contains("not sent"));
    adapter.close().await.unwrap();
  }
}

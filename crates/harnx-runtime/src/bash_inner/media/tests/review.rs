use super::*;

#[tokio::test]
async fn text_data_uri_literals_keep_their_shape_without_storage() {
    for uri in [
        "data:text/plain,hi",
        "data:text/plain;base64,aGk=",
        "data:text/plain;base64,INVALID!",
    ] {
        let original = json!({"content": [{"type": "text", "text": uri}]});
        let mut value = original.clone();
        InnerMedia::new(None).prepare(&mut value).await.unwrap();
        assert_eq!(value, original);

        let (provider, _ctx) = harness(original, None, None, false);
        for option in ["", "-o text", "-o json"] {
            let result = run(&provider, &format!("screenshot {option}")).await;
            assert_eq!(result["exit_code"], 0, "{result}");
            assert!(result["stdout"].as_str().unwrap().contains(uri), "{result}");
        }
    }
}

#[tokio::test]
async fn non_base64_data_strings_and_resource_uris_keep_their_shape() {
    for original in [
        json!("data:text/plain,hi"),
        json!("data:business-value"),
        json!({"nested": ["data:text/plain,hi"]}),
        json!({"content": [{"type": "resource_link", "uri": "data:text/plain,hi", "mimeType": "text/plain"}]}),
        json!({"content": [{"type": "resource", "resource": {"uri": "data:text/plain,hi"}}]}),
    ] {
        let mut value = original.clone();
        InnerMedia::new(None).prepare(&mut value).await.unwrap();
        assert_eq!(value, original);

        let (provider, _ctx) = harness(original.clone(), None, None, false);
        let result = run(&provider, "screenshot -o json").await;
        assert_eq!(result["exit_code"], 0, "{result}");
        // Rendering projects resource blocks as markers independently of media storage.
        if original.get("content").is_none() {
            let output: Value = serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
            assert_eq!(output, original);
        }
    }
}

#[tokio::test]
async fn typed_json_mirrors_without_data_uri_strings_are_externalized() {
    let mirror = json!({"nested": [image()]}).to_string();
    assert!(!mirror.contains("data:"));
    for mut value in [
        json!(mirror),
        json!({"content": [{"type": "text", "text": mirror}]}),
    ] {
        let store = Arc::new(MemoryStore::default());
        InnerMedia::new(Some(store.clone()))
            .prepare(&mut value)
            .await
            .unwrap();
        assert!(!value.to_string().contains(PNG));
        assert!(value.to_string().contains("/attachments/cid:media:"));
        assert_eq!(store.0.lock().unwrap().len(), 1);
    }
}

fn invalid_media_items() -> Vec<Value> {
    vec![
        json!({"type": "image", "data": PNG}),
        json!({"type": "audio", "mimeType": "audio/wav", "data": "INVALID!"}),
        json!({"type": "resource", "resource": {"blob": 4}}),
        json!({"mimeType": "image/png", "data": "INVALID!"}),
        json!({"mimeType": "application/octet-stream", "blob": "INVALID!"}),
        json!({"type": "resource_link", "uri": "cid:media:bad"}),
        json!({"type": "resource", "resource": {"uri": "cid:media:bad"}}),
        json!("data:image/png;base64,INVALID!"),
        json!(
            json!({"content": [{"type": "audio", "mimeType": "audio/wav", "data": "INVALID!"}]})
                .to_string()
        ),
    ]
}

#[tokio::test]
async fn invalid_later_media_never_uploads_earlier_media() {
    for invalid in invalid_media_items() {
        for original in [
            json!({"content": [image(), invalid]}),
            json!({"structuredContent": [image(), invalid]}),
            json!({"content": [{"type": "text", "text": json!([image(), invalid]).to_string()}]}),
        ] {
            let store = Arc::new(MemoryStore::default());
            let mut value = original.clone();
            let error = InnerMedia::new(Some(store.clone()))
                .prepare(&mut value)
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<InvalidMedia>().is_some(), "{error:#}");
            assert!(
                store.0.lock().unwrap().is_empty(),
                "uploaded before validation"
            );
            assert_eq!(
                value, original,
                "validation must not partially rewrite output"
            );
        }
    }
}

#[tokio::test]
async fn later_oversized_media_never_uploads_earlier_media() {
    let max_bytes = harnx_script::FsLimits::default().max_file_size;
    let max_encoded = max_bytes.div_ceil(3) * 4;
    // The rounded encoded limit can still decode to too many bytes.
    for encoded_len in [max_encoded, max_encoded + 4] {
        let oversized = "A".repeat(encoded_len as usize);
        let mut value = json!({"content": [
            image(),
            {"type": "audio", "mimeType": "audio/wav", "data": oversized},
        ]});
        let store = Arc::new(MemoryStore::default());
        let error = InnerMedia::new(Some(store.clone()))
            .prepare(&mut value)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("file-size limit"));
        assert!(
            store.0.lock().unwrap().is_empty(),
            "uploaded before size validation"
        );
    }
}

fn uri_only_resources(uri: &str) -> [Value; 2] {
    [
        json!({"type": "resource", "resource": {"uri": uri}}),
        json!({"type": "resource_link", "uri": uri}),
    ]
}

#[tokio::test]
async fn uri_only_resources_reject_malformed_media_cids_as_inner_errors() {
    for uri in [
        "cid:media:bad".to_owned(),
        "cid:media:pantheon%2Fatlas/inner1/not-a-hash".to_owned(),
        format!("cid:media:pantheon%2Fatlas/inner1/{}/extra", "a".repeat(64)),
    ] {
        for block in uri_only_resources(&uri) {
            let (provider, _ctx) = harness(json!({"content": [block]}), None, None, false);
            let result = run(&provider, "screenshot -o json; echo should-not-run").await;
            assert_exit_code(&result, 1);
            assert_command_not_run(&result);
            assert_eq!(result["trace"][0]["status"], "tool_error");
            assert!(result["stderr"]
                .as_str()
                .unwrap()
                .contains("invalid media attachment reference"));
        }
    }
}

#[tokio::test]
async fn uri_only_resources_preserve_valid_non_inline_links_without_storage() {
    for uri in [
        format!("cid:media:pantheon%2Fatlas/inner1/{}", "a".repeat(64)),
        "https://example.com/resource".to_owned(),
        "data:text/plain,hi".to_owned(),
    ] {
        for block in uri_only_resources(&uri) {
            let original = json!({"content": [block]});
            let mut value = original.clone();
            InnerMedia::new(None).prepare(&mut value).await.unwrap();
            assert_eq!(value, original);
        }
    }
}

#[tokio::test]
async fn uri_only_resources_externalize_base64_data_through_the_same_store() {
    let store = Arc::new(MemoryStore::default());
    let processor = InnerMedia::new(Some(store.clone()));
    let uri = format!("data:image/png;base64,{PNG}");
    for block in uri_only_resources(&uri) {
        let mut value = json!({"content": [block]});
        processor.prepare(&mut value).await.unwrap();
        assert_no_inline_png(&value);
        assert!(value["content"][0]["path"]
            .as_str()
            .unwrap()
            .starts_with("/attachments/cid:media:"));
        assert_single_upload(&store);
    }
}

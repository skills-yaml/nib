use super::*;

#[test]
fn generation_options_are_shared_across_the_contract() {
    let options = GenerationOptions::provider_default()
        .with_temperature(1.0)
        .expect("valid");
    assert_eq!(options.temperature(), Some(1.0));
    assert_eq!(
        options.resolved_reasoning(Some(ReasoningEffort::Medium)),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        options
            .with_reasoning(ReasoningOption::Disabled)
            .resolved_reasoning(Some(ReasoningEffort::Medium)),
        Some(ReasoningEffort::None)
    );
}

#[test]
fn every_openai_compatible_adapter_omits_provider_default_temperature() {
    let messages = [LlmMessage::user("inspect")];
    let tools = [ToolDefinition::function("read_file")];
    for provider in ["openai", "grok", "openrouter", "meta"] {
        let client = openai_compat(provider, "https://example.test/v1".to_string());
        let body = client
            .request_body(LlmRequest::new(&messages, Some(&tools)), false)
            .expect("valid Chat request");
        assert!(
            body.get("temperature").is_none(),
            "{provider} leaked provider-default temperature"
        );
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }
}

#[test]
fn every_openai_compatible_adapter_serializes_explicit_finite_temperature() {
    let messages = [LlmMessage::user("inspect")];
    for provider in ["openai", "grok", "openrouter", "meta"] {
        let client = openai_compat(provider, "https://example.test/v1".to_string());
        let request = LlmRequest::new(&messages, None)
            .with_temperature(0.4)
            .expect("valid temperature");
        let body = client
            .request_body(request, false)
            .expect("valid Chat request");
        assert_eq!(body["temperature"], 0.4, "{provider}");
    }
}

#[test]
fn anthropic_and_gemini_omit_provider_default_temperature_and_serialize_explicit_values() {
    let messages = [LlmMessage::user("inspect")];
    let anthropic = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude".to_string(),
        vec!["key".to_string()],
        "https://api.anthropic.com/v1/messages",
    )
    .expect("anthropic");
    let default_body = anthropic
        .request_body(LlmRequest::new(&messages, None), false)
        .expect("anthropic default");
    assert!(default_body.get("temperature").is_none());
    let explicit = anthropic
        .request_body(
            LlmRequest::new(&messages, None)
                .with_temperature(0.2)
                .expect("temp"),
            false,
        )
        .expect("anthropic explicit");
    assert_eq!(explicit["temperature"], 0.2);

    let gemini = crate::llm::gemini::GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["key".to_string()],
        "https://generativelanguage.googleapis.com/v1beta",
    )
    .expect("gemini");
    let default_body = gemini
        .request_body(LlmRequest::new(&messages, None))
        .expect("gemini default");
    assert!(default_body.get("generationConfig").is_none());
    let explicit = gemini
        .request_body(
            LlmRequest::new(&messages, None)
                .with_temperature(1.5)
                .expect("temp"),
        )
        .expect("gemini explicit");
    assert_eq!(explicit["generationConfig"]["temperature"], 1.5);
}

#[test]
fn responses_rejects_explicit_temperature_before_io() {
    let client = OpenAiResponsesClient::configured(
        "openai".to_string(),
        "fixture-model".to_string(),
        vec!["fixture-key".to_string()],
        "http://127.0.0.1:9".to_string(),
        None,
    );
    let messages = [LlmMessage::user("inspect")];
    let request = LlmRequest::new(&messages, None)
        .with_temperature(0.75)
        .expect("valid temperature");
    let error = client
        .request_body(request, false)
        .expect_err("Responses must not discard explicit temperature");
    assert!(
        error.contains("do not support explicit temperature"),
        "{error}"
    );
}

#[tokio::test]
async fn mock_rejects_unsupported_reasoning_before_io() {
    let messages = [LlmMessage::user("inspect")];
    let request =
        || LlmRequest::new(&messages, None).with_reasoning_effort(Some(ReasoningEffort::Medium));

    let mock = MockLlmClient::new();
    let complete = mock.complete(request()).await.expect_err("mock complete");
    let stream = mock.stream(request()).await.expect_err("mock stream");
    assert_eq!(complete.class, LlmErrorClass::UnsupportedRequest);
    assert_eq!(stream.class, complete.class);
    assert_eq!(complete.phase, LlmErrorPhase::Request);
    assert_eq!(stream.phase, complete.phase);
    assert!(complete.contains("do not support reasoning_effort"));
}

#[tokio::test]
async fn network_conformance_matrix_keeps_http_errors_typed_bounded_and_provider_owned() {
    for adapter in NETWORK_ADAPTERS {
        let mut statuses = vec![400, 401, 403];
        statuses.extend_from_slice(
            crate::llm::registry::retry_capabilities(
                adapter.provider(),
                adapter.registry_transport(),
            )
            .retryable_http_statuses,
        );
        statuses.sort_unstable();
        statuses.dedup();
        for status in statuses {
            let mut paired = Vec::new();
            for stream in [false, true] {
                let scripted = scripted_http_failures(adapter, status);
                let expected_attempts = scripted.len();
                let (base_url, requests) = serve_sequence(scripted);
                let error = invoke_failure(
                    adapter.client(base_url).as_ref(),
                    stream,
                    REMOTE_PROMPT_SENTINEL,
                )
                .await;
                assert_eq!(
                    error.class,
                    expected_http_class(status),
                    "{} {status} class",
                    adapter.label()
                );
                assert_eq!(
                    error.phase,
                    LlmErrorPhase::HttpResponse,
                    "{} {status} phase",
                    adapter.label()
                );
                assert_eq!(
                    error.http_status,
                    Some(status),
                    "{} status",
                    adapter.label()
                );
                assert_eq!(
                    error.attempts.attempts(),
                    expected_attempts as u8,
                    "{} {status} attempts",
                    adapter.label()
                );
                assert_eq!(
                    error.request_id.as_deref(),
                    adapter.documented_request_id().map(|(_, value)| value),
                    "{} request-ID ownership",
                    adapter.label()
                );
                assert_safe_error_surface(adapter, &error);

                for _ in 0..expected_attempts {
                    let request = requests
                        .recv_timeout(Duration::from_secs(5))
                        .expect("captured conformance request");
                    let request_line = request.lines().next().expect("request line");
                    assert!(
                        request_line.contains(adapter.expected_path(stream)),
                        "{} expected path in {request_line}",
                        adapter.label()
                    );
                }
                paired.push(error);
            }
            assert_eq!(
                paired[0].class,
                paired[1].class,
                "{} {status} complete/stream class",
                adapter.label()
            );
            assert_eq!(
                paired[0].phase,
                paired[1].phase,
                "{} {status} complete/stream phase",
                adapter.label()
            );
        }
    }
}

#[tokio::test]
async fn terminal_authority_matrix_rejects_incomplete_unsafe_and_malformed_turns() {
    for adapter in NETWORK_ADAPTERS {
        for case in TERMINAL_CASES {
            let fixture = terminal_fixture(adapter, case);
            let (complete_url, complete_requests) =
                serve_once("200 OK", "application/json", fixture.complete);
            let complete = observe_turn(adapter, complete_url, false).await;
            complete_requests
                .recv_timeout(Duration::from_secs(5))
                .expect("complete terminal request");
            let refusal_is_allowed = case == TerminalCase::RefusedOrSafetyBlocked;
            assert_no_tool_authority(adapter, &complete, refusal_is_allowed);

            let (stream_url, stream_requests) =
                serve_once("200 OK", "text/event-stream", fixture.stream);
            let streamed = observe_turn(adapter, stream_url, true).await;
            stream_requests
                .recv_timeout(Duration::from_secs(5))
                .expect("stream terminal request");
            assert_no_tool_authority(adapter, &streamed, refusal_is_allowed);

            match (&complete.result, &streamed.result) {
                (Err(complete_error), Err(stream_error)) => {
                    assert_eq!(
                        complete_error.class,
                        stream_error.class,
                        "{} {case:?} class parity",
                        adapter.label()
                    );
                    assert_eq!(
                        complete_error.phase,
                        LlmErrorPhase::TerminalValidation,
                        "{} {case:?} complete phase",
                        adapter.label()
                    );
                    assert_eq!(
                        stream_error.phase,
                        LlmErrorPhase::Stream,
                        "{} {case:?} stream phase",
                        adapter.label()
                    );
                }
                (Ok(complete_response), Ok(stream_response)) => {
                    assert_eq!(
                        complete_response.terminal_status,
                        stream_response.terminal_status,
                        "{} {case:?} terminal parity",
                        adapter.label()
                    );
                    assert_eq!(
                        complete_response.finish_reason,
                        stream_response.finish_reason,
                        "{} {case:?} finish parity",
                        adapter.label()
                    );
                }
                _ => panic!(
                    "{} {case:?} complete/stream authority mismatch",
                    adapter.label()
                ),
            }
        }
    }
}

#[test]
fn conformance_matrix_exactly_covers_every_registered_provider_transport() {
    let registered = PROVIDERS
        .iter()
        .flat_map(|provider| {
            provider
                .transports()
                .map(move |transport| (provider.id, transport))
        })
        .collect::<std::collections::BTreeSet<_>>();
    let covered = ALL_ADAPTERS
        .iter()
        .map(|adapter| (adapter.provider(), adapter.registry_transport()))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(covered, registered);
    assert_eq!(covered.len(), ALL_ADAPTERS.len());
}

#[tokio::test]
async fn every_registered_transport_completes_the_same_safe_text_turn() {
    for adapter in ALL_ADAPTERS {
        if adapter == ConformanceAdapter::Mock {
            let client = adapter.client(String::new());
            let messages = [LlmMessage::user("safe terminal")];
            let complete = client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect("Mock completion");
            let stream = client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect("Mock stream");
            let streamed = stream.finish().await.expect("Mock stream completion");
            assert_eq!(streamed.content, complete.content);
            assert_eq!(streamed.finish_reason, complete.finish_reason);
            assert_eq!(streamed.attempts.attempts(), 0);
            continue;
        }

        let fixture = successful_fixture(adapter);
        let mut responses = Vec::new();
        for (stream, content_type, body) in [
            (false, "application/json", fixture.complete),
            (true, "text/event-stream", fixture.stream),
        ] {
            let (base_url, requests) = serve_once("200 OK", content_type, body);
            let observed = observe_turn(adapter, base_url, stream).await;
            let request = requests
                .recv_timeout(Duration::from_secs(5))
                .expect("captured successful request");
            assert!(
                request
                    .lines()
                    .next()
                    .expect("request line")
                    .contains(adapter.expected_path(stream)),
                "{} successful path",
                adapter.label()
            );
            assert!(observed
                .public_events
                .iter()
                .all(|event| { !format!("{event:?}").contains("private_call_matrix") }));
            let response = observed.result.expect("safe terminal completion");
            assert!(
                observed.public_events.is_empty(),
                "{} provider deltas must stay private even for a valid stream",
                adapter.label()
            );
            responses.push(response);
        }
        assert_eq!(responses[0].terminal_status, LlmTerminalStatus::Completed);
        assert_eq!(responses[1].terminal_status, responses[0].terminal_status);
        assert_eq!(responses[0].content.as_deref(), Some("matrix-ok"));
        assert_eq!(responses[1].content, responses[0].content);
        assert_eq!(responses[1].finish_reason, responses[0].finish_reason);
        assert!(responses.iter().all(|response| {
            response.tool_calls.is_none()
                && response.continuation.is_none()
                && response.attempts.attempts() == 1
        }));
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn every_network_transport_correlates_parallel_results_across_complete_and_stream() {
    for adapter in NETWORK_ADAPTERS {
        for stream in [false, true] {
            let final_fixture = successful_fixture(adapter);
            let (base_url, requests) = serve_sequence(vec![
                parallel_tool_fixture(adapter, stream),
                ScriptedHttpResponse::new(
                    "200 OK",
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                    if stream {
                        final_fixture.stream
                    } else {
                        final_fixture.complete
                    },
                ),
            ]);
            let client = adapter.client(base_url);
            let messages = [LlmMessage::user("correlate parallel tools")];
            let tools = [
                ToolDefinition::function("probe_alpha"),
                ToolDefinition::function("probe_beta"),
            ];
            let scope = LlmRequestScope::new("continuation-matrix", "parallel-run")
                .expect("continuation scope");
            let first_request = LlmRequest::new(&messages, Some(&tools)).with_scope(scope.clone());
            let first = if stream {
                client
                    .stream(first_request)
                    .await
                    .expect("first provider stream")
                    .finish()
                    .await
                    .expect("first provider stream completion")
            } else {
                client
                    .complete(first_request)
                    .await
                    .expect("first provider completion")
            };
            assert_eq!(first.finish_reason, LlmFinishReason::ToolCalls);
            let calls = first.tool_calls.as_ref().expect("parallel tool calls");
            assert_eq!(calls.len(), 2, "{} {stream}", adapter.label());
            assert_eq!(calls[0].name, "probe_alpha");
            assert_eq!(calls[1].name, "probe_beta");
            assert_ne!(calls[0].invocation_id, calls[1].invocation_id);
            let alpha_id = calls[0].invocation_id;
            let beta_id = calls[1].invocation_id;

            let mut continuation = first.continuation.expect("provider continuation");
            continuation
                .record_tool_result(
                    ToolResult::error(alpha_id, json!({"correlation": "alpha"})).unwrap(),
                )
                .expect("correlate alpha");
            continuation
                .record_tool_result(
                    ToolResult::success(beta_id, json!({"correlation": "beta"})).unwrap(),
                )
                .expect("correlate beta");
            let second_request = LlmRequest::new(&messages, Some(&tools))
                .with_scope(scope)
                .with_continuation(Some(continuation));
            let second = if stream {
                client
                    .stream(second_request)
                    .await
                    .expect("second provider stream")
                    .finish()
                    .await
                    .expect("second provider stream completion")
            } else {
                client
                    .complete(second_request)
                    .await
                    .expect("second provider completion")
            };
            assert_eq!(second.finish_reason, LlmFinishReason::Complete);
            assert_eq!(second.content.as_deref(), Some("matrix-ok"));

            let first_wire = requests
                .recv_timeout(Duration::from_secs(5))
                .expect("first captured continuation request");
            let second_wire = requests
                .recv_timeout(Duration::from_secs(5))
                .expect("second captured continuation request");
            assert!(
                first_wire
                    .lines()
                    .next()
                    .expect("first request line")
                    .contains(adapter.expected_path(stream)),
                "{} {stream} first path",
                adapter.label()
            );
            let second_body = second_wire
                .split_once("\r\n\r\n")
                .expect("second HTTP body")
                .1;
            for marker in ["probe_alpha", "probe_beta", "alpha", "beta"] {
                assert!(
                    second_body.contains(marker),
                    "{} {stream} missing correlated {marker}: {second_body}",
                    adapter.label()
                );
            }
        }
    }
}

#[tokio::test]
async fn every_registered_transport_has_complete_and_stream_terminal_authority() {
    for adapter in ALL_ADAPTERS {
        if adapter == ConformanceAdapter::Mock {
            let client = adapter.client(String::new());
            let messages = [LlmMessage::user("mock terminal conformance")];
            let complete = client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect("Mock completion");
            let streamed = client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect("Mock stream")
                .finish()
                .await
                .expect("Mock stream terminal");
            assert_eq!(complete.terminal_status, LlmTerminalStatus::Completed);
            assert_eq!(streamed.terminal_status, complete.terminal_status);
            assert_eq!(streamed.finish_reason, complete.finish_reason);
            assert!(complete.continuation.is_none());
            assert!(streamed.continuation.is_none());
            continue;
        }

        let fixture = terminal_fixture(adapter, TerminalCase::MissingTerminal);
        for (stream, content_type, body) in [
            (false, "application/json", fixture.complete.clone()),
            (true, "text/event-stream", fixture.stream.clone()),
        ] {
            let (base_url, _) = serve_once("200 OK", content_type, body);
            let observed = observe_turn(adapter, base_url, stream).await;
            assert_no_tool_authority(adapter, &observed, false);
            assert!(
                observed.result.is_err(),
                "{} missing terminal",
                adapter.label()
            );
        }
    }
}

#[tokio::test]
async fn every_network_transport_rejects_complete_stream_and_event_byte_overflow() {
    for adapter in NETWORK_ADAPTERS {
        for (stream, declared_length) in [
            (false, crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
            (true, crate::llm::MAX_LLM_STREAM_BYTES + 1),
        ] {
            let (base_url, _) = serve_once_with_declared_length(
                "200 OK",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
                "",
                Some(declared_length),
            );
            let error = invoke_failure(
                adapter.client(base_url).as_ref(),
                stream,
                REMOTE_PROMPT_SENTINEL,
            )
            .await;
            assert_eq!(
                error.class,
                LlmErrorClass::Protocol,
                "{} byte class",
                adapter.label()
            );
            assert_eq!(
                error.phase,
                LlmErrorPhase::TerminalValidation,
                "{} byte phase",
                adapter.label()
            );
            assert_safe_error_surface(adapter, &error);
        }

        let oversized_event = format!(
            "data: {}\n\n",
            "x".repeat(crate::llm::MAX_LLM_STREAM_EVENT_BYTES + 1)
        );
        let (base_url, _) = serve_once("200 OK", "text/event-stream", oversized_event);
        let error = invoke_failure(
            adapter.client(base_url).as_ref(),
            true,
            REMOTE_PROMPT_SENTINEL,
        )
        .await;
        assert_eq!(
            error.class,
            LlmErrorClass::Protocol,
            "{} event class",
            adapter.label()
        );
        assert_eq!(
            error.phase,
            LlmErrorPhase::Stream,
            "{} event phase",
            adapter.label()
        );
        assert_safe_error_surface(adapter, &error);
    }
}

#[tokio::test]
async fn every_network_transport_bounds_complete_and_stream_output_items() {
    for adapter in NETWORK_ADAPTERS {
        let fixture = output_item_overflow_fixture(adapter);
        for (stream, content_type, body) in [
            (false, "application/json", fixture.complete),
            (true, "text/event-stream", fixture.stream),
        ] {
            let (base_url, _) = serve_once("200 OK", content_type, body);
            let observed = observe_turn(adapter, base_url, stream).await;
            assert_no_tool_authority(adapter, &observed, false);
            let error = observed.result.expect_err("item overflow must fail closed");
            assert_eq!(
                error.phase,
                if stream {
                    LlmErrorPhase::Stream
                } else {
                    LlmErrorPhase::TerminalValidation
                },
                "{} item-limit phase",
                adapter.label()
            );
        }
    }
}

#[tokio::test]
async fn every_network_transport_bounds_stream_event_count() {
    for adapter in NETWORK_ADAPTERS {
        let body = ignorable_stream_event(adapter).repeat(crate::llm::MAX_LLM_STREAM_EVENTS + 1);
        assert!(body.len() < crate::llm::MAX_LLM_STREAM_BYTES);
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let error = invoke_failure(
            adapter.client(base_url).as_ref(),
            true,
            REMOTE_PROMPT_SENTINEL,
        )
        .await;
        assert_eq!(
            error.class,
            LlmErrorClass::Protocol,
            "{} event count",
            adapter.label()
        );
        assert_eq!(
            error.phase,
            LlmErrorPhase::Stream,
            "{} event phase",
            adapter.label()
        );
        assert_safe_error_surface(adapter, &error);
    }
}

#[tokio::test]
async fn receiver_drop_cancels_every_network_transport_before_terminal_authority() {
    for adapter in NETWORK_ADAPTERS {
        let (base_url, request_rx, disconnect_rx) =
            serve_open_stream(first_public_stream_event(adapter));
        let client = adapter.client(base_url);
        let messages = [LlmMessage::user("drop conformance stream")];
        let mut stream = client
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect("conformance stream starts");
        request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured streaming request");
        assert_eq!(
            stream
                .recv_private()
                .await
                .expect("first event")
                .expect("public event"),
            LlmStreamEvent::Delta(LlmDelta::Content("first".to_string())),
            "{} first public event",
            adapter.label()
        );
        drop(stream);
        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "{} receiver drop", adapter.label());
    }
}

#[tokio::test]
async fn post_handshake_failures_keep_actual_attempts_for_every_network_transport() {
    for adapter in NETWORK_ADAPTERS {
        let missing = terminal_fixture(adapter, TerminalCase::MissingTerminal);
        let (base_url, requests) = serve_sequence(vec![
            ScriptedHttpResponse::new(
                "503 Service Unavailable",
                "application/json",
                remote_error_body(),
            ),
            ScriptedHttpResponse::new("200 OK", "text/event-stream", missing.stream),
        ]);
        let observed = observe_turn(adapter, base_url, true).await;
        assert_no_tool_authority(adapter, &observed, false);
        let error = observed
            .result
            .expect_err("post-handshake missing terminal must fail");
        assert_eq!(error.attempts.attempts(), 2, "{} attempts", adapter.label());
        assert_eq!(
            error.phase,
            LlmErrorPhase::Stream,
            "{} phase",
            adapter.label()
        );
        assert!(!error.attempts.credential_rotation_occurred());
        for _ in 0..2 {
            requests
                .recv_timeout(Duration::from_secs(5))
                .expect("captured post-handshake request");
        }
    }
}

#[tokio::test]
async fn complete_and_stream_keep_http_401_classes_equal_for_every_network_provider() {
    let messages = [LlmMessage::user("inspect")];
    let body = json!({"error": {"code": "invalid_api_key", "message": "private"}}).to_string();

    for provider in ["openai", "grok", "openrouter", "meta"] {
        let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
        let complete_error = openai_compat(provider, base_url)
            .complete(LlmRequest::new(&messages, None))
            .await
            .expect_err("complete 401");
        let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
        let stream_error = openai_compat(provider, base_url)
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect_err("stream 401");
        assert_eq!(complete_error.provider, provider);
        assert_complete_and_stream_errors_match(
            provider,
            complete_error,
            stream_error,
            LlmErrorClass::Authentication,
            LlmErrorPhase::HttpResponse,
        )
        .await;
    }

    let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
    let complete_error = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude".to_string(),
        vec!["key".to_string()],
        base_url,
    )
    .expect("anthropic")
    .complete(LlmRequest::new(&messages, None))
    .await
    .expect_err("anthropic complete 401");
    let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
    let stream_error = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude".to_string(),
        vec!["key".to_string()],
        base_url,
    )
    .expect("anthropic")
    .stream(LlmRequest::new(&messages, None))
    .await
    .expect_err("anthropic stream 401");
    assert_eq!(complete_error.provider, "anthropic");
    assert_complete_and_stream_errors_match(
        "anthropic",
        complete_error,
        stream_error,
        LlmErrorClass::Authentication,
        LlmErrorPhase::HttpResponse,
    )
    .await;

    let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
    let complete_error = crate::llm::gemini::GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["key".to_string()],
        base_url,
    )
    .expect("gemini")
    .complete(LlmRequest::new(&messages, None))
    .await
    .expect_err("gemini complete 401");
    let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
    let stream_error = crate::llm::gemini::GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["key".to_string()],
        base_url,
    )
    .expect("gemini")
    .stream(LlmRequest::new(&messages, None))
    .await
    .expect_err("gemini stream 401");
    assert_eq!(complete_error.provider, "google");
    assert_complete_and_stream_errors_match(
        "google",
        complete_error,
        stream_error,
        LlmErrorClass::Authentication,
        LlmErrorPhase::HttpResponse,
    )
    .await;
}

#[tokio::test]
async fn documented_request_ids_reach_complete_and_stream_http_failures() {
    let openai_request_id = "req_openai-123";
    assert_eq!(
        chat_http_failure(
            "openai",
            response_header("x-request-id", openai_request_id),
            false,
        )
        .await
        .request_id
        .as_deref(),
        Some(openai_request_id)
    );
    assert_eq!(
        chat_http_failure(
            "openai",
            response_header("x-request-id", openai_request_id),
            true,
        )
        .await
        .request_id
        .as_deref(),
        Some(openai_request_id)
    );
    assert_eq!(
        responses_http_failure(
            "openai",
            response_header("x-request-id", openai_request_id),
            false,
        )
        .await
        .request_id
        .as_deref(),
        Some(openai_request_id)
    );
    assert_eq!(
        responses_http_failure(
            "openai",
            response_header("x-request-id", openai_request_id),
            true,
        )
        .await
        .request_id
        .as_deref(),
        Some(openai_request_id)
    );

    let anthropic_request_id = "req_anthropic.123";
    assert_eq!(
        anthropic_http_failure(response_header("request-id", anthropic_request_id), false,)
            .await
            .request_id
            .as_deref(),
        Some(anthropic_request_id)
    );
    assert_eq!(
        anthropic_http_failure(response_header("request-id", anthropic_request_id), true,)
            .await
            .request_id
            .as_deref(),
        Some(anthropic_request_id)
    );
}

#[tokio::test]
async fn request_id_capture_is_bounded_redacted_and_provider_isolated() {
    for invalid in [
        "x".repeat(129),
        "request id with spaces".to_string(),
        "fixture-key".to_string(),
    ] {
        let error =
            chat_http_failure("openai", response_header("x-request-id", invalid), false).await;
        assert!(error.request_id.is_none());
    }

    assert!(responses_http_failure(
        "openai",
        response_header("request-id", "wrong-openai-header"),
        true,
    )
    .await
    .request_id
    .is_none());
    assert!(anthropic_http_failure(
        response_header("x-request-id", "wrong-anthropic-header"),
        false,
    )
    .await
    .request_id
    .is_none());

    let both_headers = || {
        vec![
            ("x-request-id".to_string(), "req_compatible".to_string()),
            ("request-id".to_string(), "req_native".to_string()),
        ]
    };
    for provider in ["grok", "openrouter", "meta"] {
        assert!(
            chat_http_failure(provider, both_headers(), false)
                .await
                .request_id
                .is_none(),
            "{provider} Chat must not infer OpenAI or Anthropic request-ID headers"
        );
        assert!(
            responses_http_failure(provider, both_headers(), true)
                .await
                .request_id
                .is_none(),
            "{provider} Responses must not infer OpenAI or Anthropic request-ID headers"
        );
    }
    assert!(gemini_http_failure(both_headers(), false)
        .await
        .request_id
        .is_none());
    assert!(gemini_http_failure(both_headers(), true)
        .await
        .request_id
        .is_none());
}

#[tokio::test]
async fn complete_and_stream_reject_unsupported_reasoning_identically_for_native_and_mock() {
    let messages = [LlmMessage::user("inspect")];
    let request =
        || LlmRequest::new(&messages, None).with_reasoning_effort(Some(ReasoningEffort::Medium));

    let mock = MockLlmClient::new();
    let complete = mock.complete(request()).await.expect_err("mock complete");
    let stream = mock.stream(request()).await.expect_err("mock stream");
    assert_complete_and_stream_errors_match(
        "mock",
        complete,
        stream,
        LlmErrorClass::UnsupportedRequest,
        LlmErrorPhase::Request,
    )
    .await;

    let anthropic = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude".to_string(),
        vec!["key".to_string()],
        "http://127.0.0.1:9",
    )
    .expect("anthropic");
    let complete = anthropic
        .complete(request())
        .await
        .expect_err("anthropic complete");
    let stream = anthropic
        .stream(request())
        .await
        .expect_err("anthropic stream");
    assert_complete_and_stream_errors_match(
        "anthropic-reasoning",
        complete,
        stream,
        LlmErrorClass::UnsupportedRequest,
        LlmErrorPhase::Request,
    )
    .await;

    let gemini = crate::llm::gemini::GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["key".to_string()],
        "http://127.0.0.1:9",
    )
    .expect("gemini");
    let complete = gemini
        .complete(request())
        .await
        .expect_err("gemini complete");
    let stream = gemini.stream(request()).await.expect_err("gemini stream");
    assert_complete_and_stream_errors_match(
        "gemini-reasoning",
        complete,
        stream,
        LlmErrorClass::UnsupportedRequest,
        LlmErrorPhase::Request,
    )
    .await;
}

#[tokio::test]
async fn final_transient_http_errors_report_actual_exhaustion_rotation_and_hints() {
    for stream in [false, true] {
        let responses = (0..3)
            .map(|attempt| {
                let response = json_response(
                    "429 Too Many Requests",
                    json!({"error": {"code": "rate_limit_exceeded"}}),
                );
                if attempt == 2 {
                    response.with_header("Retry-After", "7")
                } else {
                    response
                }
            })
            .collect();
        let (base_url, requests) = serve_sequence(responses);
        let client = OpenAiCompatClient::configured(
            "openai".to_string(),
            "fixture-model".to_string(),
            vec!["credential-a".to_string(), "credential-b".to_string()],
            base_url,
            None,
        );
        let messages = [LlmMessage::user("rate limit")];
        let error = if stream {
            client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect_err("stream rate limit")
        } else {
            client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("complete rate limit")
        };
        assert_eq!(error.class, LlmErrorClass::RateLimited);
        assert_eq!(
            error.retry,
            RetryDisposition::ExhaustedAfterCredentialRotation
        );
        assert_eq!(error.retry_after_seconds, Some(7));
        assert_eq!(error.attempts.attempts(), 3);
        assert!(error.attempts.credential_rotation_occurred());
        let captured = (0..3)
            .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect::<Vec<_>>();
        assert!(captured[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer credential-a"));
        assert!(captured[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer credential-b"));
        assert!(captured[2]
            .to_ascii_lowercase()
            .contains("authorization: bearer credential-a"));
    }

    let responses = (0..3)
        .map(|attempt| {
            let response = json_response(
                "529 Overloaded",
                json!({"type": "error", "error": {"type": "overloaded_error"}}),
            );
            if attempt == 2 {
                response.with_header("Retry-After", "5")
            } else {
                response
            }
        })
        .collect();
    let (base_url, _) = serve_sequence(responses);
    let error = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude-fixture".to_string(),
        vec!["anthropic-key".to_string()],
        base_url,
    )
    .expect("Anthropic fixture")
    .complete(LlmRequest::new(&[LlmMessage::user("overload")], None))
    .await
    .expect_err("Anthropic overload");
    assert_eq!(error.class, LlmErrorClass::ProviderUnavailable);
    assert_eq!(error.retry, RetryDisposition::Exhausted);
    assert_eq!(error.retry_after_seconds, Some(5));
    assert_eq!(error.attempts.attempts(), 3);
    assert!(!error.attempts.credential_rotation_occurred());
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn retried_tool_result_requests_preserve_method_path_and_body_for_each_wire_dialect() {
    let tool_result = json!({"retry_semantic_probe": true});

    let (base_url, requests) = serve_sequence(retry_then_json(
        json!({
            "choices": [{
                "message": {"tool_calls": [{
                    "id": "call_retry",
                    "function": {"name": "probe", "arguments": "{}"}
                }]},
                "finish_reason": "tool_calls"
            }]
        }),
        json!({"choices": [{"message": {"content": "done"}, "finish_reason": "stop"}]}),
    ));
    let chat = OpenAiCompatClient::configured(
        "openai".to_string(),
        "fixture-model".to_string(),
        vec!["chat-key".to_string()],
        base_url,
        None,
    );
    let scope = LlmRequestScope::new("retry-session", "chat-run").unwrap();
    let first = chat
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope.clone()),
        )
        .await
        .unwrap();
    let call = &first.tool_calls.as_ref().unwrap()[0];
    let mut continuation = first.continuation.unwrap();
    continuation
        .record_tool_result(ToolResult::success(call.invocation_id, tool_result.clone()).unwrap())
        .unwrap();
    let response = chat
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope)
            .with_continuation(Some(continuation)),
        )
        .await
        .unwrap();
    assert_eq!(response.attempts.attempts(), 3);
    let retried_body = assert_three_retry_requests_are_semantically_identical(
        &requests,
        "retry_semantic_probe",
        "authorization",
        "Bearer chat-key",
    );
    assert_eq!(retried_body["messages"][0]["role"], "user");
    assert_eq!(retried_body["messages"][1]["role"], "assistant");
    assert_eq!(retried_body["messages"][2]["role"], "tool");

    let (base_url, requests) = serve_sequence(retry_then_json(
        json!({
            "id": "resp_retry_1",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "status": "completed",
                "call_id": "call_retry",
                "name": "probe",
                "arguments": "{}"
            }]
        }),
        json!({
            "id": "resp_retry_2",
            "status": "completed",
            "output": [{
                "type": "message",
                "status": "completed",
                "content": [{"type": "output_text", "text": "done"}]
            }]
        }),
    ));
    let responses = OpenAiResponsesClient::configured(
        "openai",
        "fixture-model".to_string(),
        vec!["responses-key".to_string()],
        format!("{base_url}/v1/responses"),
        None,
    );
    let scope = LlmRequestScope::new("retry-session", "responses-run").unwrap();
    let first = responses
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope.clone()),
        )
        .await
        .unwrap();
    let call = &first.tool_calls.as_ref().unwrap()[0];
    let mut continuation = first.continuation.unwrap();
    continuation
        .record_tool_result(ToolResult::success(call.invocation_id, tool_result.clone()).unwrap())
        .unwrap();
    let response = responses
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope)
            .with_continuation(Some(continuation)),
        )
        .await
        .unwrap();
    assert_eq!(response.attempts.attempts(), 3);
    let retried_body = assert_three_retry_requests_are_semantically_identical(
        &requests,
        "retry_semantic_probe",
        "authorization",
        "Bearer responses-key",
    );
    assert_eq!(retried_body["input"][0]["role"], "user");
    assert_eq!(retried_body["input"][1]["type"], "function_call");
    assert_eq!(retried_body["input"][2]["type"], "function_call_output");

    let (base_url, requests) = serve_sequence(retry_then_json(
        json!({
            "content": [{"type": "tool_use", "id": "toolu_retry", "name": "probe", "input": {}}],
            "stop_reason": "tool_use"
        }),
        json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"}),
    ));
    let anthropic = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude-fixture".to_string(),
        vec!["anthropic-key".to_string()],
        base_url,
    )
    .unwrap();
    let scope = LlmRequestScope::new("retry-session", "anthropic-run").unwrap();
    let first = anthropic
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope.clone()),
        )
        .await
        .unwrap();
    let call = &first.tool_calls.as_ref().unwrap()[0];
    let mut continuation = first.continuation.unwrap();
    continuation
        .record_tool_result(ToolResult::success(call.invocation_id, tool_result.clone()).unwrap())
        .unwrap();
    let response = anthropic
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope)
            .with_continuation(Some(continuation)),
        )
        .await
        .unwrap();
    assert_eq!(response.attempts.attempts(), 3);
    let retried_body = assert_three_retry_requests_are_semantically_identical(
        &requests,
        "retry_semantic_probe",
        "x-api-key",
        "anthropic-key",
    );
    assert_eq!(retried_body["messages"][0]["role"], "user");
    assert_eq!(retried_body["messages"][1]["role"], "assistant");
    assert_eq!(retried_body["messages"][2]["role"], "user");
    assert_eq!(
        retried_body["messages"][2]["content"][0]["type"],
        "tool_result"
    );

    let (base_url, requests) = serve_sequence(retry_then_json(
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"functionCall": {"name": "probe", "args": {}}}]},
                "finishReason": "STOP"
            }]
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "done"}]}, "finishReason": "STOP"}]
        }),
    ));
    let gemini = crate::llm::gemini::GeminiClient::with_base_url(
        "gemini-fixture".to_string(),
        vec!["gemini-key".to_string()],
        base_url,
    )
    .unwrap();
    let scope = LlmRequestScope::new("retry-session", "gemini-run").unwrap();
    let first = gemini
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope.clone()),
        )
        .await
        .unwrap();
    let call = &first.tool_calls.as_ref().unwrap()[0];
    let mut continuation = first.continuation.unwrap();
    continuation
        .record_tool_result(ToolResult::success(call.invocation_id, tool_result).unwrap())
        .unwrap();
    let response = gemini
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("call probe")],
                Some(&[ToolDefinition::function("probe")]),
            )
            .with_scope(scope)
            .with_continuation(Some(continuation)),
        )
        .await
        .unwrap();
    assert_eq!(response.attempts.attempts(), 3);
    let retried_body = assert_three_retry_requests_are_semantically_identical(
        &requests,
        "retry_semantic_probe",
        "x-goog-api-key",
        "gemini-key",
    );
    assert_eq!(retried_body["contents"][0]["role"], "user");
    assert_eq!(retried_body["contents"][1]["role"], "model");
    assert_eq!(retried_body["contents"][2]["role"], "user");
    assert!(retried_body["contents"][2]["parts"][0]
        .get("functionResponse")
        .is_some());
}

#[tokio::test]
async fn post_handshake_stream_failure_retains_successful_retry_attempts() {
    let (base_url, _) = serve_sequence(vec![
            json_response("503 Service Unavailable", json!({"error": {}})),
            ScriptedHttpResponse::new(
                "200 OK",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            ),
        ]);
    let client = OpenAiCompatClient::configured(
        "openai".to_string(),
        "fixture-model".to_string(),
        vec!["fixture-key".to_string()],
        base_url,
        None,
    );
    let stream = client
        .stream(LlmRequest::new(
            &[LlmMessage::user("stream after retry")],
            None,
        ))
        .await
        .expect("HTTP handshake succeeds after retry");
    let error = stream
        .finish()
        .await
        .expect_err("premature stream EOF remains a protocol failure");
    assert_eq!(error.phase, LlmErrorPhase::Stream);
    assert_eq!(error.retry, RetryDisposition::NotRetryable);
    assert_eq!(error.attempts.attempts(), 2);
    assert!(!error.attempts.credential_rotation_occurred());

    let (base_url, _) = serve_sequence(vec![
            json_response("503 Service Unavailable", json!({"error": {}})),
            ScriptedHttpResponse::new(
                "200 OK",
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
        ]);
    let completed = OpenAiCompatClient::configured(
        "openai".to_string(),
        "fixture-model".to_string(),
        vec!["fixture-key".to_string()],
        base_url,
        None,
    )
    .stream(LlmRequest::new(
        &[LlmMessage::user("successful stream after retry")],
        None,
    ))
    .await
    .unwrap()
    .finish()
    .await
    .unwrap();
    assert_eq!(completed.content.as_deref(), Some("done"));
    assert_eq!(completed.attempts.attempts(), 2);
    assert!(!completed.attempts.credential_rotation_occurred());
}

#[tokio::test]
async fn mock_complete_and_stream_run_correlated_parallel_tool_continuations() {
    async fn two_request_turn(stream: bool) -> Value {
        let client = MockLlmClient::new();
        let scope = LlmRequestScope::new(
            "mock-conformance-session",
            if stream { "stream-run" } else { "complete-run" },
        )
        .expect("Mock scope");
        let messages = [LlmMessage::user("mock parallel continuation")];
        let tools = [
            ToolDefinition::function("record_probe_a"),
            ToolDefinition::function("record_probe_b"),
        ];
        let first_request = LlmRequest::new(&messages, Some(&tools)).with_scope(scope.clone());
        let first = if stream {
            client
                .stream(first_request)
                .await
                .expect("first Mock stream")
                .finish()
                .await
                .expect("first Mock stream completion")
        } else {
            client
                .complete(first_request)
                .await
                .expect("first Mock turn")
        };
        assert_eq!(first.attempts.attempts(), 0);
        assert!(
            first.usage.is_none(),
            "Mock must report usage as unavailable"
        );
        let calls = first.tool_calls.as_ref().expect("parallel Mock calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "record_probe_a");
        assert_eq!(calls[1].name, "record_probe_b");
        assert_ne!(calls[0].invocation_id, calls[1].invocation_id);

        let mut continuation = first.continuation.expect("Mock continuation");
        continuation
            .record_tool_result(
                ToolResult::success(calls[1].invocation_id, json!({"receipt": "b"}))
                    .expect("second result"),
            )
            .expect("correlate second result first");
        continuation
            .record_tool_result(
                ToolResult::error(calls[0].invocation_id, json!({"receipt": "a"}))
                    .expect("first result"),
            )
            .expect("correlate first result second");

        let second_request = LlmRequest::new(&messages, Some(&tools))
            .with_scope(scope)
            .with_continuation(Some(continuation));
        let second = if stream {
            client
                .stream(second_request)
                .await
                .expect("second Mock stream")
                .finish()
                .await
                .expect("second Mock stream completion")
        } else {
            client
                .complete(second_request)
                .await
                .expect("second Mock turn")
        };
        assert_eq!(second.attempts.attempts(), 0);
        assert!(
            second.usage.is_none(),
            "Mock must report usage as unavailable"
        );
        let content = second.content.expect("Mock final content");
        let encoded = content
            .strip_prefix("Mock tool results: ")
            .expect("Mock result prefix");
        serde_json::from_str(encoded).expect("Mock result projection")
    }

    let complete = two_request_turn(false).await;
    let streamed = two_request_turn(true).await;
    assert_eq!(complete, streamed);
    assert_eq!(complete[0]["tool"], "record_probe_a");
    assert_eq!(complete[0]["classification"], "error");
    assert_eq!(complete[0]["output"], json!({"receipt": "a"}));
    assert_eq!(complete[1]["tool"], "record_probe_b");
    assert_eq!(complete[1]["classification"], "success");
    assert_eq!(complete[1]["output"], json!({"receipt": "b"}));
}

#[tokio::test]
async fn factory_mock_client_accepts_typed_requests() {
    let mut providers = HashMap::new();
    providers.insert(
        "mock".to_string(),
        ProviderEntry {
            model: "mock-model".to_string(),
            ..ProviderEntry::default()
        },
    );
    let config = LlmConfig {
        active_provider: Some("mock".to_string()),
        providers,
        ..LlmConfig::default()
    };
    let client = create_client(&config, Some("mock")).expect("mock client");
    let messages = [LlmMessage::user("explore project")];
    let tools = [ToolDefinition::function("list_directory")];
    let scope = LlmRequestScope::new("factory-session", "factory-run").expect("scope");
    let response = client
        .complete(LlmRequest::new(&messages, Some(&tools)).with_scope(scope))
        .await
        .expect("typed mock completion");
    assert!(response.tool_calls.is_some());
}

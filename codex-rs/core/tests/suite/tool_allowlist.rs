use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_allowlist_denies_unadvertised_shell_execution() -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("r1"),
                responses::ev_function_call(
                    "denied",
                    "exec_command",
                    &json!({"cmd":"touch forbidden"}).to_string(),
                ),
                responses::ev_completed("r1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("answer", "Tool was denied."),
                responses::ev_completed("r2"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.allowed_tools = Some(Vec::new());
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Try the tool".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body_json()["tools"], json!([]));
    assert!(
        requests[1]
            .body_json()
            .to_string()
            .contains("not allowed by this thread")
    );
    assert!(!test.cwd.path().join("forbidden").exists());
    Ok(())
}

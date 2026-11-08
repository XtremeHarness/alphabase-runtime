use super::*;
use crate::auth::AuthProvider;
use crate::provider::RetryConfig;
use codex_http_client::HttpClientBuilder;
use codex_http_client::ReqwestTransport;
use std::time::Duration;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

struct LocalAuth;

impl AuthProvider for LocalAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) {}
}

fn client(server: &MockServer, headers: HeaderMap) -> ResponsesClient<ReqwestTransport> {
    ResponsesClient::new(
        ReqwestTransport::from_http_client(HttpClientBuilder::new().build_direct().unwrap()),
        Provider {
            name: "fixture".into(),
            base_url: format!("{}/runtime-bridge/v1", server.uri()),
            query_params: None,
            headers,
            retry: RetryConfig {
                max_attempts: 3,
                base_delay: Duration::from_millis(10),
                retry_429: true,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_millis(300),
        },
        Arc::new(LocalAuth),
    )
}

#[tokio::test]
async fn private_bridge_header_timeout_does_not_retry_ambiguous_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/runtime-bridge/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .expect(1)
        .mount(&server)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert("x-alphabase-provider", HeaderValue::from_static("fixture"));
    let result = client(&server, headers)
        .stream(
            serde_json::json!({"input": [{"type": "function_call_output", "call_id": "check", "output": "verified"}]}),
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await;

    let Err(error @ ApiError::BridgeAcknowledgementTimeout) = result else {
        panic!("expected the private bridge acknowledgement timeout");
    };
    assert!(!crate::map_api_error(error).is_retryable());
    server.verify().await;
}

#[tokio::test]
async fn ordinary_responses_headers_are_outside_the_stream_idle_window() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/runtime-bridge/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(450)))
        .expect(1)
        .mount(&server)
        .await;
    let result = client(&server, HeaderMap::new())
        .stream(
            serde_json::json!({"input": "hello"}),
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await;

    assert!(result.is_ok());
}

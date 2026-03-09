//!
//! providers/tests/providers.rs
//!
//! End-to-end tests for the HTTP-backed embedding clients (OpenAI, OpenRouter,
//! Mistral, Gemini, Cohere) plus the factory's configuration/error paths.
//!
//! These tests never touch a real provider: each client is pointed at a tiny
//! in-process mock HTTP server (`providers::testing`) that records the request
//! bodies it receives and replies with canned responses. This lets us assert
//! both the request the client *sends* (model name, `dimensions`, input shape)
//! and how it maps the response back into `Embedding`s — including the
//! order-by-index guarantee and the provider-specific error mapping — without
//! any API keys or network access.
//!
//! The AWS Titan client builds its URL from `region`/`model_id` internally (it
//! cannot be redirected at a base URL), so its request/response codecs are
//! covered by the in-crate unit tests in `src/titan.rs` and `src/aws_sigv4.rs`
//! instead.

use providers::testing as mock;
use providers::{EmbeddingBackend, EmbeddingError, ProviderConfig};

// ---------------------------------------------------------------------------
// OpenAI
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_parses_and_reorders_by_index() {
    use providers::OpenAIClient;
    // Response intentionally out of order: index 1 before index 0.
    let body = r#"{"data":[
        {"embedding":[0.3,0.4],"index":1},
        {"embedding":[0.1,0.2],"index":0}
    ]}"#;
    let m = mock::always(200, body).await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        "sk-test".to_string(),
        Some(2),
        m.url.clone(),
        None,
    );

    let out = client.embed(&["a", "b"], None).await.expect("embed ok");
    assert_eq!(out.len(), 2);
    // Sorted by text_index ascending.
    assert_eq!(out[0].text_index, 0);
    assert_eq!(out[0].vector, vec![0.1, 0.2]);
    assert_eq!(out[1].text_index, 1);
    assert_eq!(out[1].vector, vec![0.3, 0.4]);

    // text-embedding-3-* models forward the requested dimensions.
    let req = m.last_request();
    assert!(req.contains("\"dimensions\":2"), "req was: {req}");
    assert!(req.contains("\"model\":\"text-embedding-3-small\""));
}

#[tokio::test]
async fn openai_omits_dimensions_for_legacy_models() {
    use providers::OpenAIClient;
    let body = r#"{"data":[{"embedding":[0.1],"index":0}]}"#;
    let m = mock::always(200, body).await;
    // ada-002 rejects `dimensions`; the client must not send it even when set.
    let client = OpenAIClient::new(
        "text-embedding-ada-002".to_string(),
        "sk-test".to_string(),
        Some(256),
        m.url.clone(),
        None,
    );
    let _ = client.embed(&["hello"], None).await.expect("embed ok");
    let req = m.last_request();
    assert!(
        !req.contains("dimensions"),
        "ada-002 must omit dimensions: {req}"
    );
}

#[tokio::test]
async fn openai_maps_4xx_to_api_error() {
    use providers::OpenAIClient;
    let m = mock::always(400, r#"{"error":{"code":"invalid_request_error"}}"#).await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        "sk-test".to_string(),
        None,
        m.url.clone(),
        None,
    );
    let err = client.embed(&["x"], None).await.unwrap_err();
    match err {
        EmbeddingError::Api { status, message } => {
            assert_eq!(status, 400);
            // The status and the provider's own code — never its prose.
            assert!(message.contains("invalid_request_error"), "{message}");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    // 4xx is non-retriable: exactly one request made.
    assert_eq!(m.request_count(), 1);
}

// ---------------------------------------------------------------------------
// OpenRouter (OpenAI-compatible; sorts by index; surfaces decode failures)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openrouter_parses_and_forwards_dimensions() {
    use providers::OpenRouterClient;
    let body = r#"{"data":[{"embedding":[1.0,2.0,3.0],"index":0}]}"#;
    let m = mock::always(200, body).await;
    let client = OpenRouterClient::new(
        "openai/text-embedding-3-large".to_string(),
        "or-test".to_string(),
        Some(3),
        m.url.clone(),
        None,
    );
    let out = client.embed(&["doc"], None).await.expect("embed ok");
    assert_eq!(out[0].vector, vec![1.0, 2.0, 3.0]);
    let req = m.last_request();
    assert!(req.contains("\"dimensions\":3"), "{req}");
}

#[tokio::test]
async fn openrouter_decode_failure_becomes_api_200() {
    use providers::OpenRouterClient;
    // 200 OK but the body is an error envelope, not the expected shape.
    let m = mock::always(200, r#"{"error":{"message":"model unavailable"}}"#).await;
    let client = OpenRouterClient::new(
        "openai/text-embedding-3-small".to_string(),
        "or-test".to_string(),
        None,
        m.url.clone(),
        None,
    );
    let err = client.embed(&["x"], None).await.unwrap_err();
    match err {
        EmbeddingError::Api { status, message } => {
            assert_eq!(status, 200);
            // Position and classification, never the body: a 2xx that will
            // not parse is the likeliest place to find the *source text*
            // echoed back, and this message is logged and stored durably.
            assert!(message.contains("not the documented shape"), "{message}");
            assert!(!message.contains("model unavailable"), "{message}");
            assert!(!message.contains("not json"), "{message}");
        }
        other => panic!("expected Api decode error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Mistral (OpenAI-compatible; never sends dimensions; surfaces decode failures)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mistral_parses_and_never_sends_dimensions() {
    use providers::MistralClient;
    let body = r#"{"data":[
        {"embedding":[0.5],"index":0},
        {"embedding":[0.6],"index":1}
    ]}"#;
    let m = mock::always(200, body).await;
    let client = MistralClient::new(
        "mistral-embed-2312".to_string(),
        "mi-test".to_string(),
        m.url.clone(),
        None,
    );
    let out = client.embed(&["a", "b"], None).await.expect("embed ok");
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].vector, vec![0.6]);
    let req = m.last_request();
    assert!(
        !req.contains("dimensions"),
        "mistral must not send dimensions: {req}"
    );
    assert!(req.contains("\"model\":\"mistral-embed-2312\""));
}

#[tokio::test]
async fn mistral_decode_failure_becomes_api_200() {
    use providers::MistralClient;
    let m = mock::always(200, "not json at all").await;
    let client = MistralClient::new(
        "mistral-embed".to_string(),
        "mi-test".to_string(),
        m.url.clone(),
        None,
    );
    let err = client.embed(&["x"], None).await.unwrap_err();
    match err {
        EmbeddingError::Api { status, message } => {
            assert_eq!(status, 200);
            // Position and classification, never the body: a 2xx that will
            // not parse is the likeliest place to find the *source text*
            // echoed back, and this message is logged and stored durably.
            assert!(message.contains("not the documented shape"), "{message}");
            assert!(!message.contains("model unavailable"), "{message}");
            assert!(!message.contains("not json"), "{message}");
        }
        other => panic!("expected Api decode error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Gemini (per-text batch request; auth via URL query key)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gemini_parses_batch_in_order() {
    use providers::GeminiClient;
    let body = r#"{"embeddings":[
        {"values":[0.1,0.2]},
        {"values":[0.3,0.4]}
    ]}"#;
    let m = mock::always(200, body).await;
    let client = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g-test".to_string(),
        None,
        "search_document",
        m.url.clone(),
        None,
    );
    let out = client
        .embed(&["first", "second"], None)
        .await
        .expect("embed ok");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].text_index, 0);
    assert_eq!(out[0].vector, vec![0.1, 0.2]);
    assert_eq!(out[1].text_index, 1);
    assert_eq!(out[1].vector, vec![0.3, 0.4]);

    // One request per text is packed into a single batch body.
    let req = m.last_request();
    assert!(req.contains("\"requests\""), "{req}");
    assert!(req.contains("models/gemini-embedding-001"), "{req}");
}

#[tokio::test]
async fn gemini_maps_error_status() {
    use providers::GeminiClient;
    let m = mock::always(403, r#"{"error":"forbidden"}"#).await;
    let client = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g-test".to_string(),
        None,
        "search_document",
        m.url.clone(),
        None,
    );
    let err = client.embed(&["x"], None).await.unwrap_err();
    assert!(
        matches!(err, EmbeddingError::Api { status: 403, .. }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Secret hygiene: no configured key value may appear in any error Display.
// Error strings travel into host logs (and, mapped to wire codes, toward
// PostgreSQL), so this is a hard guarantee, not a style preference.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_error_display_ever_contains_the_api_key() {
    use providers::{CohereClient, GeminiClient, MistralClient, OpenAIClient, OpenRouterClient};
    const KEY: &str = "sk-super-secret-key-0123456789";

    // A giant error body (larger than the preview cap) and a bearer-auth 401:
    // the Display must carry a bounded preview and never the key.
    let huge_body = format!("{{\"error\":\"{}\"}}", "x".repeat(5000));
    let api_errors = mock::always(401, &huge_body).await;
    let url = api_errors.url.clone();

    // Gemini is the critical case: the key rides the request URL, and
    // reqwest network errors normally print the URL. Point it at a closed
    // port for a guaranteed connect error.
    let closed_port = {
        // Bind then drop: the port existed a moment ago and nothing listens now.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}")
    };

    let clients: Vec<Box<dyn EmbeddingBackend>> = vec![
        Box::new(OpenAIClient::new(
            "m".into(),
            KEY.into(),
            None,
            url.clone(),
            None,
        )),
        Box::new(OpenRouterClient::new(
            "m".into(),
            KEY.into(),
            None,
            url.clone(),
            None,
        )),
        Box::new(MistralClient::new(
            "m".into(),
            KEY.into(),
            url.clone(),
            None,
        )),
        Box::new(CohereClient::new(
            "m".into(),
            KEY.into(),
            "search_document".into(),
            None,
            url.clone(),
            None,
        )),
        // API-error path with the key in the URL:
        Box::new(GeminiClient::new(
            "m".into(),
            KEY.into(),
            None,
            "search_document",
            url,
            None,
        )),
        // Network-error path with the key in the URL:
        Box::new(GeminiClient::new(
            "m".into(),
            KEY.into(),
            None,
            "search_document",
            closed_port,
            None,
        )),
    ];

    for client in clients {
        let err = client.embed(&["x"], None).await.unwrap_err();
        let display = err.to_string();
        assert!(
            !display.contains(KEY),
            "error Display leaked the API key: {display}"
        );
        assert!(
            display.len() < 2000,
            "error Display is unbounded ({} bytes): {display:.120}",
            display.len()
        );
    }
}

/// `Debug` is the other leak channel: a stray `{config:?}` in a log line or
/// panic payload must not print any secret. (Display is covered above.)
#[test]
fn provider_config_debug_redacts_every_secret() {
    let secrets = [
        "sk-api-key-value",
        "bedrock-bearer-value",
        "AKIAEXAMPLEID",
        "aws-secret-value",
    ];
    let config = ProviderConfig {
        provider: "aws".to_string(),
        api_key: Some(secrets[0].to_string()),
        base_url: Some("https://example.invalid".to_string()),
        region: Some("us-east-1".to_string()),
        bearer_token: Some(secrets[1].to_string()),
        access_key_id: Some(secrets[2].to_string()),
        secret_access_key: Some(secrets[3].to_string()),
    };
    let debug = format!("{config:?}");
    for secret in secrets {
        assert!(!debug.contains(secret), "Debug leaked {secret:?}: {debug}");
    }
    // Non-secret fields stay readable, and presence is still visible.
    assert!(debug.contains("us-east-1"), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");
}

// ---------------------------------------------------------------------------
// Cohere (v3 direct array AND v4 nested-under-`float` response shapes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cohere_parses_v4_nested_float() {
    use providers::CohereClient;
    let body = r#"{"embeddings":{"float":[[0.1,0.2],[0.3,0.4]]}}"#;
    let m = mock::always(200, body).await;
    let client = CohereClient::new(
        "embed-v4.0".to_string(),
        "co-test".to_string(),
        "search_document".to_string(),
        None,
        m.url.clone(),
        None,
    );
    let out = client.embed(&["a", "b"], None).await.expect("embed ok");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].vector, vec![0.1, 0.2]);
    assert_eq!(out[1].vector, vec![0.3, 0.4]);
    let req = m.last_request();
    assert!(req.contains("\"input_type\":\"search_document\""), "{req}");
    assert!(req.contains("\"embedding_types\":[\"float\"]"), "{req}");
}

#[tokio::test]
async fn cohere_parses_v3_direct_array() {
    use providers::CohereClient;
    let body = r#"{"embeddings":[[1.0],[2.0],[3.0]]}"#;
    let m = mock::always(200, body).await;
    let client = CohereClient::new(
        "embed-english-v3.0".to_string(),
        "co-test".to_string(),
        "search_query".to_string(),
        None,
        m.url.clone(),
        None,
    );
    let out = client
        .embed(&["a", "b", "c"], None)
        .await
        .expect("embed ok");
    assert_eq!(out.len(), 3);
    assert_eq!(out[2].text_index, 2);
    assert_eq!(out[2].vector, vec![3.0]);
}

// ---------------------------------------------------------------------------
// Retry behaviour exercised through a real client: 503 then 200.
// (Pure-closure retry unit tests live in src/retry.rs.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_retries_transient_5xx_then_succeeds() {
    use providers::OpenAIClient;
    let m = mock::spawn(vec![
        (503, r#"{"error":"unavailable"}"#.to_string()),
        (
            200,
            r#"{"data":[{"embedding":[9.0],"index":0}]}"#.to_string(),
        ),
    ])
    .await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        "sk-test".to_string(),
        None,
        m.url.clone(),
        None,
    );
    let out = client
        .embed(&["x"], None)
        .await
        .expect("should succeed after one retry");
    assert_eq!(out[0].vector, vec![9.0]);
    assert_eq!(m.request_count(), 2, "expected exactly one retry");
}

// ---------------------------------------------------------------------------
// Factory: configuration / authentication error paths. The factory takes a
// resolved ProviderConfig (never the process environment), so these are
// deterministic — no env-var clearing dance needed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn factory_error_paths() {
    use providers::new_embedding_backend;

    // `Box<dyn EmbeddingBackend>` (the Ok variant) isn't `Debug`, so we can't
    // use `unwrap_err`; this helper collapses a result to its error instead.
    fn err_of(r: Result<Box<dyn EmbeddingBackend>, EmbeddingError>) -> EmbeddingError {
        match r {
            Ok(_) => panic!("expected an error, got a client"),
            Err(e) => e,
        }
    }

    fn cfg(provider: &str) -> ProviderConfig {
        ProviderConfig {
            provider: provider.to_string(),
            ..Default::default()
        }
    }

    // Unsupported provider.
    let err = err_of(new_embedding_backend(
        &cfg("does-not-exist"),
        "m",
        0,
        "search_document",
        None,
    ));
    assert!(
        matches!(err, EmbeddingError::Configuration(ref s) if s.contains("Unsupported provider")),
        "{err:?}"
    );

    // Every key-gated provider refuses a config without a key.
    for provider in ["openai", "openrouter", "mistral", "google", "cohere"] {
        let err = err_of(new_embedding_backend(
            &cfg(provider),
            "some-model",
            0,
            "search_document",
            None,
        ));
        assert!(
            matches!(err, EmbeddingError::Authentication(_)),
            "{provider}: expected Authentication error, got {err:?}"
        );
    }

    // AWS surfaces a Configuration error when the region is missing, and an
    // Authentication error when the region is set but no auth variant is.
    let err = err_of(new_embedding_backend(
        &cfg("aws"),
        "amazon.titan-embed-text-v2:0",
        1024,
        "search_document",
        None,
    ));
    assert!(matches!(err, EmbeddingError::Configuration(_)), "{err:?}");

    let mut aws = cfg("aws");
    aws.region = Some("us-east-1".to_string());
    let err = err_of(new_embedding_backend(
        &aws,
        "amazon.titan-embed-text-v2:0",
        1024,
        "search_document",
        None,
    ));
    assert!(matches!(err, EmbeddingError::Authentication(_)), "{err:?}");
}

/// The construction success paths, including the `gemini` → google alias,
/// exercised against the mock so no real key or endpoint is involved.
#[tokio::test]
async fn factory_builds_clients_and_accepts_the_gemini_alias() {
    use providers::new_embedding_backend;

    let body = r#"{"embeddings":[{"values":[0.5]}]}"#;
    let m = mock::always(200, body).await;

    for alias in ["google", "gemini", "GEMINI"] {
        let config = ProviderConfig {
            provider: alias.to_string(),
            api_key: Some("g-test".to_string()),
            base_url: Some(m.url.clone()),
            ..Default::default()
        };
        let backend = new_embedding_backend(&config, "gemini-embedding-001", 0, "", None)
            .expect("gemini client builds");
        let out = backend.embed(&["hi"], None).await.expect("embed ok");
        assert_eq!(out[0].vector, vec![0.5]);
    }
}

// ---------------------------------------------------------------------------
// The gateway, built where postvec actually builds it
// ---------------------------------------------------------------------------

/// The embedded host constructs its gateway on the PostgreSQL **launcher
/// thread**, outside `runtime.block_on` (`postvec/src/client/embedded.rs`) —
/// so `Gateway::load` builds a `reqwest::Client` per provider with no tokio
/// runtime on the thread. Every other test in this tree runs under
/// `#[tokio::test]` and could not see a regression here; a panic would land
/// in a database background worker during startup, which is not a failure
/// mode this project accepts for a configuration file.
#[test]
fn the_gateway_loads_with_no_tokio_runtime_on_the_thread() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    // A providers.d is 0700; the loader refuses a group/world-writable one,
    // and `tempfile` honours the umask.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("openai.toml");
    std::fs::write(
        &path,
        "provider = \"openai\"\napi_key = \"sk-test\"\n\n[[models]]\n\
         name = \"openai-text-embedding-3-small\"\n\
         provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let gateway = providers::gateway::Gateway::load(dir.path(), &Default::default());
    assert!(gateway.owns("openai-text-embedding-3-small"));
    assert_eq!(gateway.inflight_budget(), 4);
}

// ---------------------------------------------------------------------------
// Adversarial upstreams
// ---------------------------------------------------------------------------

/// Nothing in HTTP obliges a peer to send the body size it promised — or to
/// promise one at all. A chunked response with no `Content-Length` can stream
/// until the reader gives up, and `bytes()`/`text()` give up at OOM. In
/// embedded mode that allocation is the PostgreSQL launcher's RSS.
///
/// The client must stop near its own budget, and must classify the refusal as
/// **permanent**: retrying re-runs the same allocation against the same broken
/// peer.
#[tokio::test]
async fn an_endless_chunked_success_body_is_refused_without_buffering_it() {
    use providers::OpenAIClient;
    let m = mock::flood(200).await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        "sk-test".to_string(),
        // dim 2 over 1 input: the budget floors at 256 KiB, far below what a
        // flooding peer would otherwise hand us.
        Some(2),
        m.url.clone(),
        None,
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let err = client.embed(&["x"], Some(deadline)).await.unwrap_err();
    match err {
        EmbeddingError::Api { status, message } => {
            assert_eq!(status, 200, "permanent, not a retriable outage: {message}");
            assert!(message.contains("budget"), "{message}");
        }
        other => panic!("expected a bounded-body refusal, got {other:?}"),
    }
    // The peer got nowhere near unbounded: a few budgets' worth at most,
    // counting the socket buffers it filled after we stopped reading.
    assert!(
        m.flooded_bytes() < 64 * 1024 * 1024,
        "the client kept reading: {} bytes accepted",
        m.flooded_bytes()
    );
}

/// The same bound applies to *error* bodies, which is the worse case: an
/// error is retried, so an unbounded diagnostic allocation would happen once
/// per attempt.
#[tokio::test]
async fn an_endless_error_body_is_bounded_and_not_retried_into_oblivion() {
    use providers::MistralClient;
    let m = mock::flood(400).await;
    let client = MistralClient::new(
        "mistral-embed".to_string(),
        "mi-test".to_string(),
        m.url.clone(),
        None,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let err = client.embed(&["x"], Some(deadline)).await.unwrap_err();
    match err {
        EmbeddingError::Api { status, message } => {
            assert_eq!(status, 400);
            // 400 is permanent, so exactly one attempt was made.
            assert_eq!(m.request_count(), 1, "a 4xx must not be retried");
            assert!(
                message.len() < 4096,
                "the preview is bounded: {}",
                message.len()
            );
        }
        other => panic!("expected a bounded Api error, got {other:?}"),
    }
}

/// The body of an upstream error never leaves the connector.
///
/// Scrubbing the *known* credential out of an arbitrary body defends against
/// the case you thought of. It cannot remove the **source text** a provider
/// echoes back in a 400 — in whatever form it chooses — and that text belongs
/// to a database while this message is logged by the inference host (a
/// different machine in remote mode) and stored in
/// `postvec.jobs_dead.last_error`. So only a short, code-shaped identifier is
/// forwarded, and everything else is dropped.
#[tokio::test]
async fn no_part_of_an_upstream_error_body_reaches_diagnostics() {
    use providers::{CohereClient, OpenAIClient};

    const KEY: &str = "sk-live-0123456789abcdefghij";
    const SOURCE: &str = "quarterly revenue guidance increased materially";

    // 401: the body is withheld entirely.
    let m = mock::always(
        401,
        &format!(r#"{{"error":{{"message":"bad key {KEY}"}}}}"#),
    )
    .await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        KEY.to_string(),
        None,
        m.url.clone(),
        None,
    );
    let err = client.embed(&[SOURCE], None).await.unwrap_err();
    assert!(!err.to_string().contains(KEY), "{err}");

    // Any other status: the credential, the echoed source text, its
    // fragments and any control characters are all absent — only the code
    // survives.
    for status in [400u16, 429, 500] {
        let hostile = format!(
            "{{\"error\":{{\"code\":\"invalid_request_error\",\"message\":\"key {KEY} \
             rejected for input '{SOURCE}'\\nERROR:  forged log line\"}}}}"
        );
        let m = mock::always(status, &hostile).await;
        let client = CohereClient::new(
            "embed-v4.0".to_string(),
            KEY.to_string(),
            "search_document".to_string(),
            None,
            m.url.clone(),
            None,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        let err = client.embed(&[SOURCE], Some(deadline)).await.unwrap_err();
        let text = err.to_string();
        assert!(!text.contains(KEY), "status {status}: {text}");
        assert!(!text.contains(SOURCE), "status {status}: {text}");
        for fragment in ["quarterly", "revenue", "guidance", "materially", "forged"] {
            assert!(
                !text.contains(fragment),
                "status {status} leaked {fragment:?}: {text}"
            );
        }
        assert!(!text.contains('\n') && !text.contains('\r'), "{text}");
        // The one thing that does survive: a machine-readable code.
        assert!(
            text.contains("invalid_request_error"),
            "status {status}: {text}"
        );
    }

    // A code-*shaped* value that is not one this crate knows forwards
    // nothing: the vocabulary is closed, not a grammar, so a provider cannot
    // smuggle a spaceless fragment of the row through `error.code`.
    let m = mock::always(
        400,
        r#"{"error":{"code":"quarterly_revenue_guidance_increased_materially"}}"#,
    )
    .await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        KEY.to_string(),
        None,
        m.url.clone(),
        None,
    );
    let err = client.embed(&[SOURCE], None).await.unwrap_err();
    assert!(!err.to_string().contains("quarterly"), "{err}");
    assert!(!err.to_string().contains("revenue"), "{err}");

    // A body with no recognised code forwards nothing at all.
    let m = mock::always(400, "the model said: quarterly revenue guidance increased").await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        KEY.to_string(),
        None,
        m.url.clone(),
        None,
    );
    let err = client.embed(&[SOURCE], None).await.unwrap_err();
    assert!(!err.to_string().contains("quarterly"), "{err}");
}

/// A token-limit refusal is classified where the body is, so the body does
/// not have to travel to the gateway for the gateway to know.
#[tokio::test]
async fn a_token_limit_refusal_is_classified_at_the_connector() {
    use providers::OpenAIClient;
    let m = mock::always(
        400,
        r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
    )
    .await;
    let client = OpenAIClient::new(
        "text-embedding-3-small".to_string(),
        "sk-test".to_string(),
        None,
        m.url.clone(),
        None,
    );
    let err = client.embed(&["x"], None).await.unwrap_err();
    assert!(
        matches!(err, EmbeddingError::InputTooLong { status: 400 }),
        "{err:?}"
    );
    // …and it says so without quoting the provider.
    assert!(!err.to_string().contains("8192"), "{err}");
}

/// The Gemini key travels in `x-goog-api-key`, the header Google documents,
/// and never in the URL — where every proxy and reverse-proxy access log on
/// the path would record it.
#[tokio::test]
async fn the_gemini_key_is_a_header_not_a_query_parameter() {
    use providers::GeminiClient;
    let m = mock::always(200, r#"{"embeddings":[{"values":[0.5]}]}"#).await;
    let client = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g-secret-key-value".to_string(),
        None,
        "search_document",
        m.url.clone(),
        None,
    );
    client.embed(&["hi"], None).await.expect("embed ok");

    // The mock records request bodies, so assert on the client's own view:
    // an error carries no URL, and the URL it builds has no query at all.
    let m2 = mock::always(500, "boom").await;
    let client = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g-secret-key-value".to_string(),
        None,
        "search_document",
        m2.url.clone(),
        None,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let err = client.embed(&["hi"], Some(deadline)).await.unwrap_err();
    assert!(!err.to_string().contains("g-secret-key-value"), "{err}");
    assert!(!err.to_string().contains("key="), "{err}");
}

/// The file documents `dim` as authoritative and the gateway checks every
/// response against it — so a descriptor asking for anything but a model's
/// native width has to actually *ask* for it. Gemini takes
/// `outputDimensionality`; Cohere v4 takes `output_dimension`. Both also get
/// the query/document purpose, which Gemini spells as `taskType`.
#[tokio::test]
async fn gemini_and_cohere_request_the_declared_dimension_and_purpose() {
    use providers::{CohereClient, GeminiClient};

    let m = mock::always(200, r#"{"embeddings":[{"values":[0.1,0.2]}]}"#).await;
    let client = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g-test".to_string(),
        Some(1536),
        "search_query",
        m.url.clone(),
        None,
    );
    client.embed(&["q"], None).await.expect("embed ok");
    let sent = m.last_request();
    assert!(sent.contains("\"outputDimensionality\":1536"), "{sent}");
    assert!(sent.contains("\"taskType\":\"RETRIEVAL_QUERY\""), "{sent}");

    // Documents get the other task type.
    let client = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g-test".to_string(),
        None,
        "search_document",
        m.url.clone(),
        None,
    );
    client.embed(&["d"], None).await.expect("embed ok");
    let sent = m.last_request();
    assert!(
        sent.contains("\"taskType\":\"RETRIEVAL_DOCUMENT\""),
        "{sent}"
    );
    // No declared dimension: the field is omitted, not sent as null.
    assert!(!sent.contains("outputDimensionality"), "{sent}");

    let m = mock::always(200, r#"{"embeddings":{"float":[[0.1,0.2]]}}"#).await;
    let client = CohereClient::new(
        "embed-v4.0".to_string(),
        "co-test".to_string(),
        "search_document".to_string(),
        Some(1024),
        m.url.clone(),
        None,
    );
    client.embed(&["d"], None).await.expect("embed ok");
    let sent = m.last_request();
    assert!(sent.contains("\"output_dimension\":1024"), "{sent}");
}

/// Google's embedding models do not share one request contract, and the
/// crate documents that arbitrary model ids work. Sending `taskType` or
/// `outputDimensionality` to a model that does not accept them turns a
/// working descriptor into a 400 on every call, so an id outside the known
/// list gets neither and embeds at its native width.
#[tokio::test]
async fn gemini_sends_request_options_only_to_models_documented_to_take_them() {
    use providers::GeminiClient;

    let m = mock::always(200, r#"{"embeddings":[{"values":[0.6,0.8]}]}"#).await;
    let known = GeminiClient::new(
        "gemini-embedding-001".to_string(),
        "g".to_string(),
        Some(1536),
        "search_query",
        m.url.clone(),
        None,
    );
    known.embed(&["q"], None).await.expect("embed ok");
    let sent = m.last_request();
    assert!(sent.contains("\"taskType\""), "{sent}");
    assert!(sent.contains("\"outputDimensionality\""), "{sent}");

    let unknown = GeminiClient::new(
        "some-future-embedding-model".to_string(),
        "g".to_string(),
        Some(1536),
        "search_query",
        m.url.clone(),
        None,
    );
    unknown.embed(&["q"], None).await.expect("embed ok");
    let sent = m.last_request();
    assert!(
        !sent.contains("taskType"),
        "an unknown id must get neither: {sent}"
    );
    assert!(!sent.contains("outputDimensionality"), "{sent}");
}

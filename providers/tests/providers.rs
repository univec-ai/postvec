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
    let m = mock::always(400, r#"{"error":"bad request"}"#).await;
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
            assert!(message.contains("bad request"), "{message}");
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
            assert!(message.contains("decode failed"), "{message}");
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
            assert!(message.contains("decode failed"), "{message}");
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
            url.clone(),
            None,
        )),
        // API-error path with the key in the URL:
        Box::new(GeminiClient::new("m".into(), KEY.into(), url, None)),
        // Network-error path with the key in the URL:
        Box::new(GeminiClient::new("m".into(), KEY.into(), closed_port, None)),
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

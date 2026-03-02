//!
//! providers/src/aws_sigv4.rs
//!
//! A self-contained, from-scratch implementation of the AWS Signature V4 signing process.
//!
//! This module provides the necessary functions to cryptographically sign a `reqwest`
//! request, allowing the application to authenticate with AWS services like Bedrock
//! without relying on the official AWS SDK. The process is complex and follows the
//! official AWS documentation precisely.
//!
//! The main steps are:
//! 1. Create a Canonical Request: A standardized string representation of the request.
//! 2. Create a String to Sign: Combines metadata with a hash of the canonical request.
//! 3. Calculate the Signature: Derives a signing key and uses it to sign the string from step 2.
//! 4. Add the Signature to the Request: The final signature is added to the `Authorization` header.
//!

use crate::EmbeddingError;
use chrono::Utc;
use hex::encode as hex_encode;
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, Request, Url};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type HmacSha256 = Hmac<Sha256>;

/// Build a header value from a string that came out of configuration.
///
/// Deliberately not `.unwrap()`. The `Authorization` value embeds the
/// operator's `access_key_id` verbatim, and this code runs inside the
/// PostgreSQL launcher process in embedded mode — a key file with a stray
/// newline or a non-ASCII byte in it would turn a configuration mistake into
/// a panic in a database background worker. It is a configuration error, so
/// it comes back as one and flows through the same classification as every
/// other provider failure.
fn header_value(field: &str, raw: &str) -> Result<HeaderValue, EmbeddingError> {
    HeaderValue::from_str(raw).map_err(|_| {
        EmbeddingError::Configuration(format!(
            "the AWS {field} is not usable as an HTTP header value (control or non-ASCII \
             characters); check the credential file for stray whitespace"
        ))
    })
}

/// Main function to sign a `reqwest::Request`. It modifies the request's headers in place.
pub fn sign_request(
    request: &mut Request,
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
) -> Result<(), EmbeddingError> {
    // --- Common variables ---
    let now = Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();

    // The signing process needs to read the method, url, body, and original headers
    // before it can calculate the signature. After calculation, it modifies the headers.
    // To respect the borrow checker, we clone the necessary parts for the calculation phase.
    let method = request.method().clone();
    let url = request.url().clone();
    let body = request.body();
    let host = url.host_str().unwrap_or("").to_string();

    // Create a temporary map of headers that includes the original headers
    // plus the new ones required for signing.
    let host_value = header_value("host", &host)?;
    let date_value = header_value("date", &amz_date)?;
    let mut headers_for_signing = request.headers().clone();
    headers_for_signing.insert("host", host_value.clone());
    headers_for_signing.insert("x-amz-date", date_value.clone());

    // --- Task 1: Create a Canonical Request ---
    let (canonical_request, signed_headers) =
        create_canonical_request(&method, &url, &headers_for_signing, body);

    // --- Task 2: Create the String to Sign ---
    let string_to_sign =
        create_string_to_sign(&amz_date, &date_stamp, region, service, &canonical_request);

    // --- Task 3: Calculate the Signature ---
    let signature = calculate_signature(&string_to_sign, &date_stamp, region, service, secret_key);

    // --- Task 4: Add the Signature to the HTTP Request ---
    let authorization_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request, SignedHeaders={}, Signature={}",
        access_key, date_stamp, region, service, signed_headers, signature
    );

    // Now, we get a mutable borrow of the actual request headers and insert the
    // headers that were used for signing.
    let authorization = header_value("access key id", &authorization_header)?;
    let final_headers = request.headers_mut();
    final_headers.insert("host", host_value);
    final_headers.insert("x-amz-date", date_value);
    final_headers.insert("Authorization", authorization);
    Ok(())
}

/// Hashes the request payload (body). Returns a hex-encoded SHA256 hash.
fn hash_payload(body: Option<&reqwest::Body>) -> String {
    let payload = body.and_then(|b| b.as_bytes()).unwrap_or(b"");
    let hash = Sha256::digest(payload);
    hex_encode(hash)
}

// Define a constant for the AWS SigV4 path encoding set.
// This set includes all characters that MUST be encoded, according to AWS documentation.
// It excludes the unreserved characters (A-Z, a-z, 0-9, -, _,., ~) and the path separator '/'.
const AWS_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Query components encode everything except RFC 3986's unreserved set —
/// `/` included, unlike the path set above.
const AWS_QUERY_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn sigv4_encode(raw: &str) -> String {
    utf8_percent_encode(raw, AWS_QUERY_ENCODE_SET).to_string()
}

/// Creates the canonical request string.
fn create_canonical_request(
    method: &Method,
    url: &Url,
    headers: &HeaderMap,
    body: Option<&reqwest::Body>,
) -> (String, String) {
    // Canonical URI
    let path = url.path();
    // AWS requires the path to be normalized. An empty path should be "/".
    // All characters except unreserved ones and '/' must be percent-encoded.
    let canonical_uri = if path.is_empty() {
        "/".to_string()
    } else {
        utf8_percent_encode(path, AWS_PATH_ENCODE_SET).to_string()
    };

    // Canonical Query String, sorted by key. `query_pairs()` hands back
    // *decoded* names and values, so each half has to be re-encoded with the
    // same strict set as the path — AWS canonicalizes what it received, and
    // emitting a decoded space or `&` here would sign a different string
    // than the service verifies. Inert for Bedrock's `InvokeModel`, which
    // carries no query string; wrong the first time any endpoint does.
    let mut query_pairs: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        query_pairs.insert(sigv4_encode(&key), sigv4_encode(&value));
    }
    let canonical_query_string = query_pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Canonical Headers and Signed Headers, sorted by header name.
    let mut canonical_headers_map = BTreeMap::new();
    for (key, value) in headers.iter() {
        let key_lower = key.as_str().to_lowercase();
        let value_str = value.to_str().unwrap_or("").trim();
        canonical_headers_map.insert(key_lower, value_str);
    }
    let canonical_headers = canonical_headers_map
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect::<String>();
    let signed_headers = canonical_headers_map
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(";");

    // Hashed Payload
    let hashed_payload = hash_payload(body);

    // Combine all parts into the final canonical request string.
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        canonical_uri,
        canonical_query_string,
        canonical_headers,
        signed_headers,
        hashed_payload
    );

    (canonical_request, signed_headers)
}

/// Creates the string that will be signed.
fn create_string_to_sign(
    amz_date: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    canonical_request: &str,
) -> String {
    let algorithm = "AWS4-HMAC-SHA256";
    let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
    let hashed_canonical_request = hex_encode(Sha256::digest(canonical_request.as_bytes()));

    format!(
        "{}\n{}\n{}\n{}",
        algorithm, amz_date, credential_scope, hashed_canonical_request
    )
}

/// Derives the signing key from the secret access key through a series of HMAC operations.
fn get_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret);
    let k_date = HmacSha256::new_from_slice(k_secret.as_bytes())
        .unwrap()
        .chain_update(date.as_bytes())
        .finalize()
        .into_bytes();
    let k_region = HmacSha256::new_from_slice(&k_date)
        .unwrap()
        .chain_update(region.as_bytes())
        .finalize()
        .into_bytes();
    let k_service = HmacSha256::new_from_slice(&k_region)
        .unwrap()
        .chain_update(service.as_bytes())
        .finalize()
        .into_bytes();
    HmacSha256::new_from_slice(&k_service)
        .unwrap()
        .chain_update(b"aws4_request")
        .finalize()
        .into_bytes()
        .to_vec()
}

/// Calculates the final signature.
fn calculate_signature(
    string_to_sign: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    secret_key: &str,
) -> String {
    let signing_key = get_signing_key(secret_key, date_stamp, region, service);
    let signature_bytes = HmacSha256::new_from_slice(&signing_key)
        .unwrap()
        .chain_update(string_to_sign.as_bytes())
        .finalize()
        .into_bytes();
    hex_encode(signature_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    /// SHA-256 of the empty string — the canonical hash AWS expects for an
    /// empty payload. Pinning it guards the `hash_payload` helper.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn hashes_empty_payload() {
        assert_eq!(hash_payload(None), EMPTY_SHA256);
    }

    #[test]
    fn hashes_known_payload() {
        // SHA-256 of the bytes "hello".
        let body = reqwest::Body::from("hello");
        assert_eq!(
            hash_payload(Some(&body)),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn derives_documented_signing_key() {
        // AWS Signature V4 documented worked example
        // ("Examples of how to derive a signing key for Signature Version 4"):
        //   secret  = wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY
        //   date    = 20120215
        //   region  = us-east-1
        //   service = iam
        // The resulting signing key has a fixed, published byte sequence.
        let key = get_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        let expected: [u8; 32] = [
            0xf4, 0x78, 0x0e, 0x2d, 0x9f, 0x65, 0xfa, 0x89, 0x5f, 0x9c, 0x67, 0xb3, 0x2c, 0xe1,
            0xba, 0xf0, 0xb0, 0xd8, 0xa4, 0x35, 0x05, 0xa0, 0x00, 0xa1, 0xa9, 0xe0, 0x90, 0xd4,
            0x14, 0xdb, 0x40, 0x4d,
        ];
        assert_eq!(key, expected.to_vec());
    }

    #[test]
    fn string_to_sign_has_expected_shape() {
        let sts = create_string_to_sign(
            "20120215T000000Z",
            "20120215",
            "us-east-1",
            "iam",
            "canonical-request-placeholder",
        );
        let lines: Vec<&str> = sts.split('\n').collect();
        assert_eq!(lines[0], "AWS4-HMAC-SHA256");
        assert_eq!(lines[1], "20120215T000000Z");
        assert_eq!(lines[2], "20120215/us-east-1/iam/aws4_request");
        // Last line is the hex SHA-256 of the canonical request (64 chars).
        assert_eq!(lines[3].len(), 64);
        assert!(lines[3].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn canonical_request_normalises_empty_path_and_sorts_headers() {
        let url = Url::parse("https://example.com?b=2&a=1").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-zed", "1".parse().unwrap());
        headers.insert("x-abc", "2".parse().unwrap());
        let (canonical, signed) = create_canonical_request(&Method::POST, &url, &headers, None);

        let lines: Vec<&str> = canonical.split('\n').collect();
        assert_eq!(lines[0], "POST");
        // Empty path normalises to "/".
        assert_eq!(lines[1], "/");
        // Query string sorted by key.
        assert_eq!(lines[2], "a=1&b=2");
        // Headers are lowercased and sorted; signed-headers list matches.
        assert_eq!(signed, "x-abc;x-zed");
    }

    /// `query_pairs()` decodes; the canonical query string has to be
    /// re-encoded or the signature covers a different string than the
    /// service verifies. No endpoint this crate calls carries a query
    /// string today, which is exactly why this needs a test rather than a
    /// deployment to notice.
    #[test]
    fn canonical_query_values_are_re_encoded() {
        let url = Url::parse("https://example.com/p?b=x%20y&a=a%2Fb").unwrap();
        let (canonical, _) = create_canonical_request(&Method::GET, &url, &HeaderMap::new(), None);
        let lines: Vec<&str> = canonical.split('\n').collect();
        assert_eq!(lines[2], "a=a%2Fb&b=x%20y");
    }

    #[test]
    fn sign_request_adds_required_headers() {
        let client = Client::new();
        let mut req = client
            .post("https://bedrock-runtime.us-east-1.amazonaws.com/model/m/invoke")
            .body("{}")
            .build()
            .unwrap();
        sign_request(&mut req, "AKIDEXAMPLE", "secret", "us-east-1", "bedrock").unwrap();

        let headers = req.headers();
        assert!(headers.contains_key("host"));
        assert!(headers.contains_key("x-amz-date"));
        let auth = headers.get("Authorization").unwrap().to_str().unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
            "{auth}"
        );
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
        assert!(auth.contains("SignedHeaders="));
        assert!(auth.contains("Signature="));
    }

    /// A credential that cannot be a header value is a configuration error,
    /// never a panic: this code runs inside the PostgreSQL launcher process
    /// in embedded mode, and the access key id is operator-supplied — a key
    /// file with a stray newline is a realistic way to get here.
    #[test]
    fn an_unusable_access_key_is_a_configuration_error_not_a_panic() {
        let client = Client::new();
        let mut req = client
            .post("https://bedrock-runtime.us-east-1.amazonaws.com/model/m/invoke")
            .body("{}")
            .build()
            .unwrap();
        let err =
            sign_request(&mut req, "AKID\nEXAMPLE", "secret", "us-east-1", "bedrock").unwrap_err();
        assert!(matches!(err, EmbeddingError::Configuration(_)), "{err:?}");
        // The message names the field, never the value.
        assert!(!err.to_string().contains("AKID"), "{err}");
    }
}

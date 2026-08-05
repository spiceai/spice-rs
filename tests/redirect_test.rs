//! The API key must not follow a redirect off the origin it was configured for.
//!
//! Regression test for <https://github.com/spiceai/spiceai/issues/12502>. The SDK sends the key
//! in an `X-API-Key` header, and `reqwest` strips only the standard credential headers on a
//! cross-origin hop, so before the same-origin redirect policy a 307 would have replayed the
//! POST — header and body alike — to whatever origin the `Location` named.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "test-api-key-must-not-leak";

/// Building a client needs a Flight endpoint, but the channel connects lazily and these tests
/// only exercise the HTTP path, so nothing ever dials it.
const UNUSED_FLIGHT_URL: &str = "http://127.0.0.1:1";

fn submit_response() -> serde_json::Value {
    json!({
        "query_id": "query-1",
        "status": "PENDING",
        "status_url": "/v1/queries/query-1/status",
        "results_url": "/v1/queries/query-1/results"
    })
}

/// A 307, which preserves the method and the body, so the credentialed POST would be replayed
/// verbatim to `url` if the policy let the hop through.
fn redirect_to(url: &str) -> ResponseTemplate {
    ResponseTemplate::new(307).insert_header("location", url)
}

fn accepted() -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(submit_response())
}

async fn client_for(http_url: &str) -> spiceai::Client {
    spiceai::ClientBuilder::new()
        .http_url(http_url)
        .api_key(API_KEY)
        .flight_url(UNUSED_FLIGHT_URL)
        .build()
        .await
        .expect("client should build")
}

#[tokio::test]
async fn test_cross_origin_redirect_does_not_carry_the_api_key() {
    // A second origin, which would receive the replayed POST if the redirect were followed.
    let other_origin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/queries"))
        .respond_with(accepted())
        .mount(&other_origin)
        .await;

    let runtime = MockServer::start().await;
    let off_origin = format!("{}/v1/queries", other_origin.uri());
    Mock::given(method("POST"))
        .and(path("/v1/queries"))
        .respond_with(redirect_to(&off_origin))
        .mount(&runtime)
        .await;

    let client = client_for(&runtime.uri()).await;
    let error = client
        .query("SELECT 1")
        .await
        .expect_err("a refused redirect should not submit a query");

    // Stopping hands the 3xx back as a response, so it surfaces with its status code rather
    // than as an opaque transport failure.
    assert!(
        error.to_string().contains("307"),
        "expected the refused redirect to surface as HTTP 307, got: {error}"
    );

    // The point of the fix: the off-origin server was never contacted, so it never saw the key.
    let received = other_origin
        .received_requests()
        .await
        .expect("the mock server records requests");
    assert!(
        received.is_empty(),
        "the API key left its origin: the off-origin server received {} request(s)",
        received.len()
    );
}

#[tokio::test]
async fn test_same_origin_redirect_is_still_followed() {
    let runtime = MockServer::start().await;
    let relocated = "/v1/queries-relocated";
    let same_origin = format!("{}{relocated}", runtime.uri());

    Mock::given(method("POST"))
        .and(path("/v1/queries"))
        .respond_with(redirect_to(&same_origin))
        .mount(&runtime)
        .await;
    Mock::given(method("POST"))
        .and(path(relocated))
        .respond_with(accepted())
        .mount(&runtime)
        .await;

    let client = client_for(&runtime.uri()).await;
    let job = client
        .query("SELECT 1")
        .await
        .expect("a same-origin redirect should still be followed");

    assert_eq!(job.id(), "query-1");

    // Both hops landed on the runtime: the original path and the relocated one.
    let received = runtime
        .received_requests()
        .await
        .expect("the mock server records requests");
    assert_eq!(received.len(), 2);
}

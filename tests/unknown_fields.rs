//! Pins REPE's forward-compatibility guarantee: a structured request body that
//! carries an extra, undeclared object key decodes on a server built against an
//! older version of the struct — the unknown key is ignored, not rejected. This
//! is the "newer client, older server" version-skew case that keeps an optional
//! field from becoming a lock-step deployment.
//!
//! See `docs/protocol.md` ("Schema Evolution") and `docs/server.md`
//! ("Unknown request-body keys").

#![cfg(not(target_arch = "wasm32"))]

use repe::server::Router;
use repe::{AsyncClient, AsyncServer, ErrorCode};

/// The server was built against v1 of the request schema: it only knows `name`.
#[derive(Default)]
struct RequestV1 {
    name: String,
}
structio::object!(RequestV1 { name });

/// A newer client adds an optional field the older server has never heard of.
#[derive(Default)]
struct RequestV2 {
    name: String,
    /// Forward-looking optional field, absent from `RequestV1`.
    locale: String,
}
structio::object!(RequestV2 { name, locale });

#[derive(Default, PartialEq, Debug)]
struct Reply {
    greeting: String,
}
structio::object!(Reply { greeting });

async fn spawn(router: Router) -> String {
    let listener = AsyncServer::listen("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    tokio::spawn(async move { AsyncServer::new(router).serve(listener).await });
    addr
}

fn greet_router() -> Router {
    Router::new().with_typed::<RequestV1, Reply, _>(
        "/greet",
        |req: RequestV1| -> Result<Reply, (ErrorCode, String)> {
            Ok(Reply {
                greeting: format!("hello, {}", req.name),
            })
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_body_with_unknown_key_decodes_on_older_server() {
    let addr = spawn(greet_router()).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    // Newer client sends `name` plus an unknown `locale`; the older server must
    // decode `name` and drop `locale` rather than reject the whole request.
    let reply: Reply = client
        .call_typed_json(
            "/greet",
            &RequestV2 {
                name: "ada".into(),
                locale: "en-GB".into(),
            },
        )
        .await
        .expect("unknown JSON key must be ignored, not rejected");

    assert_eq!(
        reply,
        Reply {
            greeting: "hello, ada".into()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beve_body_with_unknown_key_decodes_on_older_server() {
    let addr = spawn(greet_router()).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    // Same guarantee over the BEVE body format.
    let reply: Reply = client
        .call_typed_beve(
            "/greet",
            &RequestV2 {
                name: "grace".into(),
                locale: "en-US".into(),
            },
        )
        .await
        .expect("unknown BEVE key must be ignored, not rejected");

    assert_eq!(
        reply,
        Reply {
            greeting: "hello, grace".into()
        }
    );
}

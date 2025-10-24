use repe::AsyncClient;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize)]
struct AddReq {
    a: i64,
    b: i64,
}

#[derive(Deserialize, Debug)]
struct AddResp {
    sum: i64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = AsyncClient::connect("127.0.0.1:8082").await?;

    let pong = client.call_json("/ping", &json!({})).await?;
    println!("/ping => {}", pong);

    let mul = client.call_json("/mul", &json!({"x": 6, "y": 7})).await?;
    println!("/mul => {}", mul);

    let sum: AddResp = client
        .call_typed_json("/add", &AddReq { a: 2, b: 3 })
        .await?;
    println!("/add => {}", sum.sum);

    client
        .notify_typed_json("/jobs/refresh", &AddReq { a: 0, b: 0 })
        .await?;
    client
        .notify_typed_beve("/jobs/refresh_beve", &AddReq { a: 1, b: 2 })
        .await?;

    Ok(())
}

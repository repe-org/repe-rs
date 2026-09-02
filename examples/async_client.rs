use repe::AsyncClient;

#[derive(Default)]
struct AddReq {
    a: i64,
    b: i64,
}
structio::object!(AddReq { a, b });

#[derive(Default, Debug)]
struct AddResp {
    sum: i64,
}
structio::object!(AddResp { sum });

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = AsyncClient::connect("127.0.0.1:8082").await?;

    let pong = client.call_typed_json("/ping", &json!({})).await?;
    println!("/ping => {}", pong);

    let mul = client
        .call_typed_json("/mul", &json!({"x": 6, "y": 7}))
        .await?;
    println!("/mul => {}", mul);

    let sum: AddResp = client
        .call_typed_json("/add", &AddReq { a: 2, b: 3 })
        .await?;
    println!("/add => {}", sum.sum);

    client
        .notify_json("/jobs/refresh", &AddReq { a: 0, b: 0 })
        .await?;
    client
        .notify_beve("/jobs/refresh_beve", &AddReq { a: 1, b: 2 })
        .await?;

    Ok(())
}

use repe::AsyncClient;

#[derive(Default, Debug)]
struct Empty;
structio::object!(Empty {});

#[derive(Default, Debug)]
struct Pong {
    pong: bool,
}
structio::object!(Pong { pong });

#[derive(Default, Debug)]
struct Factors {
    x: i64,
    y: i64,
}
structio::object!(Factors { x, y });

#[derive(Default, Debug)]
struct Product {
    product: i64,
}
structio::object!(Product { product });

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

    let pong: Pong = client.call_typed_json("/ping", &Empty).await?;
    println!("/ping => pong={}", pong.pong);

    let mul: Product = client
        .call_typed_json("/mul", &Factors { x: 6, y: 7 })
        .await?;
    println!("/mul => {}", mul.product);

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

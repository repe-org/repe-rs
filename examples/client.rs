use repe::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect("127.0.0.1:8081")?;

    let pong = client.call_typed_json("/ping", &json!({}))?;
    println!("/ping => {}", pong);

    let echo = client.call_typed_json("/echo", &json!({"msg":"hello"}))?;
    println!("/echo => {}", echo);

    let status = client.call_typed_json("/status", &json!({}))?;
    println!("/status => {}", status);

    #[derive(Default)]
    struct AddReq {
        a: i64,
        b: i64,
    }
    structio::object!(AddReq { a, b });

    #[derive(Default)]
    struct AddResp {
        sum: i64,
    }
    structio::object!(AddResp { sum });

    let add: AddResp = client.call_typed_json("/add", &AddReq { a: 4, b: 5 })?;
    println!("/add => {}", add.sum);

    Ok(())
}

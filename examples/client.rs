use repe::Client;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::connect("127.0.0.1:8081")?;

    let pong = client.call_json("/ping", &json!({}))?;
    println!("/ping => {}", pong);

    let echo = client.call_json("/echo", &json!({"msg":"hello"}))?;
    println!("/echo => {}", echo);

    let status = client.call_json("/status", &json!({}))?;
    println!("/status => {}", status);

    Ok(())
}

use repe::{BodyFormat, Message, QueryFormat};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let msg = Message::builder()
        .id(1)
        .query_str("/echo")
        .query_format(QueryFormat::JsonPointer)
        .body_json(&serde_json::json!({"hello": "world"}))?
        .build();

    let bytes = msg.to_vec();
    let parsed = Message::from_slice(&bytes)?;
    assert_eq!(parsed.header.id, 1);
    assert_eq!(parsed.header.query_length as usize, "/echo".len());
    assert_eq!(parsed.header.body_format, BodyFormat::Json as u16);
    let v: serde_json::Value = parsed.json_body()?;
    assert_eq!(v["hello"], "world");
    println!("Roundtrip OK: {} bytes", bytes.len());
    Ok(())
}

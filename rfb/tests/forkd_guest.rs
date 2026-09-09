#![cfg(feature = "forkd")]

use rfb::forkd_guest::{
    ForkdGuestClient, ForkdGuestError, MAX_GUEST_RESULTS, MAX_GUEST_RESULT_BYTES,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

async fn server(response: String) -> (String, tokio::task::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (read, mut write) = socket.into_split();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        write.write_all(response.as_bytes()).await.unwrap();
        write.flush().await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    });
    (address, task)
}

#[tokio::test]
async fn request_sends_ndjson_and_collects_events_until_terminal_response() {
    let (address, task) = server("{\"event\":\"started\"}\n{\"data\":\"ok\"}\n".into()).await;
    let client = ForkdGuestClient::new(address);
    let responses = client.request(json!({"action":"ping"})).await.unwrap();
    assert_eq!(
        responses,
        vec![json!({"event":"started"}), json!({"data":"ok"})]
    );
    assert_eq!(task.await.unwrap(), json!({"action":"ping"}));
}

#[tokio::test]
async fn remote_error_is_returned_from_ndjson_response() {
    let (address, _) = server("{\"error\":\"permission denied\"}\n".into()).await;
    let error = ForkdGuestClient::new(address)
        .request(json!({"action":"ls"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkdGuestError::Remote(message) if message == "permission denied"));
}

#[tokio::test]
async fn exec_and_eval_preserve_opaque_guest_paths() {
    let (address, task) = server("{\"ok\":true}\n".into()).await;
    let client = ForkdGuestClient::new(address);
    client
        .exec_in("C:\\guest\\work", vec!["printf".into(), "x".into()], 7)
        .await
        .unwrap();
    let request = task.await.unwrap();
    assert_eq!(request["cwd"], "C:\\guest\\work");
    assert_eq!(request["timeout"], 7);

    let (address, task) = server("{\"ok\":true}\n".into()).await;
    ForkdGuestClient::new(address)
        .eval_in("/guest/opaque", "1 + 1")
        .await
        .unwrap();
    let request = task.await.unwrap();
    assert_eq!(
        request,
        json!({"action":"eval","cwd":"/guest/opaque","code":"1 + 1"})
    );
}

#[tokio::test]
async fn tool_result_count_and_encoded_size_limits_are_enforced() {
    let results = vec![json!(null); MAX_GUEST_RESULTS + 1];
    let (address, _) =
        server(serde_json::to_string(&json!({"results":results})).unwrap() + "\n").await;
    let error = ForkdGuestClient::new(address)
        .execute_tool("ls", json!({"path":"."}))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkdGuestError::LimitExceeded));

    let oversized = "x".repeat(MAX_GUEST_RESULT_BYTES + 1);
    let (address, _) =
        server(serde_json::to_string(&json!({"data":oversized})).unwrap() + "\n").await;
    let error = ForkdGuestClient::new(address)
        .execute_tool("ls", json!({"path":"."}))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkdGuestError::TooLarge));
}

#[tokio::test]
async fn oversized_ndjson_line_is_rejected() {
    let (address, _) =
        server(format!("{{\"data\":\"{}\"}}\n", "x".repeat(1024 * 1024)).to_string()).await;
    let error = ForkdGuestClient::new(address)
        .request(json!({"action":"ping"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkdGuestError::TooLarge));
}

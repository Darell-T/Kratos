use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

use kratos_client::{ApiRequest, Client};
use serde_json::json;

const FORBIDDEN: &str = "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\n\
                         Content-Length: 21\r\nConnection: close\r\n\r\n{\"error\":\"Forbidden\"}";

fn serve_once(reply: &'static str) -> (Client, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        let request = read_request(&mut connection);
        connection.write_all(reply.as_bytes()).unwrap();
        request
    });
    (Client::new(&format!("http://{authority}")).unwrap(), server)
}

fn read_request(connection: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = connection.read(&mut chunk).unwrap();
        assert!(read > 0, "the client closed before it finished the request");
        request.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&request).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")?
                        .parse()
                        .ok()
                })
                .unwrap_or(0);
            if body.len() >= length {
                return request;
            }
        }
    }
}

#[test]
fn sends_the_request_as_given_and_adds_only_what_is_missing() {
    // Catches a client that repairs the request, for example by replacing a foreign Host, which
    // would make the engine's Host and Origin lanes pass without ever sending what they claim to
    // send.
    let (client, server) = serve_once(FORBIDDEN);
    let request = ApiRequest::new("POST", "/api/workspaces")
        .header("Host", "evil.example")
        .header("Origin", "http://evil.example")
        .body(b"{}".to_vec());

    let reply = client.send(&request).unwrap();

    let expected = "POST /api/workspaces HTTP/1.1\r\nHost: evil.example\r\nOrigin: http://evil.example\r\n\
                    Content-Length: 2\r\nConnection: close\r\n\r\n{}";
    assert_eq!(String::from_utf8(server.join().unwrap()).unwrap(), expected);
    assert_eq!(
        (reply.status, reply.body),
        (403, json!({ "error": "Forbidden" }))
    );

    let (client, server) = serve_once(FORBIDDEN);
    client.get("/api/health").unwrap();
    let sent = String::from_utf8(server.join().unwrap()).unwrap();
    let host = host_header(&sent);
    assert_eq!(
        sent,
        format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
    );
}

fn host_header(request: &str) -> &str {
    request
        .lines()
        .find_map(|line| line.strip_prefix("Host: "))
        .unwrap()
}

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use kratos_api_client::{ApiRequest, Client};
use kratos_protocol::{CLIENT_TOKEN_FILE, ThreadId};
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
        if let Some(head_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..head_end]);
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")?
                        .parse()
                        .ok()
                })
                .unwrap_or(0);
            if request.len() - head_end - 4 >= length {
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

struct TokenHome(PathBuf);

impl TokenHome {
    fn new() -> TokenHome {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "kratos-api-client-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return TokenHome(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("cannot create token directory {}: {error}", path.display()),
            }
        }
    }
}

impl Drop for TokenHome {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "cannot remove token directory {}: {error}",
                self.0.display()
            );
        }
    }
}

#[test]
fn default_credentials_follow_given_headers_without_changing_a_reused_binary_request() {
    let home = TokenHome::new();
    fs::write(home.0.join(CLIENT_TOKEN_FILE), "  current-token \r\n").unwrap();
    let (client, server) = serve_once(FORBIDDEN);
    let client = client
        .with_token_from(&home.0)
        .acting_for(ThreadId::new("default-thread".to_owned()));
    let request = ApiRequest::new("POST", "/raw//bytes?x=%2F")
        .header("X-Trace", "first")
        .header("Host", "manual.host")
        .body(b"\0\xff\x80\r\n".to_vec());

    client.send(&request).unwrap();

    assert_eq!(
        server.join().unwrap(),
        b"POST /raw//bytes?x=%2F HTTP/1.1\r\nX-Trace: first\r\nHost: manual.host\r\n\
          Authorization: Bearer current-token\r\nKratos-Thread: default-thread\r\n\
          Content-Length: 5\r\nConnection: close\r\n\r\n\0\xff\x80\r\n"
    );

    let (bare, server) = serve_once(FORBIDDEN);
    bare.send(&request).unwrap();

    assert_eq!(
        server.join().unwrap(),
        b"POST /raw//bytes?x=%2F HTTP/1.1\r\nX-Trace: first\r\nHost: manual.host\r\n\
          Content-Length: 5\r\nConnection: close\r\n\r\n\0\xff\x80\r\n"
    );
}

#[test]
fn mixed_case_explicit_headers_keep_their_order_duplicates_and_raw_values() {
    let home = TokenHome::new();
    fs::write(home.0.join(CLIENT_TOKEN_FILE), "default-token").unwrap();
    let (client, server) = serve_once(FORBIDDEN);
    let client = client
        .with_token_from(&home.0)
        .acting_for(ThreadId::new("default-thread".to_owned()));
    let request = ApiRequest::new("pOsT", "/raw//empty?name=%2F")
        .header("hOsT", "foreign.example")
        .header("aUtHoRiZaTiOn", "Bearer explicit-one")
        .header("AUTHORIZATION", "Bearer explicit-two")
        .header("kRaToS-tHrEaD", "explicit-thread")
        .header("Kratos-Thread", "duplicate-thread")
        .header("cOnTeNt-LeNgTh", "0")
        .header("cOnNeCtIoN", "keep-alive")
        .header("X-Raw", "first\r\nX-Injected: second")
        .body(Vec::new());

    client.send(&request).unwrap();

    assert_eq!(
        server.join().unwrap(),
        b"pOsT /raw//empty?name=%2F HTTP/1.1\r\nhOsT: foreign.example\r\n\
          aUtHoRiZaTiOn: Bearer explicit-one\r\nAUTHORIZATION: Bearer explicit-two\r\n\
          kRaToS-tHrEaD: explicit-thread\r\nKratos-Thread: duplicate-thread\r\n\
          cOnTeNt-LeNgTh: 0\r\ncOnNeCtIoN: keep-alive\r\n\
          X-Raw: first\r\nX-Injected: second\r\n\r\n"
    );
}

#[test]
fn file_tokens_reload_while_pinned_tokens_keep_their_original_presence_and_value() {
    let home = TokenHome::new();
    let token_file = home.0.join(CLIENT_TOKEN_FILE);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = Client::new(&format!("http://{}", listener.local_addr().unwrap()))
        .unwrap()
        .with_token_from(&home.0);
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..7 {
            let (mut connection, _) = listener.accept().unwrap();
            requests.push(read_request(&mut connection));
            connection.write_all(FORBIDDEN.as_bytes()).unwrap();
        }
        requests
    });
    let request = ApiRequest::new("GET", "/token").header("Host", "manual.host");
    let pinned_missing = client.clone().with_current_token().unwrap();
    fs::write(&token_file, "first-token").unwrap();
    let pinned = client.clone().with_current_token().unwrap();

    client.send(&request).unwrap();
    fs::write(&token_file, " second-token \n").unwrap();
    client.send(&request).unwrap();
    pinned.send(&request).unwrap();
    pinned_missing.send(&request).unwrap();
    fs::write(&token_file, " \r\n").unwrap();
    client.send(&request).unwrap();
    fs::remove_file(&token_file).unwrap();
    client.send(&request).unwrap();
    pinned.send(&request).unwrap();

    let first = b"GET /token HTTP/1.1\r\nHost: manual.host\r\n\
                  Authorization: Bearer first-token\r\nConnection: close\r\n\r\n"
        .to_vec();
    let second = b"GET /token HTTP/1.1\r\nHost: manual.host\r\n\
                   Authorization: Bearer second-token\r\nConnection: close\r\n\r\n"
        .to_vec();
    let absent = b"GET /token HTTP/1.1\r\nHost: manual.host\r\nConnection: close\r\n\r\n".to_vec();
    assert_eq!(
        server.join().unwrap(),
        [
            first.clone(),
            second,
            first.clone(),
            absent.clone(),
            absent.clone(),
            absent,
            first
        ]
    );
}

fn host_header(request: &str) -> &str {
    request
        .lines()
        .find_map(|line| line.strip_prefix("Host: "))
        .unwrap()
}

#[test]
fn unreadable_token_errors_name_the_file_for_http_and_sockets() {
    // Catches unreadable credentials silently becoming an unauthenticated request.
    let home = TokenHome::new();
    let token_file = home.0.join(CLIENT_TOKEN_FILE);
    let client = Client::new("http://127.0.0.1:9")
        .unwrap()
        .with_token_from(&home.0);
    fs::create_dir(&token_file).unwrap();
    for error in [
        client.get("/api/workspaces").unwrap_err(),
        client.connect_events(&[]).err().unwrap(),
        client.clone().with_current_token().unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains(&token_file.display().to_string()),
            "the token read error must name its file: {error}"
        );
        assert!(std::error::Error::source(&error).is_some());
    }
    fs::remove_dir(&token_file).unwrap();
    fs::write(&token_file, [0xff]).unwrap();
    let error = client.get("/api/workspaces").unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&token_file.display().to_string())
    );
}

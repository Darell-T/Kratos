//! The commands, as a table: adding a command is adding a row.
//!
//! Every command prints its answer as JSON lines on stdout. A reply prints as
//! `{"status":201,"body":{…}}` even when the status is 4xx or 5xx, because lanes assert on
//! refusals. The create commands send their arguments unchecked, so a lane can send blank or
//! relative values.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use kratos_client::{
    ApiReply, ApiRequest, Client, ClientError, Followed, Incoming, Normalizer, bench_post,
    bench_turns,
};
use kratos_protocol::{Seq, ServerMessage, ThreadId};
use serde_json::{Value, json};

pub const USAGE: &str = "\
usage: kratos-client [--url <url>] [--normalize] <command>

  health
  workspaces
  create-workspace <name> <path>
  threads <workspace-id>
  create-thread <workspace-id> <title> <provider>
  post <thread-id> <text>
  send <thread-id> <text> [--message-id <id>] [--image <path>]... [--file <path>]...
  thread <thread-id>
  steer <thread-id> <message-id>
  remove <thread-id> <message-id>
  retry <thread-id> <message-id>
  discard <thread-id> <message-id>
  stop <thread-id>
  resume <thread-id>
  settings <thread-id> <settings-json>
  skills <thread-id>
  approve <approval-id> <accept|acceptForSession|decline|cancel>
  answer <question-id> <answers-json>
  schedule <continuation-id>
  usage
  subscribe <thread-id> [--after <seq>] [--count <n>] [--read-delay-ms <ms>]
  request <METHOD> <path> [--header <name:value>]... [--json <body> | --body-bytes <n>]
  socket-send <bytes> [--header <name:value>]...
  bench-post <url>
  bench-turns <fake-control-file>

The engine is --url, else $KRATOS_URL, else http://127.0.0.1:4200. Requests carry the token the
engine wrote to $KRATOS_HOME/client-token (default ~/.kratos/client-token), when there is one.
--normalize replaces ids and times, so two engines' output compares with a plain diff.
subscribe follows a thread through disconnects and engine restarts and prints each event once,
and live updates, such as a reply as it is written, as they come; --count counts events only.
--read-delay-ms makes it a stalled reader: it subscribes, waits, then reads.
request sends exactly what it is told, for the Host, Origin, and size-cap checks.
socket-send sends one message of exactly <bytes> bytes and reports whether the engine closed
the socket.";

const DEFAULT_URL: &str = "http://127.0.0.1:4200";
/// `subscribe` gives up when the engine sends nothing for this long.
const SUBSCRIBE_WAIT: Duration = Duration::from_secs(60);
/// `socket-send` waits this long to learn whether the engine closes the socket.
const SOCKET_SEND_WAIT: Duration = Duration::from_secs(2);
const BENCH_MESSAGES: NonZeroUsize = NonZeroUsize::new(500).unwrap();

pub enum Failure {
    /// The command line is wrong. Nothing was sent.
    Usage(String),
    Client(ClientError),
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Failure {
        Failure::Client(error)
    }
}

fn usage<T>(message: impl Into<String>) -> Result<T, Failure> {
    Err(Failure::Usage(message.into()))
}

struct Command {
    name: &'static str,
    positional: usize,
    options: &'static [&'static str],
    run: fn(&mut Session, &Args) -> Result<(), Failure>,
}

#[rustfmt::skip]
const COMMANDS: [Command; 25] = [
    Command { name: "health", positional: 0, options: &[], run: health },
    Command { name: "workspaces", positional: 0, options: &[], run: workspaces },
    Command { name: "create-workspace", positional: 2, options: &[], run: create_workspace },
    Command { name: "threads", positional: 1, options: &[], run: threads },
    Command { name: "create-thread", positional: 3, options: &[], run: create_thread },
    Command { name: "post", positional: 2, options: &[], run: post },
    Command {
        name: "send", positional: 2, options: &["--message-id", "--image", "--file"], run: send,
    },
    Command { name: "thread", positional: 1, options: &[], run: thread_details },
    Command { name: "steer", positional: 2, options: &[], run: steer },
    Command { name: "remove", positional: 2, options: &[], run: remove },
    Command { name: "retry", positional: 2, options: &[], run: retry },
    Command { name: "discard", positional: 2, options: &[], run: discard },
    Command { name: "stop", positional: 1, options: &[], run: stop },
    Command { name: "resume", positional: 1, options: &[], run: resume },
    Command { name: "settings", positional: 2, options: &[], run: settings },
    Command { name: "skills", positional: 1, options: &[], run: skills },
    Command { name: "approve", positional: 2, options: &[], run: approve },
    Command { name: "answer", positional: 2, options: &[], run: answer },
    Command { name: "schedule", positional: 1, options: &[], run: schedule },
    Command { name: "usage", positional: 0, options: &[], run: usage_report },
    Command {
        name: "subscribe", positional: 1,
        options: &["--after", "--count", "--read-delay-ms"], run: subscribe,
    },
    Command {
        name: "request", positional: 2,
        options: &["--header", "--json", "--body-bytes"], run: request,
    },
    Command { name: "socket-send", positional: 1, options: &["--header"], run: socket_send },
    Command { name: "bench-post", positional: 1, options: &[], run: bench },
    Command { name: "bench-turns", positional: 1, options: &[], run: turns_bench },
];

pub fn run(arguments: &[String]) -> Result<(), Failure> {
    let (name, args) = Args::parse(arguments)?;
    let Some(name) = name else {
        return usage("no command given");
    };
    let Some(command) = COMMANDS.iter().find(|command| command.name == name) else {
        return usage(format!("unknown command {name:?}"));
    };
    args.check(command)?;
    let url = args
        .option("--url")
        .map(str::to_owned)
        .or_else(|| std::env::var("KRATOS_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_owned());
    let mut session = Session {
        client: with_token(Client::new(&url)?),
        normalizer: args.normalize.then(Normalizer::default),
    };
    (command.run)(&mut session, &args)
}

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    /// `--name value` pairs in the order given, so `--header` may repeat.
    options: Vec<(String, String)>,
    normalize: bool,
}

impl Args {
    fn parse(arguments: &[String]) -> Result<(Option<String>, Args), Failure> {
        let mut args = Args::default();
        let mut command = None;
        let mut remaining = arguments.iter();
        while let Some(argument) = remaining.next() {
            if argument == "--normalize" {
                args.normalize = true;
            } else if argument.starts_with("--") {
                let Some(value) = remaining.next() else {
                    return usage(format!("{argument} needs a value"));
                };
                args.options.push((argument.clone(), value.clone()));
            } else if command.is_none() {
                command = Some(argument.clone());
            } else {
                args.positional.push(argument.clone());
            }
        }
        Ok((command, args))
    }

    fn check(&self, command: &Command) -> Result<(), Failure> {
        if self.positional.len() != command.positional {
            let wanted = command.positional;
            return usage(format!(
                "{} takes {wanted} arguments, got {}",
                command.name,
                self.positional.len()
            ));
        }
        let unknown = self
            .options
            .iter()
            .map(|(name, _)| name.as_str())
            .find(|name| *name != "--url" && !command.options.contains(name));
        match unknown {
            Some(name) => usage(format!("{} does not take {name}", command.name)),
            None => Ok(()),
        }
    }

    fn option<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        self.options_named(name).last()
    }

    fn options_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.options
            .iter()
            .filter(move |(given, _)| given == name)
            .map(|(_, value)| value.as_str())
    }

    fn number<T: FromStr>(&self, name: &str) -> Result<Option<T>, Failure> {
        let Some(text) = self.option(name) else {
            return Ok(None);
        };
        match text.parse() {
            Ok(number) => Ok(Some(number)),
            Err(_) => usage(format!("{name} needs a whole number, got {text:?}")),
        }
    }
}

struct Session {
    client: Client,
    normalizer: Option<Normalizer>,
}

impl Session {
    fn get(&mut self, path: &str) -> Result<(), Failure> {
        let reply = self.client.get(path)?;
        self.print_reply(reply);
        Ok(())
    }

    fn post(&mut self, path: &str, body: &Value) -> Result<(), Failure> {
        let reply = self.client.post(path, body)?;
        self.print_reply(reply);
        Ok(())
    }

    fn print_reply(&mut self, mut reply: ApiReply) {
        self.normalize(&mut reply.body);
        println!("{}", reply.to_json_line());
    }

    fn print(&mut self, mut line: Value) {
        self.normalize(&mut line);
        println!("{line}");
    }

    fn normalize(&mut self, value: &mut Value) {
        if let Some(normalizer) = &mut self.normalizer {
            normalizer.apply(value);
        }
    }
}

fn health(session: &mut Session, _: &Args) -> Result<(), Failure> {
    session.get("/api/health")
}

fn workspaces(session: &mut Session, _: &Args) -> Result<(), Failure> {
    session.get("/api/workspaces")
}

fn create_workspace(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let [name, path] = [0, 1].map(|index| &args.positional[index]);
    session.post("/api/workspaces", &json!({ "name": name, "path": path }))
}

fn threads(session: &mut Session, args: &Args) -> Result<(), Failure> {
    session.get(&format!(
        "/api/threads?workspaceId={}",
        encode(&args.positional[0])
    ))
}

fn create_thread(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let [workspace_id, title, provider] = [0, 1, 2].map(|index| &args.positional[index]);
    let body = json!({ "workspaceId": workspace_id, "title": title, "provider": provider });
    session.post("/api/threads", &body)
}

fn post(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/threads/{}/messages", encode(&args.positional[0]));
    session.post(&path, &json!({ "text": args.positional[1] }))
}

/// Text alone goes as `text`; with attachments, as `input` parts, the text first (D86).
fn send(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/threads/{}/messages", encode(&args.positional[0]));
    let text = &args.positional[1];
    let attachments: Vec<Value> = args
        .options_named("--image")
        .map(|path| json!({ "type": "image", "path": path }))
        .chain(
            args.options_named("--file")
                .map(|path| json!({ "type": "file", "path": path })),
        )
        .collect();
    let mut body = if attachments.is_empty() {
        json!({ "text": text })
    } else {
        let parts = std::iter::once(json!({ "type": "text", "text": text })).chain(attachments);
        json!({ "input": parts.collect::<Vec<_>>() })
    };
    if let Some(message_id) = args.option("--message-id") {
        body["messageId"] = json!(message_id);
    }
    session.post(&path, &body)
}

fn thread_details(session: &mut Session, args: &Args) -> Result<(), Failure> {
    session.get(&format!("/api/threads/{}", encode(&args.positional[0])))
}

fn message_path(args: &Args, action: &str) -> String {
    let [thread_id, message_id] = [0, 1].map(|index| encode(&args.positional[index]));
    format!("/api/threads/{thread_id}/messages/{message_id}{action}")
}

fn steer(session: &mut Session, args: &Args) -> Result<(), Failure> {
    session.post(&message_path(args, "/steer"), &json!({}))
}

fn remove(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let reply = session.client.delete(&message_path(args, ""))?;
    session.print_reply(reply);
    Ok(())
}

fn retry(session: &mut Session, args: &Args) -> Result<(), Failure> {
    session.post(&message_path(args, "/retry"), &json!({}))
}

fn discard(session: &mut Session, args: &Args) -> Result<(), Failure> {
    session.post(&message_path(args, "/discard"), &json!({}))
}

fn stop(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/threads/{}/stop", encode(&args.positional[0]));
    session.post(&path, &json!({}))
}

fn resume(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/threads/{}/resume", encode(&args.positional[0]));
    session.post(&path, &json!({}))
}

fn settings(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/threads/{}/settings", encode(&args.positional[0]));
    let body = json_argument(&args.positional[1], "settings-json")?;
    let reply = session.client.put(&path, &body)?;
    session.print_reply(reply);
    Ok(())
}

fn skills(session: &mut Session, args: &Args) -> Result<(), Failure> {
    session.get(&format!(
        "/api/threads/{}/skills",
        encode(&args.positional[0])
    ))
}

fn approve(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/approvals/{}", encode(&args.positional[0]));
    session.post(&path, &json!({ "decision": args.positional[1] }))
}

fn answer(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!("/api/questions/{}", encode(&args.positional[0]));
    let answers = json_argument(&args.positional[1], "answers-json")?;
    session.post(&path, &json!({ "answers": answers }))
}

fn schedule(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let path = format!(
        "/api/continuations/{}/schedule",
        encode(&args.positional[0])
    );
    session.post(&path, &json!({}))
}

fn usage_report(session: &mut Session, _: &Args) -> Result<(), Failure> {
    session.get("/api/usage")
}

fn json_argument(text: &str, name: &str) -> Result<Value, Failure> {
    serde_json::from_str(text).or_else(|_| usage(format!("{name} must be JSON, got {text:?}")))
}

/// `client` with the token of the engine on `$KRATOS_HOME`, else on `.kratos` in the user's home
/// folder.
fn with_token(client: Client) -> Client {
    let home = std::env::var_os("KRATOS_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".kratos")));
    match home {
        Some(home) => client.with_token_from(&home),
        None => client,
    }
}

fn subscribe(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let thread_id = ThreadId::new(args.positional[0].clone());
    let after = Seq::new(args.number("--after")?.unwrap_or(0));
    let count: usize = args.number("--count")?.unwrap_or(usize::MAX);
    let mut follower = session.client.follow(thread_id.clone(), after);
    follower.connect()?;
    if let Some(milliseconds) = args.number("--read-delay-ms")? {
        thread::sleep(Duration::from_millis(milliseconds));
    }
    let mut printed = 0;
    while printed < count {
        match follower.next(SUBSCRIBE_WAIT) {
            Ok(Followed::Event(event)) => {
                printed += 1;
                session.print(message_line(&ServerMessage::Event { event }));
            }
            Ok(Followed::Live(update)) => {
                let thread_id = thread_id.clone();
                session.print(message_line(&ServerMessage::Live { thread_id, update }));
            }
            Err(ClientError::Engine(message)) => {
                session.print(message_line(&ServerMessage::Error {
                    message: message.clone(),
                }));
                return Err(ClientError::Engine(message).into());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn request(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let mut request = ApiRequest::new(&args.positional[0], &args.positional[1]);
    for header in args.options_named("--header") {
        let (name, value) = split_header(header)?;
        request = request.header(name, value);
    }
    let body = match (args.option("--json"), args.number::<usize>("--body-bytes")?) {
        (Some(_), Some(_)) => return usage("request takes --json or --body-bytes, not both"),
        (Some(json), None) => Some(json.as_bytes().to_vec()),
        (None, Some(size)) => Some(filler_body(size)?),
        (None, None) => None,
    };
    if let Some(body) = body {
        request = request.body(body);
    }
    let reply = session.client.send(&request)?;
    session.print_reply(reply);
    Ok(())
}

fn socket_send(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let Ok(size) = args.positional[0].parse::<usize>() else {
        return usage(format!(
            "socket-send needs a number of bytes, got {:?}",
            args.positional[0]
        ));
    };
    let mut headers = Vec::new();
    for header in args.options_named("--header") {
        headers.push(split_header(header)?);
    }
    let mut socket = session.client.connect_events(&headers)?;
    // The engine may close the socket before it has read the whole message, which fails the send.
    // What the socket does next is the answer, so the send's own result is not.
    let _ = socket.send_text(padded_message(size)?);
    let line = match socket.next_incoming(SOCKET_SEND_WAIT) {
        Ok(Some(Incoming::Closed(code))) => json!({ "socket": "closed", "code": code }),
        Ok(Some(Incoming::Text(text))) => json!({ "socket": "open", "text": text }),
        Ok(None) => json!({ "socket": "open" }),
        Err(error) => json!({ "socket": "closed", "code": null, "error": error.to_string() }),
    };
    session.print(line);
    Ok(())
}

fn bench(_: &mut Session, args: &Args) -> Result<(), Failure> {
    let client = with_token(Client::new(&args.positional[0])?);
    let report = bench_post(&client, BENCH_MESSAGES)?;
    println!(
        "{}",
        serde_json::to_string(&report).expect("a bench report serializes")
    );
    Ok(())
}

fn turns_bench(session: &mut Session, args: &Args) -> Result<(), Failure> {
    let report = bench_turns(&session.client, std::path::Path::new(&args.positional[0]))?;
    session.print(serde_json::to_value(report).expect("a bench report serializes"));
    Ok(())
}

fn message_line(message: &ServerMessage) -> Value {
    serde_json::to_value(message).expect("a server message serializes")
}

fn split_header(header: &str) -> Result<(&str, &str), Failure> {
    match header.split_once(':') {
        Some((name, value)) => Ok((name.trim(), value.trim())),
        None => usage(format!("a header looks like name:value, got {header:?}")),
    }
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// A JSON body of exactly `size` bytes, `{"text":"xxx…"}`, for probing the engine's body cap.
fn filler_body(size: usize) -> Result<Vec<u8>, Failure> {
    const EMPTY: &str = r#"{"text":""}"#;
    let Some(padding) = size.checked_sub(EMPTY.len()) else {
        return usage(format!("--body-bytes must be at least {}", EMPTY.len()));
    };
    Ok(format!(r#"{{"text":"{}"}}"#, "x".repeat(padding)).into_bytes())
}

/// An `unsubscribe` message padded with spaces to exactly `size` bytes. It stays valid JSON that
/// the engine accepts and ignores, so only its size can get it refused.
fn padded_message(size: usize) -> Result<String, Failure> {
    const MESSAGE: &str = r#"{"type":"unsubscribe","threadId":"padding"}"#;
    let Some(padding) = size.checked_sub(MESSAGE.len()) else {
        return usage(format!(
            "socket-send needs at least {} bytes",
            MESSAGE.len()
        ));
    };
    Ok(format!("{MESSAGE}{}", " ".repeat(padding)))
}

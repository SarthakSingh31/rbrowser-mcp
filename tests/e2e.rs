//! End-to-end tests that drive the server binary over stdio. They need Chrome installed, so they
//! are ignored by default: run with `cargo test -- --ignored`.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread::sleep,
    time::Duration,
};

use serde_json::{Value, json};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rbrowser-mcp"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "0"}
            }),
        );
        server.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        server
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert_ne!(
                self.stdout.read_line(&mut line).unwrap(),
                0,
                "server exited"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// Call a tool, returning its text output and whether it was an error.
    fn call(&mut self, name: &str, args: Value) -> (String, bool) {
        let response = self.request("tools/call", json!({"name": name, "arguments": args}));
        let result = &response["result"];
        let text = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().unwrap_or("<image>").to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    fn ok(&mut self, name: &str, args: Value) -> String {
        let (text, is_error) = self.call(name, args);
        assert!(!is_error, "{name} failed: {text}");
        text
    }

    /// Chrome browser processes started by this server, as (headless) flags.
    fn browsers(&self) -> Vec<bool> {
        let out = Command::new("pgrep")
            .args(["-a", "-P", &self.child.id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("--user-data-dir"))
            .map(|l| l.contains("--headless"))
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "needs Chrome"]
fn pages_are_lazy_separate_and_closable() {
    let mut s = Server::start(&[]);
    sleep(Duration::from_millis(500));
    assert!(s.browsers().is_empty(), "no browser before open_page");

    let opened = s.ok(
        "open_page",
        json!({"url": "data:text/html,<title>One</title>1"}),
    );
    assert!(opened.contains("p1") && opened.contains("close_page"));
    s.ok(
        "open_page",
        json!({"url": "data:text/html,<title>Two</title>2"}),
    );
    assert_eq!(s.browsers(), vec![true], "one headless browser");

    let list = s.ok("list_pages", json!({}));
    assert!(
        list.contains("\"One\"") && list.contains("\"Two\""),
        "{list}"
    );

    s.ok("close_page", json!({"page_id": "p1"}));
    let (text, is_error) = s.call("screenshot", json!({"page_id": "p1"}));
    assert!(is_error && text.contains("p1"));

    s.ok("close_page", json!({"page_id": "p2"}));
    sleep(Duration::from_millis(500));
    assert!(s.browsers().is_empty(), "browser closed with its last page");
}

#[test]
#[ignore = "needs Chrome"]
fn evaluate_reports_values_and_exceptions() {
    let mut s = Server::start(&[]);
    s.ok("open_page", json!({"url": "data:text/html,<h1>Hi</h1>"}));

    let ua = s.ok(
        "evaluate",
        json!({"page_id": "p1", "script": "navigator.userAgent"}),
    );
    assert!(!ua.contains("Headless"), "{ua}");
    assert_eq!(
        s.ok("evaluate", json!({"page_id": "p1", "script": "1 + 41"})),
        "42"
    );
    let obj = s.ok(
        "evaluate",
        json!({"page_id": "p1", "script": "({h1: document.querySelector('h1').textContent})"}),
    );
    assert_eq!(obj, r#"{"h1":"Hi"}"#);

    let (text, is_error) = s.call(
        "evaluate",
        json!({"page_id": "p1", "script": "throw new Error('boom')"}),
    );
    assert!(is_error && text.contains("boom"), "{text}");
}

#[test]
#[ignore = "needs Chrome and a display"]
fn show_and_hide_move_page_between_browsers() {
    let mut s = Server::start(&[]);
    // A real origin is needed for cookies and localStorage.
    s.ok("open_page", json!({"url": "https://example.com"}));
    s.ok(
        "evaluate",
        json!({"page_id": "p1", "script": "document.cookie = 'sess=abc; max-age=3600'; localStorage.setItem('k', 'v')"}),
    );

    s.ok("show_page", json!({"page_id": "p1"}));
    sleep(Duration::from_millis(500));
    assert_eq!(s.browsers(), vec![false], "only a visible browser");
    let state = s.ok(
        "evaluate",
        json!({"page_id": "p1", "script": "({cookie: document.cookie, ls: localStorage.getItem('k')})"}),
    );
    assert_eq!(state, r#"{"cookie":"sess=abc","ls":"v"}"#);

    s.ok("hide_page", json!({"page_id": "p1"}));
    sleep(Duration::from_millis(500));
    assert_eq!(s.browsers(), vec![true], "only a headless browser");
    assert!(s.ok("list_pages", json!({})).contains("[headless]"));
}

//! `delldisplay mcp` over real stdio: handshake, tool list, one call.
//!
//! Display 99 never exists, so this never touches a monitor, and a tool call
//! comes back as a tool error rather than a protocol error.
#![cfg(feature = "mcp")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn handshake_list_and_call() {
    let dir = std::env::temp_dir().join(format!("delldisplay-mcp-stdio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("mcp.toml");
    std::fs::write(&config, "cooldown_seconds = 0\nnotify = false\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_delldisplay"))
        .args(["--display", "99", "mcp", "--config"])
        .arg(&config)
        .env("XDG_STATE_HOME", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    let mut recv = || -> Value {
        let line = lines.next().expect("a reply").unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON-RPC ({e}): {line}"))
    };

    send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        },
    }));
    let init = recv();
    assert_eq!(init["result"]["serverInfo"]["name"], "delldisplay");
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("display_restore"));
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = recv();
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["display_state", "display_arrange", "display_restore"]
    );
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(
        tools[1]["inputSchema"]["required"],
        json!(["layout", "reason"])
    );

    send(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "display_state", "arguments": {} },
    }));
    let call = recv();
    assert_eq!(call["result"]["isError"], true, "{call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("couldn't open the display"), "{text}");

    send(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "no_such_tool", "arguments": {} },
    }));
    assert!(recv()["error"].is_object());

    drop(stdin);
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
}

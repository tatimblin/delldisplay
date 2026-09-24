//! `delldisplay mcp`: the display as MCP tools, over stdio, for AI agents.
//!
//! Three tools: `display_state` reads, `display_arrange` changes the layout
//! and what each pane shows, `display_restore` takes this computer's change
//! back. Each call opens the display, takes the lock in [`store`], runs a
//! function from [`tools`] on a blocking thread, and closes the display
//! again: a session held for hours goes stale across layout changes and
//! sleep.
//!
//! Stdout is the protocol stream, so nothing here prints to it. Notes go to
//! stderr, which clients show in their logs.

mod config;
mod notify;
mod store;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args as ClapArgs;
use ddc_transport::{Ddc, I2c};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};

use crate::commands::exec::{exit, Ctx};
use config::Config;
use store::Store;
use tools::{Call, Reply};

#[derive(ClapArgs, Debug)]
#[command(after_help = "\
Point an agent at `delldisplay mcp`. For Claude Code:
  claude mcp add delldisplay -- delldisplay mcp

The config file sets what the server may do on its own: allow_takeover,
cooldown_seconds, notify, self_input, display. It's optional.")]
pub struct Args {
    /// Config file [default: $DELLDISPLAY_MCP_CONFIG, else ~/.config/delldisplay/mcp.toml]
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

const INSTRUCTIONS: &str = "\
Controls a Dell monitor that several computers share. The user may be looking \
at it right now, working on another computer. Change it only when showing the \
user something on this computer is worth interrupting them for. Prefer adding \
this computer beside what they're working on over replacing it, and put the \
monitor back with display_restore when they're done with it.";

pub fn run(a: &Args, ctx: &Ctx) -> u8 {
    let path = a
        .config
        .clone()
        .or_else(|| std::env::var_os("DELLDISPLAY_MCP_CONFIG").map(PathBuf::from))
        .unwrap_or_else(config::default_path);
    let cfg = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("delldisplay mcp: {e}");
            return exit::USAGE;
        }
    };
    let server = Server {
        display: cfg.display.unwrap_or(ctx.display),
        cfg: Arc::new(cfg),
        store: Arc::new(Store::new(&ctx.state_dir())),
        max_dwell: ctx.max_dwell,
        open: Arc::new(|index: usize| crate::open_display(index, true)),
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("delldisplay mcp: {e}");
            return exit::FAILED;
        }
    };
    let served = rt.block_on(async {
        let service = server.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    });
    match served {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("delldisplay mcp: {e}");
            exit::FAILED
        }
    }
}

type Open<T> = dyn Fn(usize) -> Result<Ddc<T>, String> + Send + Sync;

struct Server<T: I2c> {
    cfg: Arc<Config>,
    store: Arc<Store>,
    display: usize,
    max_dwell: Option<Duration>,
    open: Arc<Open<T>>,
}

impl<T: I2c + 'static> Server<T> {
    /// Run one tool on a blocking thread: lock, open, call, close.
    async fn run(&self, name: String, args: Map<String, Value>, client: Option<String>) -> Reply {
        let (cfg, store, open) = (self.cfg.clone(), self.store.clone(), self.open.clone());
        let (display, max_dwell) = (self.display, self.max_dwell);
        let job = move || {
            let _held = match store.lock() {
                Ok(f) => f,
                Err(e) => return Reply::error(format!("couldn't take the lock: {e}")),
            };
            let mut d = match open(display) {
                Ok(d) => d,
                Err(e) => return Reply::error(format!("couldn't open the display: {e}")),
            };
            let notify = |reason: &str| notify::post("Display changed", reason);
            let call = Call {
                cfg: &cfg,
                store: &store,
                now: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
                max_dwell,
                notify: &notify,
                client,
            };
            match name.as_str() {
                "display_state" => tools::state(&mut d, &call),
                "display_arrange" => tools::arrange(&mut d, &call, &args),
                "display_restore" => tools::restore(&mut d, &call, &args),
                other => Reply::error(format!("no tool named {other}")),
            }
        };
        tokio::task::spawn_blocking(job)
            .await
            .unwrap_or_else(|e| Reply::error(format!("the tool stopped unexpectedly: {e}")))
    }
}

fn schema(v: Value) -> Arc<Map<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => unreachable!("schemas are objects"),
    }
}

/// The tool list. The descriptions are what the model reads to decide when
/// and how to use each tool, so they say what it costs the user.
fn definitions(cfg: &Config) -> Vec<Tool> {
    let takeover = if cfg.allow_takeover {
        "Any arrangement is allowed, including taking over the whole screen."
    } else {
        "A change must keep everything now on screen visible somewhere, so add this \
         computer beside what's showing (a split, or picture-in-picture) rather than \
         replacing it. This computer may always remove its own pane."
    };
    let state = Tool::new(
        "display_state",
        "Read what the shared monitor shows right now: the layout, which input fills each \
         pane, and which input is this computer. Also lists the layouts and inputs this \
         monitor has, and whether there's a change of this computer's to put back. \
         Read-only; takes about a second. Other computers and the monitor's own buttons \
         can change it at any time, so read it before arranging and after a refusal.",
        schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
    )
    .with_title("Read the shared monitor")
    .with_annotations(ToolAnnotations::new().read_only(true).open_world(false));
    let arrange = Tool::new(
        "display_arrange",
        format!(
            "Change what the shared monitor shows: pick a layout and what goes in each pane, \
             for example side-by-side with this computer on the right, to show the user \
             something beside what they're working on. The user sees it at once. In `panes`, \
             `self` is this computer, `current` is whatever the main pane shows now, and \
             anything else is an input name from display_state. Positions left out keep \
             their source. {takeover} `reason` is shown to the user as a notification and \
             logged, so write it for them (\"Showing the test results you asked for\"). \
             Giving this computer a pane of a different size changes its resolution, so its \
             windows may move and its screen may blank for a second. `dry_run` checks \
             without writing. Call display_restore when the user is done."
        ),
        schema(json!({
            "type": "object",
            "required": ["layout", "reason"],
            "additionalProperties": false,
            "properties": {
                "layout": {
                    "type": "string",
                    "description": "A layout name from display_state, e.g. full, pip-small, \
                                    side-by-side, stacked, quad.",
                },
                "panes": {
                    "type": "object",
                    "description": "Position -> source. Positions per layout are in \
                                    display_state, e.g. left/right for side-by-side, \
                                    main/inset for pip-small.",
                    "additionalProperties": { "type": "string" },
                },
                "reason": { "type": "string", "minLength": 3, "maxLength": 200 },
                "dry_run": { "type": "boolean", "default": false },
            },
        })),
    )
    .with_title("Change the shared monitor")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(cfg.allow_takeover)
            .idempotent(true)
            .open_world(false),
    );
    let restore = Tool::new(
        "display_restore",
        "Put the shared monitor back the way it was before this computer changed it. Refuses, \
         and writes nothing, if anyone has changed the monitor since, so it never undoes \
         someone else's change. `dry_run` checks without writing.",
        schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "dry_run": { "type": "boolean", "default": false } },
        })),
    )
    .with_title("Put the shared monitor back")
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    );
    vec![state, arrange, restore]
}

fn result(reply: Reply) -> CallToolResult {
    if reply.ok {
        return CallToolResult::structured(reply.body);
    }
    // Errors carry the text the model reads; the JSON has every detail.
    let lines: Vec<String> = ["error"]
        .into_iter()
        .filter_map(|k| reply.body.get(k).and_then(Value::as_str).map(String::from))
        .chain(
            ["/refused", "/report/refused"]
                .into_iter()
                .filter_map(|p| reply.body.pointer(p).and_then(Value::as_array))
                .flatten()
                .filter_map(|r| r.as_str().map(|s| format!("refused: {s}"))),
        )
        .chain(
            reply
                .body
                .pointer("/report/failure")
                .and_then(Value::as_str)
                .map(|f| format!("failed: {f}")),
        )
        .collect();
    let mut r = CallToolResult::error(vec![
        ContentBlock::text(lines.join("\n")),
        ContentBlock::text(reply.body.to_string()),
    ]);
    r.structured_content = Some(reply.body);
    r
}

impl<T: I2c + 'static> ServerHandler for Server<T> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("delldisplay", env!("CARGO_PKG_VERSION"))
                    .with_title("Dell monitor"),
            )
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(definitions(&self.cfg)))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        definitions(&self.cfg).into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        if self.get_tool(&name).is_none() {
            return Err(McpError::invalid_params(
                format!("no tool named {name}"),
                None,
            ));
        }
        let client = context.peer.peer_info().map(|i| i.client_info.name.clone());
        let args = request.arguments.unwrap_or_default();
        Ok(result(self.run(name, args, client).await).into())
    }
}

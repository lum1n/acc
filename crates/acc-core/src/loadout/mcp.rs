//! MCP servers in each harness's own config file.
//!
//! One shape ([`McpServer`]) for every spelling, so a server can be copied
//! between harnesses; `extras` keeps the keys ACC doesn't model (Copilot's
//! `tools`, Codex timeouts, pi's `exposure`) through a same-harness round
//! trip and drops them on a copy.
//!
//! | Harness | File | Spelling |
//! |---|---|---|
//! | Claude | `~/.claude.json`, `.mcp.json` | `mcpServers`, `type: stdio/http/sse` |
//! | Codex | `~/.codex/config.toml`, `.codex/config.toml` | `[mcp_servers.<name>]` |
//! | Cursor | `~/.cursor/mcp.json`, `.cursor/mcp.json` | `mcpServers`, `url` for remote, `disabled` |
//! | Copilot | `~/.copilot/mcp-config.json` | `mcpServers`, `type: local/http/sse`, `tools` |
//! | opencode | `opencode.json(c)` | `mcp`, `type: local/remote`, `command` as an array, `enabled` |
//! | pi | `~/.pi/agent/mcp.json`, `.pi/mcp.json` | `mcpServers`, `url` for HTTP (no SSE), `enabled` |

use serde::{Deserialize, Serialize};

use super::json::{self, Json};
use super::toml::{self, Toml};
use super::{LoadoutError, Scope};
use crate::harness::Harness;

/// How a harness reaches an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    /// Listed but not translatable (an entry ACC doesn't recognise).
    Unknown { what: String },
}

/// One MCP server as one harness has it configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub transport: McpTransport,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Harness-specific keys ACC doesn't model, as a JSON object.
    #[serde(default = "empty")]
    pub extras: Json,
}

fn yes() -> bool {
    true
}

fn empty() -> Json {
    Json::Object(vec![])
}

impl McpServer {
    /// The same server with no harness-specific extras, for another harness.
    pub fn portable(&self) -> Self {
        Self {
            name: self.name.clone(),
            transport: self.transport.clone(),
            enabled: self.enabled,
            extras: empty(),
        }
    }
}

/// Whether `harness`'s file can keep a server without starting it.
/// Claude's and Copilot's can't: disabling one there means removing it.
pub fn supports_disable(harness: Harness) -> bool {
    !matches!(harness, Harness::Claude | Harness::Copilot)
}

/// Script printing `harness`'s MCP config at `scope` (nothing when the file
/// doesn't exist yet), for [`servers`]. `None` when the harness has no such
/// file there.
pub fn read_script(harness: Harness, scope: &Scope) -> Option<String> {
    super::mcp_config_script(harness, scope).map(|p| super::read_file_script(&p))
}

/// Script replacing `harness`'s MCP config at `scope` with its stdin (the
/// output of [`upsert`] or [`remove`]), after a backup to `~/.acc/backups/`.
pub fn write_script(harness: Harness, scope: &Scope) -> Option<String> {
    super::mcp_config_script(harness, scope).map(|p| super::write_file_script(&p))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// Claude: `type: stdio | http | sse`.
    Typed,
    /// Cursor: `url` means streamable HTTP, `disabled: true`.
    Url,
    /// Copilot: `type: local | http | sse`, `tools: ["*"]`.
    Copilot,
    /// opencode: `type: local | remote`, `command` is an array, `enabled`.
    Opencode,
    /// pi: `url` means streamable HTTP, `enabled: false`; no SSE.
    Pi,
}

fn dialect(harness: Harness) -> Option<(Dialect, &'static str)> {
    Some(match harness {
        Harness::Claude => (Dialect::Typed, "mcpServers"),
        Harness::Cursor => (Dialect::Url, "mcpServers"),
        Harness::Copilot => (Dialect::Copilot, "mcpServers"),
        Harness::Opencode => (Dialect::Opencode, "mcp"),
        Harness::Pi => (Dialect::Pi, "mcpServers"),
        Harness::Codex => return None,
    })
}

const JSON_KEYS: [&str; 11] = [
    "command",
    "args",
    "env",
    "environment",
    "type",
    "url",
    "serverUrl",
    "headers",
    "disabled",
    "enabled",
    "tools",
];

fn decode_json(d: Dialect, name: &str, entry: &Json) -> Option<McpServer> {
    let object = entry.as_object()?;
    let string = |k: &str| entry.get(k).and_then(Json::as_str).map(str::to_owned);
    let pairs = |k: &str| entry.get(k).map(Json::string_pairs).unwrap_or_default();
    let kind = string("type");
    let transport = match d {
        Dialect::Opencode => match kind.as_deref() {
            Some("local") => {
                let parts = entry.get("command").and_then(Json::string_array)?;
                let (command, args) = parts.split_first()?;
                McpTransport::Stdio {
                    command: command.clone(),
                    args: args.to_vec(),
                    env: pairs("environment"),
                }
            }
            Some("remote") => McpTransport::Http {
                url: string("url")?,
                headers: pairs("headers"),
            },
            other => McpTransport::Unknown {
                what: other.unwrap_or("unrecognised entry").to_owned(),
            },
        },
        _ => {
            if let Some(command) = string("command") {
                McpTransport::Stdio {
                    command,
                    args: entry.get("args").and_then(Json::string_array).unwrap_or_default(),
                    env: pairs("env"),
                }
            } else if let Some(url) = string("url").or_else(|| string("serverUrl")) {
                let headers = pairs("headers");
                if kind.as_deref() == Some("sse") {
                    McpTransport::Sse { url, headers }
                } else {
                    McpTransport::Http { url, headers }
                }
            } else {
                McpTransport::Unknown {
                    what: kind.unwrap_or_else(|| "unrecognised entry".to_owned()),
                }
            }
        }
    };
    let enabled = match d {
        Dialect::Opencode | Dialect::Pi => entry.get("enabled").and_then(Json::as_bool).unwrap_or(true),
        _ => entry.get("disabled").and_then(Json::as_bool) != Some(true),
    };
    let keep: Vec<&str> = match d {
        Dialect::Copilot => JSON_KEYS.iter().copied().filter(|k| *k != "tools").collect(),
        _ => JSON_KEYS.to_vec(),
    };
    let _ = object;
    Some(McpServer {
        name: name.to_owned(),
        transport,
        enabled,
        extras: Json::Object(entry.without(&keep)),
    })
}

fn encode_json(d: Dialect, server: &McpServer) -> Result<Json, LoadoutError> {
    let mut o: Vec<(String, Json)> = Vec::new();
    let mut put = |k: &str, v: Json| o.push((k.to_owned(), v));
    match &server.transport {
        McpTransport::Stdio { command, args, env } => {
            match d {
                Dialect::Typed => put("type", Json::str("stdio")),
                Dialect::Copilot | Dialect::Opencode => put("type", Json::str("local")),
                Dialect::Url | Dialect::Pi => {}
            }
            if d == Dialect::Opencode {
                let mut parts = vec![command.as_str()];
                parts.extend(args.iter().map(String::as_str));
                put("command", Json::strings(parts));
                if !env.is_empty() {
                    put("environment", Json::string_map(env));
                }
            } else {
                put("command", Json::str(command));
                put("args", Json::strings(args.iter().map(String::as_str)));
                if !env.is_empty() {
                    put("env", Json::string_map(env));
                }
            }
        }
        McpTransport::Http { url, headers } | McpTransport::Sse { url, headers } => {
            let sse = matches!(server.transport, McpTransport::Sse { .. });
            match d {
                Dialect::Typed | Dialect::Copilot => put("type", Json::str(if sse { "sse" } else { "http" })),
                Dialect::Opencode => put("type", Json::str("remote")),
                Dialect::Url => {
                    if sse {
                        put("type", Json::str("sse"));
                    }
                }
                Dialect::Pi => {
                    if sse {
                        return Err(LoadoutError::Unsupported("pi takes no SSE servers".into()));
                    }
                }
            }
            put("url", Json::str(url));
            if !headers.is_empty() {
                put("headers", Json::string_map(headers));
            }
        }
        McpTransport::Unknown { what } => {
            return Err(LoadoutError::Unsupported(format!(
                "can't write a server ACC doesn't understand ({what})"
            )));
        }
    }
    match d {
        Dialect::Opencode => put("enabled", Json::Bool(server.enabled)),
        Dialect::Pi => {
            if !server.enabled {
                put("enabled", Json::Bool(false));
            }
        }
        Dialect::Url => {
            if !server.enabled {
                put("disabled", Json::Bool(true));
            }
        }
        Dialect::Typed | Dialect::Copilot => {
            if !server.enabled {
                return Err(LoadoutError::Unsupported(
                    "this harness's config can't keep a disabled server; remove it instead".into(),
                ));
            }
        }
    }
    for (k, v) in server.extras.as_object().unwrap_or(&[]) {
        if !o.iter().any(|(key, _)| key == k) {
            o.push((k.clone(), v.clone()));
        }
    }
    if d == Dialect::Copilot && !o.iter().any(|(k, _)| k == "tools") {
        o.push(("tools".into(), Json::strings(["*"])));
    }
    Ok(Json::Object(o))
}

fn json_doc(text: &str) -> Result<json::Document, LoadoutError> {
    json::Document::parse(text).map_err(|e| LoadoutError::Unreadable(e.to_string()))
}

/// The servers in `harness`'s config `text` (from [`read_script`]).
pub fn servers(harness: Harness, text: &str) -> Result<Vec<McpServer>, LoadoutError> {
    let Some((d, key)) = dialect(harness) else {
        return Ok(codex_servers(text));
    };
    let doc = json_doc(text)?;
    let Some(container) = doc.get(&[key]) else {
        return Ok(vec![]);
    };
    let entries = container
        .as_object()
        .ok_or_else(|| LoadoutError::Unreadable(format!("{key:?} is not an object")))?;
    Ok(entries
        .iter()
        .filter_map(|(name, entry)| decode_json(d, name, entry))
        .collect())
}

/// `text` with `server` added or replaced.
pub fn upsert(harness: Harness, text: &str, server: &McpServer) -> Result<String, LoadoutError> {
    if !super::valid_entry_name(&server.name) {
        return Err(LoadoutError::InvalidName(server.name.clone()));
    }
    let Some((d, key)) = dialect(harness) else {
        return codex_upsert(text, server);
    };
    let mut doc = json_doc(text)?;
    if doc.get(&[key]).is_some_and(|v| v.as_object().is_none()) {
        return Err(LoadoutError::Unreadable(format!("{key:?} is not an object")));
    }
    let entry = encode_json(d, server)?;
    doc.set(&[key, &server.name], entry)
        .map_err(|e| LoadoutError::Unreadable(e.to_string()))?;
    Ok(doc.text().to_owned())
}

/// `text` without the server `name` (unchanged when it isn't there).
pub fn remove(harness: Harness, text: &str, name: &str) -> Result<String, LoadoutError> {
    let Some((_, key)) = dialect(harness) else {
        let mut doc = toml::Document::parse(text);
        doc.remove_table(&format!("mcp_servers.{name}"));
        return Ok(doc.text());
    };
    let mut doc = json_doc(text)?;
    doc.remove(&[key, name])
        .map_err(|e| LoadoutError::Unreadable(e.to_string()))?;
    Ok(doc.text().to_owned())
}

/// `text` with the server `name` kept but not started (or started again).
/// [`LoadoutError::Unsupported`] when the harness's file can't say that.
pub fn set_enabled(harness: Harness, text: &str, name: &str, enabled: bool) -> Result<String, LoadoutError> {
    let mut server = servers(harness, text)?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| LoadoutError::NotFound(name.to_owned()))?;
    if server.enabled == enabled {
        return Ok(text.to_owned());
    }
    server.enabled = enabled;
    upsert(harness, text, &server)
}

// ---- Codex: TOML -----------------------------------------------------------

const TOML_KEYS: [&str; 6] = ["command", "args", "env", "url", "http_headers", "enabled"];

fn codex_servers(text: &str) -> Vec<McpServer> {
    toml::Document::parse(text)
        .tables_under("mcp_servers")
        .into_iter()
        .map(|table| {
            let name = table.name.trim_start_matches("mcp_servers.").to_owned();
            let pairs = |k: &str| {
                table
                    .get(k)
                    .map(|v| v.to_json().string_pairs())
                    .unwrap_or_default()
            };
            let transport = if let Some(command) = table.get("command").and_then(Toml::as_str) {
                McpTransport::Stdio {
                    command: command.to_owned(),
                    args: table
                        .get("args")
                        .and_then(|a| a.to_json().string_array())
                        .unwrap_or_default(),
                    env: pairs("env"),
                }
            } else if let Some(url) = table.get("url").and_then(Toml::as_str) {
                McpTransport::Http {
                    url: url.to_owned(),
                    headers: pairs("http_headers"),
                }
            } else {
                McpTransport::Unknown {
                    what: "unrecognised table".into(),
                }
            };
            let enabled = !matches!(table.get("enabled"), Some(Toml::Bool(false)));
            McpServer {
                name,
                transport,
                enabled,
                extras: Json::Object(table.to_json().without(&TOML_KEYS)),
            }
        })
        .collect()
}

fn codex_upsert(text: &str, server: &McpServer) -> Result<String, LoadoutError> {
    let strings = |pairs: &[(String, String)]| {
        let mut pairs = pairs.to_vec();
        pairs.sort();
        Toml::Table(pairs.into_iter().map(|(k, v)| (k, Toml::String(v))).collect())
    };
    let mut entries: Vec<(String, Toml)> = Vec::new();
    match &server.transport {
        McpTransport::Stdio { command, args, env } => {
            entries.push(("command".into(), Toml::String(command.clone())));
            entries.push((
                "args".into(),
                Toml::Array(args.iter().cloned().map(Toml::String).collect()),
            ));
            if !env.is_empty() {
                entries.push(("env".into(), strings(env)));
            }
        }
        McpTransport::Http { url, headers } | McpTransport::Sse { url, headers } => {
            entries.push(("url".into(), Toml::String(url.clone())));
            if !headers.is_empty() {
                entries.push(("http_headers".into(), strings(headers)));
            }
        }
        McpTransport::Unknown { what } => {
            return Err(LoadoutError::Unsupported(format!(
                "can't write a server ACC doesn't understand ({what})"
            )));
        }
    }
    if !server.enabled {
        entries.push(("enabled".into(), Toml::Bool(false)));
    }
    for (k, v) in server.extras.as_object().unwrap_or(&[]) {
        if let Some(v) = Toml::from_json(v)
            && !entries.iter().any(|(key, _)| key == k)
        {
            entries.push((k.clone(), v));
        }
    }
    // Inline values first, sub-tables after, so the table reads naturally.
    let (tables, scalars): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|(_, v)| matches!(v, Toml::Table(_)));
    let mut doc = toml::Document::parse(text);
    doc.set_table(
        &format!("mcp_servers.{}", server.name),
        &scalars.into_iter().chain(tables).collect::<Vec<_>>(),
    );
    Ok(doc.text())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn stdio(name: &str) -> McpServer {
        McpServer {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@x/server".into()],
                env: vec![("TOKEN".into(), "t".into())],
            },
            enabled: true,
            extras: Json::Object(vec![]),
        }
    }

    fn http(name: &str) -> McpServer {
        McpServer {
            name: name.into(),
            transport: McpTransport::Http {
                url: "https://x/mcp".into(),
                headers: vec![],
            },
            enabled: true,
            extras: Json::Object(vec![]),
        }
    }

    #[test]
    fn every_json_harness_round_trips_and_keeps_the_rest() {
        for h in [
            Harness::Claude,
            Harness::Cursor,
            Harness::Copilot,
            Harness::Opencode,
            Harness::Pi,
        ] {
            let text = "{\n  // mine\n  \"theme\": \"dark\"\n}\n";
            let a = upsert(h, text, &stdio("a")).unwrap();
            let b = upsert(h, &a, &http("b")).unwrap();
            assert!(
                b.contains("// mine") && b.contains("\"theme\": \"dark\""),
                "{h:?}: {b}"
            );
            let got = servers(h, &b).unwrap();
            assert_eq!(got.len(), 2, "{h:?}");
            assert_eq!(got[0].portable(), stdio("a"), "{h:?}: {b}");
            assert_eq!(got[1].portable(), http("b"), "{h:?}: {b}");
            let gone = remove(h, &b, "a").unwrap();
            assert_eq!(servers(h, &gone).unwrap().len(), 1, "{h:?}");
            if supports_disable(h) {
                let off = set_enabled(h, &b, "b", false).unwrap();
                let s = servers(h, &off).unwrap();
                assert!(!s[1].enabled && s[0].enabled, "{h:?}: {off}");
                let on = set_enabled(h, &off, "b", true).unwrap();
                assert!(servers(h, &on).unwrap()[1].enabled);
            } else {
                assert!(matches!(
                    set_enabled(h, &b, "b", false),
                    Err(LoadoutError::Unsupported(_))
                ));
            }
        }
    }

    #[test]
    fn spellings() {
        let claude = upsert(Harness::Claude, "", &http("b")).unwrap();
        assert!(claude.contains("\"type\": \"http\""));
        let cursor = upsert(Harness::Cursor, "", &http("b")).unwrap();
        assert!(!cursor.contains("\"type\"") && cursor.contains("\"url\""));
        let copilot = upsert(Harness::Copilot, "", &stdio("a")).unwrap();
        assert!(copilot.contains("\"type\": \"local\"") && copilot.contains("\"tools\": [\"*\"]"));
        let oc = upsert(Harness::Opencode, "", &stdio("a")).unwrap();
        assert!(
            oc.contains("\"command\": [\"npx\", \"-y\", \"@x/server\"]") && oc.contains("\"enabled\": true")
        );
        assert!(oc.contains("\"environment\""));
        let pi = upsert(Harness::Pi, "", &http("b")).unwrap();
        assert!(!pi.contains("\"type\""));
        let sse = McpServer {
            transport: McpTransport::Sse {
                url: "https://x/sse".into(),
                headers: vec![("A".into(), "b".into())],
            },
            ..http("s")
        };
        assert!(matches!(
            upsert(Harness::Pi, "", &sse),
            Err(LoadoutError::Unsupported(_))
        ));
        let claude_sse = upsert(Harness::Claude, "", &sse).unwrap();
        assert_eq!(
            servers(Harness::Claude, &claude_sse).unwrap()[0].transport,
            sse.transport
        );
    }

    #[test]
    fn extras_survive_a_same_harness_round_trip() {
        let text = "{\"mcpServers\": {\"a\": {\"command\": \"x\", \"args\": [], \"tools\": [\"read\"], \"timeout\": 5}}}";
        let got = servers(Harness::Copilot, text).unwrap();
        assert_eq!(got[0].extras.get("timeout").unwrap(), &Json::Number("5".into()));
        assert!(got[0].extras.get("tools").is_some());
        let back = upsert(Harness::Copilot, text, &got[0]).unwrap();
        assert!(back.contains("\"timeout\": 5") && back.contains("\"tools\": [\"read\"]"));
        assert!(got[0].portable().extras.as_object().unwrap().is_empty());
    }

    #[test]
    fn codex_toml() {
        let text = "model = \"o3\"\n";
        let a = upsert(Harness::Codex, text, &stdio("a")).unwrap();
        assert_eq!(
            a,
            "model = \"o3\"\n\n[mcp_servers.a]\ncommand = \"npx\"\nargs = [\"-y\", \"@x/server\"]\n\n[mcp_servers.a.env]\nTOKEN = \"t\"\n"
        );
        let b = upsert(Harness::Codex, &a, &http("b")).unwrap();
        let got = servers(Harness::Codex, &b).unwrap();
        assert_eq!(got[0].portable(), stdio("a"));
        assert_eq!(got[1].portable(), http("b"));
        let off = set_enabled(Harness::Codex, &b, "a", false).unwrap();
        assert!(off.contains("enabled = false"));
        assert!(!servers(Harness::Codex, &off).unwrap()[0].enabled);
        let gone = remove(Harness::Codex, &off, "a").unwrap();
        assert!(!gone.contains("[mcp_servers.a"));
        assert_eq!(servers(Harness::Codex, &gone).unwrap().len(), 1);
    }

    #[test]
    fn bad_input() {
        assert!(matches!(
            servers(Harness::Claude, "{\"mcpServers\": 1}"),
            Err(LoadoutError::Unreadable(_))
        ));
        assert!(matches!(
            servers(Harness::Claude, "{nope"),
            Err(LoadoutError::Unreadable(_))
        ));
        assert!(matches!(
            upsert(Harness::Claude, "", &stdio("a b")),
            Err(LoadoutError::InvalidName(_))
        ));
        assert!(matches!(
            set_enabled(Harness::Cursor, "{}", "x", false),
            Err(LoadoutError::NotFound(_))
        ));
        assert!(read_script(Harness::Copilot, &Scope::Project { dir: "/w".into() }).is_none());
        assert!(
            write_script(Harness::Claude, &Scope::User)
                .unwrap()
                .contains(".acc/backups")
        );
    }
}

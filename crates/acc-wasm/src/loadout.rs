//! Skills and MCP servers for every harness (`acc_core::loadout`): scripts
//! to run on the host and pure functions over their output. Scopes,
//! servers, skills and listings are plain objects in the core's serde
//! shapes (`{scope: "user"}` or `{scope: "project", dir}`).

use acc_core::loadout::{LoadoutError, Scope, mcp, skills};
use wasm_bindgen::prelude::*;

use crate::{AccExec, arg, error, harness, js};

fn scope(v: JsValue) -> Result<Scope, JsValue> {
    arg(v, "scope")
}

fn harnesses(v: JsValue) -> Result<Vec<acc_core::Harness>, JsValue> {
    let names: Vec<String> = arg(v, "harnesses")?;
    names.iter().map(|n| harness(n)).collect()
}

fn err(e: LoadoutError) -> JsValue {
    let kind = match e {
        LoadoutError::Unsupported(_) => "Unsupported",
        _ => "InvalidArgument",
    };
    error(kind, &e.to_string())
}

fn exec(r: Result<String, LoadoutError>) -> Result<AccExec, JsValue> {
    r.map(AccExec::script).map_err(err)
}

// ---- skills ----------------------------------------------------------------

/// Lists the skill store at `scope` and every harness's skills directory,
/// for `parseSkills`.
#[wasm_bindgen(js_name = skillsListExec)]
pub fn skills_list_exec(scope: JsValue) -> Result<AccExec, JsValue> {
    Ok(AccExec::script(skills::list_script(&self::scope(scope)?)))
}

/// The listing as `{store, skills, places}`.
#[wasm_bindgen(js_name = parseSkills)]
pub fn parse_skills(stdout: &str, scope: JsValue) -> Result<JsValue, JsValue> {
    Ok(js(&skills::parse_listing(stdout, &self::scope(scope)?)))
}

/// Prints the store skill's `SKILL.md`; exits 9 when there is no such skill.
#[wasm_bindgen(js_name = skillReadExec)]
pub fn skill_read_exec(scope: JsValue, name: &str) -> Result<AccExec, JsValue> {
    exec(skills::read_script(&self::scope(scope)?, name))
}

/// Writes `markdown` as the store skill's `SKILL.md`, making the skill
/// when it is new. Check `skillIssues` first.
#[wasm_bindgen(js_name = skillWriteExec)]
pub fn skill_write_exec(scope: JsValue, name: &str, markdown: &str) -> Result<AccExec, JsValue> {
    let script = skills::write_script(&self::scope(scope)?, name).map_err(err)?;
    Ok(AccExec {
        script,
        stdin: markdown.as_bytes().to_vec(),
    })
}

/// Deletes the store skill and every harness's link to it; exits 9 when
/// there is no such skill.
#[wasm_bindgen(js_name = skillRemoveExec)]
pub fn skill_remove_exec(scope: JsValue, name: &str) -> Result<AccExec, JsValue> {
    exec(skills::remove_script(&self::scope(scope)?, name))
}

/// Links the store skill into each harness's skills directory (relative
/// symlinks; harnesses that read the store are skipped). Exits 8 when
/// something else is in the way, unless `force` replaces it, and 9 when
/// the skill isn't in the store. `harnesses` is an array of names.
#[wasm_bindgen(js_name = skillLinkExec)]
pub fn skill_link_exec(
    scope: JsValue,
    name: &str,
    harnesses: JsValue,
    force: bool,
) -> Result<AccExec, JsValue> {
    let hs = self::harnesses(harnesses)?;
    exec(skills::link_script(&self::scope(scope)?, name, &hs, force))
}

/// Removes each harness's link to the store skill (only links into the
/// store are touched).
#[wasm_bindgen(js_name = skillUnlinkExec)]
pub fn skill_unlink_exec(scope: JsValue, name: &str, harnesses: JsValue) -> Result<AccExec, JsValue> {
    let hs = self::harnesses(harnesses)?;
    exec(skills::unlink_script(&self::scope(scope)?, name, &hs))
}

/// Moves a skill that lives only in `harness`'s skills directory into the
/// store and leaves a link behind. Exits 9 when it isn't a real directory
/// there, 8 when the store already has the name.
#[wasm_bindgen(js_name = skillAdoptExec)]
pub fn skill_adopt_exec(scope: JsValue, harness: &str, name: &str) -> Result<AccExec, JsValue> {
    exec(skills::adopt_script(
        &self::scope(scope)?,
        self::harness(harness)?,
        name,
    ))
}

/// What is wrong with the skill `name` whose `SKILL.md` is `markdown`:
/// an array of `{error, message}` (empty when it is valid).
#[wasm_bindgen(js_name = skillIssues)]
pub fn skill_issues(name: &str, markdown: &str) -> JsValue {
    js(&skills::issues(name, markdown))
}

/// A `SKILL.md` for a new skill.
#[wasm_bindgen(js_name = skillTemplate)]
pub fn skill_template(name: &str, description: &str) -> String {
    skills::template(name, description)
}

// ---- MCP -------------------------------------------------------------------

/// Prints `harness`'s MCP config at `scope` (nothing when the file doesn't
/// exist yet), for `mcpServers`. `undefined` when the harness has no such
/// file there (Copilot has no project file).
#[wasm_bindgen(js_name = mcpReadExec)]
pub fn mcp_read_exec(harness: &str, scope: JsValue) -> Result<Option<AccExec>, JsValue> {
    Ok(mcp::read_script(self::harness(harness)?, &self::scope(scope)?).map(AccExec::script))
}

/// The servers in `harness`'s config `text` (from `mcpReadExec`), as an
/// array of `{name, transport, enabled, extras}`.
#[wasm_bindgen(js_name = mcpServers)]
pub fn mcp_servers(harness: &str, text: &str) -> Result<JsValue, JsValue> {
    mcp::servers(self::harness(harness)?, text)
        .map(|s| js(&s))
        .map_err(err)
}

/// `text` with `server` added or replaced; only that entry changes.
#[wasm_bindgen(js_name = mcpUpsert)]
pub fn mcp_upsert(harness: &str, text: &str, server: JsValue) -> Result<String, JsValue> {
    let server: mcp::McpServer = arg(server, "server")?;
    mcp::upsert(self::harness(harness)?, text, &server).map_err(err)
}

/// `text` without the server `name`.
#[wasm_bindgen(js_name = mcpRemove)]
pub fn mcp_remove(harness: &str, text: &str, name: &str) -> Result<String, JsValue> {
    mcp::remove(self::harness(harness)?, text, name).map_err(err)
}

/// `text` with the server `name` kept but not started (or started again).
/// `Unsupported` for Claude and Copilot, whose files can't say that: remove
/// the server instead.
#[wasm_bindgen(js_name = mcpSetEnabled)]
pub fn mcp_set_enabled(harness: &str, text: &str, name: &str, enabled: bool) -> Result<String, JsValue> {
    mcp::set_enabled(self::harness(harness)?, text, name, enabled).map_err(err)
}

/// Whether `harness`'s config can keep a disabled server.
#[wasm_bindgen(js_name = mcpSupportsDisable)]
pub fn mcp_supports_disable(harness: &str) -> Result<bool, JsValue> {
    Ok(mcp::supports_disable(self::harness(harness)?))
}

/// Replaces `harness`'s MCP config at `scope` with `text` (from
/// `mcpUpsert` or `mcpRemove`), after a backup to `~/.acc/backups/`.
#[wasm_bindgen(js_name = mcpWriteExec)]
pub fn mcp_write_exec(harness: &str, scope: JsValue, text: &str) -> Result<Option<AccExec>, JsValue> {
    Ok(
        mcp::write_script(self::harness(harness)?, &self::scope(scope)?).map(|script| AccExec {
            script,
            stdin: text.as_bytes().to_vec(),
        }),
    )
}

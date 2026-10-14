//! acc-ffi and acc-wasm must expose the same objects and functions, so
//! every embedder (mobile, desktop, web) gets the same API. Reads both
//! crates' sources and compares what they export.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;

/// `name` for exported functions and `Type::name` for exported methods,
/// from items marked with an attribute starting with `attr`.
fn exports(crate_dir: &str, attr: &str) -> BTreeSet<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_dir)
        .join("src");
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut marked = false;
        let mut in_impl: Option<String> = None;
        for raw in text.lines() {
            if raw.starts_with("#[cfg(test)]") {
                break;
            }
            if raw == "}" {
                in_impl = None;
                continue;
            }
            let line = raw.trim_start();
            if line.starts_with(attr) {
                marked = true;
                continue;
            }
            if line.starts_with("#[") || line.starts_with("//") || line.is_empty() {
                continue;
            }
            let fn_name = line
                .strip_prefix("pub fn ")
                .and_then(|r| r.split(['(', '<']).next());
            match (&in_impl, fn_name) {
                (Some(ty), Some(name)) => {
                    out.insert(format!("{ty}::{name}"));
                }
                (None, Some(name)) if marked => {
                    out.insert(name.to_owned());
                }
                (None, None) if marked && raw.starts_with("impl ") => {
                    let ty = line["impl ".len()..].trim_end_matches(" {").trim();
                    in_impl = Some(ty.to_owned());
                }
                _ => {}
            }
            marked = false;
        }
    }
    out
}

/// acc-ffi objects that open SSH connections themselves (through acc-ssh).
/// A browser has no raw sockets, so JS embedders run the same sessions over
/// their own transport with `Session` or `AccSession` instead.
const SSH_TRANSPORT: [&str; 3] = ["SshHost::", "RemoteSession::", "PaneSession::"];

#[test]
fn ffi_and_wasm_export_the_same_api() {
    let mut ffi = exports("acc-ffi", "#[uniffi::export");
    ffi.retain(|name| !SSH_TRANSPORT.iter().any(|t| name.starts_with(t)));
    let wasm = exports("acc-wasm", "#[wasm_bindgen");
    assert!(ffi.contains("AccSession::feed") && wasm.contains("AccSession::feed"));
    let only_ffi: Vec<_> = ffi.difference(&wasm).collect();
    let only_wasm: Vec<_> = wasm.difference(&ffi).collect();
    assert!(
        only_ffi.is_empty() && only_wasm.is_empty(),
        "bindings differ\nonly in acc-ffi: {only_ffi:?}\nonly in acc-wasm: {only_wasm:?}"
    );
}

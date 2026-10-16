//! The editor page Nextcloud Office loads in its iframe: a shell around the craft app's web
//! build that speaks Nextcloud Office's PostMessage API to the page around it, and the
//! "craft-live embed" contract to the app inside it (docs/design.md, "The editor page").
//!
//! The page's script is the one piece of JavaScript in craft-live: the PostMessage API can only
//! be spoken from script in the page, and the app's own build (Rust compiled to wasm) runs in
//! the inner frame. It's kept small and inline, with no other code loaded.

use serde_json::Value as Json;

use crate::ids::html;

/// The shell's script. `CFG` is the JSON in `#craft-live-config`.
const SCRIPT: &str = r#"
(function () {
  "use strict";
  var CFG = JSON.parse(document.getElementById("craft-live-config").textContent);
  var frame = document.getElementById("app");
  var note = document.getElementById("note");
  var hostReady = false, loaded = false, appReady = false, modified = false, everModified = false, closing = false;
  var queue = [];
  function toHost(id, values) {
    if (!CFG.origin || window.parent === window) return;
    var msg = JSON.stringify({ MessageId: id, SendTime: Date.now(), Values: values || {} });
    window.parent.postMessage(msg, CFG.origin);
  }
  function toApp(type, data) {
    if (!frame || !frame.contentWindow) return;
    var m = { type: type };
    for (var k in data || {}) m[k] = data[k];
    if (!appReady && type !== "craft-live:config") { queue.push(m); return; }
    frame.contentWindow.postMessage(m, location.origin);
  }
  function show(text, failed) {
    note.querySelector("p").textContent = text;
    note.hidden = false;
    if (!loaded) {
      loaded = true;
      toHost("App_LoadingStatus", failed ? { Status: "Failed" } : { Status: "Document_Loaded", DocumentLoadedTime: Date.now() });
    }
  }
  function close() {
    if (closing) return;
    closing = true;
    toApp("craft-live:close", {});
    toHost("UI_Close", { EverModified: everModified });
    if (!CFG.origin && CFG.closeUrl) location.href = CFG.closeUrl;
  }
  function api(path) {
    return fetch(CFG.api + path, { method: "POST", headers: { Authorization: "Bearer " + CFG.token } })
      .then(function (r) { return r.ok; }).catch(function () { return false; });
  }
  document.getElementById("note-close").addEventListener("click", close);
  window.addEventListener("message", function (e) {
    if (frame && e.source === frame.contentWindow && e.origin === location.origin) {
      var m = e.data;
      if (!m || typeof m.type !== "string") return;
      switch (m.type) {
        case "craft-live:hello":
          appReady = true;
          frame.contentWindow.postMessage({ type: "craft-live:config", config: CFG.embed }, location.origin);
          while (queue.length) frame.contentWindow.postMessage(queue.shift(), location.origin);
          break;
        case "craft-live:loaded":
          note.hidden = true;
          if (!loaded) { loaded = true; toHost("App_LoadingStatus", { Status: "Document_Loaded", DocumentLoadedTime: Date.now() }); }
          break;
        case "craft-live:modified":
          modified = !!m.modified; everModified = everModified || modified;
          toHost("Doc_ModifiedStatus", { Modified: modified });
          break;
        case "craft-live:saved":
          toHost("Action_Save_Resp", { success: m.success !== false, fileName: CFG.fileName });
          break;
        case "craft-live:peers":
          if (Array.isArray(m.peers)) toHost("Views_List", m.peers.map(function (p, i) {
            return { ViewId: p.clientId || i, UserId: p.user && p.user.id, UserName: p.user && p.user.name, Color: p.user && p.user.color, ReadOnly: p.role === "view", IsCurrentView: !!p.self };
          }));
          break;
        case "craft-live:close":
          close();
          break;
        case "craft-live:error":
          show(String(m.message || "The document couldn't be opened."), !loaded);
          break;
      }
      return;
    }
    if (!CFG.origin || e.origin !== CFG.origin || e.source !== window.parent) return;
    var msg;
    try { msg = typeof e.data === "string" ? JSON.parse(e.data) : e.data; } catch (err) { return; }
    if (!msg || typeof msg.MessageId !== "string") return;
    var v = msg.Values || {};
    switch (msg.MessageId) {
      case "Host_PostmessageReady": hostReady = true; break;
      case "Action_Save":
        api("/save").then(function (ok) { if (!ok) toHost("Action_Save_Resp", { success: false, errorMsg: "craft-live couldn't start a save" }); });
        toApp("craft-live:save", { notify: !!v.Notify });
        break;
      case "Action_Close": case "Close_Session": close(); break;
      case "Grab_Focus": if (frame) frame.focus(); break;
      case "Blur_Focus": if (frame) frame.blur(); break;
      case "Get_Views": toApp("craft-live:getPeers", {}); break;
      default: if (hostReady) toApp("craft-live:host", { message: msg.MessageId, values: v });
    }
  });
  toHost("App_LoadingStatus", { Status: "Frame_Ready", Features: { VersionStates: false } });
  if (!frame) { show(CFG.problem || "The editor isn't available.", false); return; }
  setTimeout(function () {
    if (!appReady && !loaded) show(CFG.notLive, false);
  }, CFG.helloTimeoutMs);
})();
"#;

const STYLE: &str = r#"
html,body{margin:0;height:100%;overflow:hidden;background:#fff;font:14px/1.45 system-ui,-apple-system,"Segoe UI",sans-serif;color:#1d2125}
@media (prefers-color-scheme:dark){html,body{background:#1b1d1f;color:#e6e7e8}#note .card{background:#26292c;border-color:#3a3e42}}
#app{border:0;width:100%;height:100%;display:block}
#note{position:fixed;inset:0;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.35)}
#note[hidden]{display:none}
#note .card{max-width:30rem;margin:16px;padding:20px 22px;background:#fff;border:1px solid #d6d9dc;border-radius:10px;box-shadow:0 8px 30px rgba(0,0,0,.2)}
#note h1{font-size:16px;margin:0 0 8px}
#note p{margin:0 0 16px;white-space:pre-line}
#note button{font:inherit;padding:6px 16px;border-radius:6px;border:1px solid #3b7dd8;background:#3b7dd8;color:#fff;cursor:pointer}
"#;

/// The page that hosts the app. `config` is what the script reads (see [`SCRIPT`]); `app_url`
/// is the app's `index.html` with the embed fragment, or `None` when the app isn't installed.
pub fn editor_page(title: &str, config: &Json, app_url: Option<&str>) -> String {
    // `</` can't end the script element from inside the JSON.
    let cfg = config.to_string().replace("</", "<\\/");
    let frame = match app_url {
        Some(u) => format!(
            "<iframe id=\"app\" src=\"{}\" title=\"{}\" allow=\"clipboard-read; clipboard-write; fullscreen\" allowfullscreen></iframe>",
            html(u),
            html(title)
        ),
        None => String::new(),
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"referrer\" content=\"no-referrer\"><title>{t}</title><style>{STYLE}</style></head><body>\
{frame}<div id=\"note\" hidden><div class=\"card\" role=\"alertdialog\" aria-labelledby=\"note-title\"><h1 id=\"note-title\">{t}</h1><p></p>\
<button id=\"note-close\" type=\"button\">Close</button></div></div>\
<script type=\"application/json\" id=\"craft-live-config\">{cfg}</script><script>{SCRIPT}</script></body></html>\n",
        t = html(title),
    )
}

/// A page that only explains a problem (and still answers Nextcloud's handshake, so its own
/// loading screen doesn't hang). `origin` is the page around it, when known.
pub fn problem_page(message: &str, origin: &str) -> String {
    let cfg = serde_json::json!({"origin": origin, "problem": message, "embed": {}, "helloTimeoutMs": 0, "notLive": message, "token": "", "api": ""});
    editor_page("craft-live", &cfg, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_escape() {
        let p = editor_page("A <b>", &json!({"x": "</script><script>alert(1)</script>"}), Some("/apps/gridcraft/index.html#a=\"b\""));
        assert!(p.contains("A &lt;b&gt;"));
        assert!(!p.contains("</script><script>alert(1)"));
        assert!(p.contains("index.html#a=&quot;b&quot;"));
        let e = problem_page("Not <allowed>", "https://cloud.example");
        assert!(e.contains("Not <allowed>") || e.contains("Not \\u003callowed"));
        assert!(!e.contains("<iframe"));
    }
}

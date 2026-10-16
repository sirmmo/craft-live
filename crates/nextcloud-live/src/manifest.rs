//! The manifest (`plugin.json`) that `--manifest` prints: one for the three apps.

use serde_json::{Value as Json, json};

pub fn manifest() -> Json {
    let cmd = |id: &str, label: &str, description: &str, params: &str| json!({"id": id, "label": label, "description": description, "params": params});
    json!({
        "id": "org.craft.nextcloud-live",
        "name": "Live (Nextcloud)",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Edit Nextcloud files live with the people who have them open in Nextcloud Office or in a craft app, through a craft-live server. Needs craft plugin protocol 2.",
        "author": "Marco Montanari and the craft-live contributors",
        "homepage": "https://github.com/sirmmo/craft-live",
        "apps": ["gridcraft", "wordcraft", "deckcraft"],
        "run": {"command": "./nextcloud-live"},
        "collab": {"transport": "nextcloud-live", "label": "Live (Nextcloud)"},
        "requires": {
            "protocol": ">=2.0",
            "features": ["dialogs", "open", "push", "collab"],
            "addins": [
                {"id": "org.craft.nextcloud", "github": "sirmmo/craft-nextcloud", "optional": true, "why": "Shares its Nextcloud accounts, and Share Live works on the files it opened"}
            ]
        },
        "commands": [
            cmd("open", "Open Live", "Open a Nextcloud file and edit it live with everyone who has it open", "{account?, path?, id?} or a dialog"),
            cmd("share", "Share Live", "Make the open document live as its Nextcloud file (or a given one)", "{account?, path?} or {fields}"),
            cmd("join", "Join Live", "Join a file's live session from its Nextcloud link (…/f/<id>)", "{link, account?} or {fields}"),
            cmd("leave", "Leave Live Session", "Leave the live session (the document stays open)", "{room?}"),
            cmd("status", "Live Status", "The live sessions and who's in them", "{}"),
            cmd("connect", "Connect to Nextcloud", "Sign in to a Nextcloud (Login Flow v2)", "{server} or {fields}"),
            cmd("finishLogin", "Finish Sign-in", "Continue the sign-in (from its dialog)", "{server, login, token, endpoint}"),
            cmd("chooseAccount", "Choose Account", "Pick the account to browse (from a dialog)", "{id?}"),
            cmd("settings", "Live Settings", "The craft-live server of each Nextcloud", "{}"),
            cmd("saveSettings", "Save Live Settings", "Store the settings (from the Settings dialog)", "{fields}"),
            cmd("dismiss", "Close", "Close a dialog", "{}"),
        ],
        "ui": {
            "ribbon": [
                {"tab": "Live", "group": "Nextcloud", "buttons": [
                    {"command": "open", "icon": "folder"},
                    {"command": "share", "icon": "share"},
                    {"command": "join", "icon": "link"},
                    {"command": "leave", "icon": "minus", "size": "small"},
                    {"command": "status", "icon": "info", "size": "small"},
                    {"command": "settings", "icon": "settings", "size": "small"},
                ]},
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn buttons_name_commands_and_plugin_json_matches() {
        let m = super::manifest();
        let ids: Vec<&str> = m["commands"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        for g in m["ui"]["ribbon"].as_array().unwrap() {
            for b in g["buttons"].as_array().unwrap() {
                assert!(ids.contains(&b["command"].as_str().unwrap()));
            }
        }
        assert!(ids.contains(&"share") && ids.contains(&"join"));
        let file: serde_json::Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        assert_eq!(file, m, "run `nextcloud-live --manifest > crates/nextcloud-live/plugin.json`");
    }
}

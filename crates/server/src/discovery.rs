//! `/hosting/discovery` (WOPI discovery XML) and `/hosting/capabilities` (the JSON Nextcloud
//! Office reads from Collabora-compatible servers).
//!
//! Nextcloud Office picks the editor by MIME type: an `<app name="<mime type>">` with an
//! `<action ext="" name="edit" default="true" urlsrc="…?">`, and appends `WOPISrc=…` and its own
//! parameters to `urlsrc`. Microsoft's discovery format names apps and actions by extension;
//! both forms are published, in the `external-http` and `external-https` zones, with the
//! `Capabilities` entry Nextcloud checks for and the proof keys (docs/design.md, "Discovery").

use serde_json::{Value as Json, json};

use crate::config::{APPS, Config, FORMATS};
use crate::ids::html;
use crate::proof::ProofKeys;

/// The editor page of an app: `<public>/edit/<app>?` (view: `?mode=view&`).
pub fn urlsrc(cfg: &Config, app: &str, view: bool) -> String {
    if view { format!("{}/edit/{app}?mode=view&", cfg.public_url) } else { format!("{}/edit/{app}?", cfg.public_url) }
}

fn app_label(app: &str) -> &'static str {
    match app {
        "gridcraft" => "GridCraft",
        "wordcraft" => "WordCraft",
        _ => "DeckCraft",
    }
}

/// The discovery XML.
pub fn discovery(cfg: &Config, proof: Option<&ProofKeys>) -> String {
    let mut zone = String::new();
    let enabled: Vec<&str> = APPS.iter().copied().filter(|a| cfg.apps.contains_key(*a)).collect();
    // By MIME type (what Nextcloud Office reads).
    for f in FORMATS.iter().filter(|f| enabled.contains(&f.app)) {
        zone.push_str(&format!(
            "    <app name=\"{}\">\n      <action default=\"true\" ext=\"\" name=\"edit\" requires=\"locks,update\" urlsrc=\"{}\"/>\n      <action ext=\"\" name=\"view\" urlsrc=\"{}\"/>\n    </app>\n",
            html(f.mime),
            html(&urlsrc(cfg, f.app, false)),
            html(&urlsrc(cfg, f.app, true)),
        ));
    }
    // By extension (Microsoft's form).
    for app in &enabled {
        zone.push_str(&format!("    <app name=\"{}\" favIconUrl=\"{}\">\n", app_label(app), html(&format!("{}/static/{app}.svg", cfg.public_url))));
        for f in FORMATS.iter().filter(|f| f.app == *app) {
            zone.push_str(&format!(
                "      <action name=\"edit\" ext=\"{}\" default=\"true\" requires=\"locks,update\" urlsrc=\"{}\"/>\n      <action name=\"view\" ext=\"{}\" urlsrc=\"{}\"/>\n",
                f.ext,
                html(&urlsrc(cfg, app, false)),
                f.ext,
                html(&urlsrc(cfg, app, true)),
            ));
        }
        zone.push_str("    </app>\n");
    }
    zone.push_str(&format!(
        "    <app name=\"Capabilities\">\n      <action ext=\"\" name=\"getinfo\" requires=\"locks,update\" urlsrc=\"{}\"/>\n    </app>\n",
        html(&format!("{}/hosting/capabilities", cfg.public_url))
    ));
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<wopi-discovery>\n");
    for name in ["external-http", "external-https"] {
        xml.push_str(&format!("  <net-zone name=\"{name}\">\n{zone}  </net-zone>\n"));
    }
    if let Some(p) = proof {
        let (cur, old) = p.discovery();
        xml.push_str(&format!(
            "  <proof-key oldvalue=\"{}\" oldmodulus=\"{}\" oldexponent=\"{}\" value=\"{}\" modulus=\"{}\" exponent=\"{}\"/>\n",
            old.value, old.modulus, old.exponent, cur.value, cur.modulus, cur.exponent
        ));
    }
    xml.push_str("</wopi-discovery>\n");
    xml
}

/// The capabilities JSON. `craftLive` is craft-live's own: it lets a client that finds this
/// server through Nextcloud learn its API.
pub fn capabilities(cfg: &Config, server_id: &str) -> Json {
    json!({
        "convert-to": {"available": false},
        "hasMobileSupport": true,
        "hasProxyPrefix": false,
        "hasSettingIframeSupport": false,
        "hasTemplateSaveAs": false,
        "hasTemplateSource": false,
        "hasWASMSupport": false,
        "hasWopiAccessCheck": true,
        "hasZoteroSupport": false,
        "hasDocumentSigningSupport": false,
        "productName": "craft-live",
        "productVersion": cfg.compat_version,
        "productVersionHash": env!("CARGO_PKG_VERSION"),
        "serverId": server_id,
        "craftLive": {
            "version": env!("CARGO_PKG_VERSION"),
            "api": format!("{}/api/v1", cfg.public_url),
            "apps": APPS.iter().filter(|a| cfg.apps.contains_key(**a)).collect::<Vec<_>>(),
            "desktopSessions": cfg.desktop_sessions,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::events::Event;

    fn cfg() -> Config {
        let mut c = Config { public_url: "https://live.example".into(), ..Config::default() };
        for a in APPS {
            c.apps.insert(a.into(), format!("/srv/{a}").into());
        }
        c.validate().unwrap();
        c
    }

    /// `(zone, app, action, ext, urlsrc)` of every action.
    fn actions(xml: &str) -> Vec<(String, String, String, String, String)> {
        let mut r = quick_xml::Reader::from_str(xml);
        let (mut zone, mut app) = (String::new(), String::new());
        let mut out = Vec::new();
        loop {
            match r.read_event().unwrap() {
                Event::Start(e) | Event::Empty(e) => {
                    let attr = |k: &str| {
                        e.attributes().flatten().find(|a| a.key.as_ref() == k.as_bytes()).map(|a| a.unescape_value().unwrap().into_owned()).unwrap_or_default()
                    };
                    match e.name().as_ref() {
                        b"net-zone" => zone = attr("name"),
                        b"app" => app = attr("name"),
                        b"action" => out.push((zone.clone(), app.clone(), attr("name"), attr("ext"), attr("urlsrc"))),
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        out
    }

    #[test]
    fn discovery_lists_every_format() {
        let c = cfg();
        let keys = ProofKeys::generate().unwrap();
        let xml = discovery(&c, Some(&keys));
        let a = actions(&xml);
        let edit =
            a.iter().find(|x| x.0 == "external-http" && x.1 == "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" && x.2 == "edit").unwrap();
        assert_eq!(edit.3, "");
        assert_eq!(edit.4, "https://live.example/edit/gridcraft?");
        assert!(a.iter().any(|x| x.1 == "application/vnd.oasis.opendocument.presentation" && x.2 == "view" && x.4.contains("deckcraft?mode=view&")));
        assert!(a.iter().any(|x| x.1 == "WordCraft" && x.3 == "docm" && x.2 == "edit"));
        assert!(a.iter().any(|x| x.0 == "external-https" && x.1 == "Capabilities" && x.4 == "https://live.example/hosting/capabilities"));
        assert_eq!(a.iter().filter(|x| x.0 == "external-http" && x.2 == "edit" && x.3.is_empty()).count(), 10);
        assert!(xml.contains("<proof-key oldvalue=\""));
        assert!(!discovery(&c, None).contains("proof-key"));
        let mut only = cfg();
        only.apps.remove("wordcraft");
        assert!(!discovery(&only, None).contains("wordprocessingml"));
    }

    #[test]
    fn capabilities_shape() {
        let j = capabilities(&cfg(), "abc");
        assert_eq!(j["productName"], "craft-live");
        assert_eq!(j["convert-to"]["available"], false);
        assert_eq!(j["hasSettingIframeSupport"], false);
        assert_eq!(j["craftLive"]["api"], "https://live.example/api/v1");
    }
}

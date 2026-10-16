# craft-live design

Version 0.1, 2026-10-16. craft-live is a WOPI editor ("WOPI client") that Nextcloud Office
(the `richdocuments` app) uses in place of Collabora Online, through its **Use your own server**
setting. Spreadsheets open in GridCraft, documents in WordCraft, presentations in DeckCraft, with
live co-editing. The craft apps' desktop builds join the same rooms through the
**nextcloud-live** add-in.

This is a clean-room design. It is built only from public documentation (the sources are
listed at the end) and from black-box behaviour reported in public issue text. No AGPL/GPL
source was read: not richdocuments, not Collabora Online, not LibreOffice. Anything not
confirmed by documentation is marked **ASSUMPTION** and gathered in
[§12](#12-to-verify-against-a-real-nextcloud).

Contents:

1. [Architecture](#1-architecture)
2. [Discovery and capabilities](#2-discovery-and-capabilities)
3. [The editor page](#3-the-editor-page)
4. [Rooms](#4-rooms)
5. [Seeding](#5-seeding)
6. [Saving](#6-saving)
7. [Locks, versions and conflicts](#7-locks-versions-and-conflicts)
8. [Desktop clients](#8-desktop-clients)
9. [The craft-live embed contract](#9-the-craft-live-embed-contract-for-the-apps-web-builds)
10. [Control messages](#10-control-messages)
11. [Security, limits, never-crash](#11-security-limits-never-crash)
12. [To verify against a real Nextcloud](#12-to-verify-against-a-real-nextcloud)
13. [Sources](#13-sources)

## 1. Architecture

```text
 Browser (Nextcloud Files)                          Desktop
 ┌──────────────────────────────┐                   ┌───────────────────────────┐
 │ Nextcloud page               │                   │ GridCraft/WordCraft/Deck… │
 │  └ iframe: craft-live shell  │ PostMessage API   │  └ nextcloud-live add-in  │
 │     └ iframe: app web build  │ (Collabora-style) │    (protocol 2 transport) │
 └───────────┬──────────────────┘                   └──────┬──────────┬─────────┘
             │ y-websocket (wss) + /api/v1                 │ wss      │ WebDAV/OCS
             ▼                                             ▼          ▼
 ┌─────────────────────────────────────────────────┐   ┌──────────────────────────┐
 │ craft-live                                      │   │ Nextcloud                │
 │  /hosting/discovery, /hosting/capabilities      │◄──┤  richdocuments (fetches  │
 │  /edit/<app>  (form POST: WOPISrc+access_token) │   │  discovery server-side)  │
 │  /ws/<room>   rooms: generic Yjs doc per file   │──►│  WOPI: CheckFileInfo,    │
 │  /api/v1/...  sessions, file, save              │   │  GetFile, PutFile, Lock  │
 │  data dir: room states, sessions, proof keys    │   │  (proof-signed requests) │
 └─────────────────────────────────────────────────┘   └──────────────────────────┘
```

- **Rooms.** One room per Nextcloud file. The room id is `nc:<host>:<fileid>`.
- **Generic Yjs.** The server holds each room's document as plain Yjs (`yrs`). It answers sync
  step 1, applies and relays updates, keeps awareness, and persists the whole state. It never
  depends on the apps' code, and never knows their schemas.
- **The apps do the format work.** File bytes become a Yjs document only in an app (seeding),
  and a Yjs document becomes file bytes only in an app (saving). The server asks a connected
  editor for either.
- **WOPI stays on the server.** The WOPI access token never reaches the app. Clients hold a
  craft-live session token instead.

## 2. Discovery and capabilities

Nextcloud fetches `<wopi_url>/hosting/discovery` and `<wopi_url>/hosting/capabilities` server-side.
It caches them for about an hour, and again on `occ richdocuments:activate-config` or when the
admin settings are saved [N1][N2][N4].

### Discovery

**Format.** Microsoft's format [M1][M2]: root `wopi-discovery`, then `net-zone`, `app`, `action`.

**By MIME type.** Nextcloud picks the editor by MIME type, from
`<app name="<mime>"><action name="edit" ext="" default="true" urlsrc="…?"/>`, and appends
`WOPISrc=…&title=…&lang=…&closebutton=1&revisionhistory=1` to `urlsrc` [N1][N3]. craft-live
publishes every format of every app that has a web build configured:

| App | Extensions (MIME types) |
|---|---|
| GridCraft | xlsx, xlsm, xltx, ods |
| WordCraft | docx, docm, odt |
| DeckCraft | pptx, pptm, odp |

Each format gets two actions:

- `edit` (default) → `<public_url>/edit/<app>?`
- `view` → `<public_url>/edit/<app>?mode=view&`

**By extension.** The same actions are also published per extension (Microsoft's form), under
apps named GridCraft, WordCraft and DeckCraft, for other WOPI hosts.

**Zones.** Both `external-http` and `external-https` carry identical content. Nextcloud examples
show `external-http` even for https servers [N1]. **ASSUMPTION**: Nextcloud doesn't insist on
one zone name.

**Capabilities entry.** `<app name="Capabilities"><action name="getinfo" ext=""
urlsrc="<public_url>/hosting/capabilities"/>`. Nextcloud's setup check looks for it [N4][N5].

**Proof keys.** A `<proof-key value modulus exponent oldvalue oldmodulus oldexponent/>` element,
when proof keys are on ([§11](#11-security-limits-never-crash)).

**Serve from the root of a host.** Nextcloud derives the browser-facing `public_wopi_url` from
the scheme and host of `urlsrc`. Known bugs drop a non-default port or a sub-path [N5][N6]. So
craft-live must be served from the root of its own host on 443, for example
`https://live.example.com/`. `craft-live check` warns otherwise.

### Capabilities

`/hosting/capabilities` returns Collabora-compatible JSON [N7]:

- **Turned off:** `convert-to.available: false`. Nextcloud would otherwise POST
  `/cool/convert-to/png` for previews; turn previews off with `occ config:app:set richdocuments
  preview_generation --type boolean --lazy --value false` [N8].
- **Settings iframe:** `hasSettingIframeSupport: false`. With it on, Nextcloud would embed a
  Collabora admin page and send settings URLs [N9].
- **Access check:** `hasWopiAccessCheck: true`, with `/hosting/wopiAccessCheck` implemented
  (below).
- **No templates, Zotero or signing:** `hasTemplateSource`, `hasTemplateSaveAs`,
  `hasZoteroSupport` and `hasDocumentSigningSupport` are false.
- **Mobile:** `hasMobileSupport: true`.
- **Product and version:** `productName: "craft-live"`. `productVersion` comes from
  `compat_version` (default `25.04.0.0`), because Nextcloud gates features on it and warns about
  "outdated" servers [N7]. **ASSUMPTION**: a version string is all those checks read.
- **craft-live's own key.** `craftLive: {version, api, apps, desktopSessions}`. A client that
  learns craft-live's address from Nextcloud can find the API here.

**`POST /hosting/wopiAccessCheck {callbackUrl}`** returns `{status: "Ok"|"NotHttpSuccess"|
"SslHandshake", details}` [N10]. craft-live fetches `<callbackUrl>/status.php`, but only when
`callbackUrl` is under an allowed host. Anything else would let anyone make craft-live fetch any
address. **ASSUMPTION**: Nextcloud reads only `status`.

## 3. The editor page

Nextcloud opens the editor with a form POST into its iframe, `multipart/form-data` or
urlencoded [N3]. The query carries `WOPISrc=…`; the body carries `access_token`,
`access_token_ttl` and other fields (`ui_defaults`, `css_variables`…), which craft-live ignores.
Nextcloud sends `access_token_ttl=0` [N3]. Per WOPI, the TTL is an absolute expiry in ms since
1970, and 0 means unknown [M3][M4]. Both spellings, `access_token_ttl` and `access_token_TTL`, are
accepted [M4].

`POST /edit/<app>` does this:

1. Checks `<app>`, and that `WOPISrc` is `<allowed base>/…/wopi/files/<id>` on an allowed
   Nextcloud. The `wopi_hosts` allow-list rejects a query or `..` in the WOPISrc. Anything else
   gets a 403 page.
2. Calls **CheckFileInfo** with the token. 401/404 gets a 403 page.
3. Decides the role and the person:
   - Role is `edit` when `UserCanWrite && !ReadOnly`, `mode` isn't `view` and `permission` isn't
     `readonly`; otherwise `view`.
   - The person is `UserId` + `UserFriendlyName`, with a stable colour derived from the id.
     Anonymous users get a guest id.
4. Derives the room: `nc:<host key>:<numeric file id>`. Nextcloud's WOPI file id is
   `<fileid>_<instanceid>` [N3], and its numeric part is the WebDAV `oc:fileid`, so web and
   desktop meet in one room. **ASSUMPTION**: this holds for every Nextcloud file; versions and
   shares could carry other forms. When the id isn't numeric, the room uses it whole, and the
   desktop won't meet the web.
5. Creates a session: a 32-byte random token, kept with the WOPI token and its expiry (or
   `session_hours` when the TTL is 0). It asks the room for the seed lease
   ([§5](#5-seeding)), and checks the file's `Version` against the stored state
   ([§7](#7-locks-versions-and-conflicts)).
6. Answers an HTML **shell**:
   - **Headers:** `Content-Security-Policy: frame-ancestors 'self' <allowed Nextcloud origins>
     <PostMessageOrigin>`, `Referrer-Policy: no-referrer` and `Cache-Control: no-store`.
   - **Inner frame:** the app's web build from the configured folder,
     `/apps/<app>/index.html#craft-live=1&…` ([§9](#9-the-craft-live-embed-contract-for-the-apps-web-builds)).

### PostMessage

The shell speaks Nextcloud Office's PostMessage API to `window.parent` [N11][N12][M5]:

- **Format and origin.** Messages are JSON strings `{MessageId, SendTime, Values}`, posted to
  `PostMessageOrigin` from CheckFileInfo. If that is missing, the shell uses the Nextcloud
  origin. Messages from any other origin are ignored.
- **Handshake:**
  - The shell sends `App_LoadingStatus {Status: "Frame_Ready"}` at once. Nextcloud answers
    `Host_PostmessageReady` and shows the frame.
  - The app sends `craft-live:loaded`, and the shell then sends `App_LoadingStatus {Status:
    "Document_Loaded", DocumentLoadedTime}`.
  - Nextcloud's loading timeout is the `richdocuments timeout` setting [N13]. **ASSUMPTION**:
    `Document_Loaded` clears it.
- **Editor → host:**

  | Shell sends | When the app sends |
  |---|---|
  | `Doc_ModifiedStatus {Modified}` | `craft-live:modified` |
  | `Views_List` | `craft-live:peers` |
  | `Action_Save_Resp {success}` | `craft-live:saved` |
  | `UI_Close {EverModified}` | `craft-live:close` (also the Close button of the shell's notice) |
  | `App_LoadingStatus {Status: "Failed"}` | `craft-live:error` before the document loaded |

- **Host → editor:**

  | Nextcloud sends | The shell does |
  |---|---|
  | `Action_Save` | `POST /api/v1/save` (the room saves now), and passes `craft-live:save` to the app |
  | `Action_Close`, `Close_Session` | the close flow |
  | `Grab_Focus`, `Blur_Focus` | focuses or blurs the app frame |
  | `Get_Views` | asks the app for its peers |
  | anything else, after `Host_PostmessageReady` | passes it on as `craft-live:host {message, values}` |

- **Fallbacks.** An app that never says hello within 20 s doesn't speak the embed contract yet.
  The shell then shows "This GridCraft build can't edit documents live yet…" with Close, and
  still sends `Document_Loaded`, so Nextcloud doesn't hang. A missing web build gets an
  explanation page that still answers the handshake.

**The one JavaScript exception.** The shell's script is the only JavaScript in craft-live: about
120 lines, inline, with nothing else loaded. The PostMessage API can only be spoken from script
in the page. Everything else, the apps included, is Rust.

## 4. Rooms

Each room is an actor task (`room.rs`) holding these:

- a `yrs::Doc`;
- the connections (session, Yjs client id from `hello`, abilities, synced or waiting);
- awareness, by client id;
- the seed lease;
- the WOPI tokens it may use (edit-capable web sessions, newest first, minus any token a host
  refused);
- the lock state;
- save scheduling;
- persistence.

**Wire protocol.** y-websocket, Appendix B of protocol 2 [P1]:

- `0/0` sync step 1;
- `0/1` sync step 2;
- `0/2` update;
- `1` awareness;
- `2/0` permission denied (y-websocket's auth message);
- `3` query awareness;
- **`100` craft-live control** (a JSON varstring, [§10](#10-control-messages)).

`/ws/<room>?token=…` (or `Authorization: Bearer`) needs a live session for that room. A browser
`Origin` must be craft-live's own.

**On join:**

1. The room sends `welcome`.
2. If the joiner is synced, its sync step 1 (the server's state vector), so the client sends what
   the server lacks.
3. Everyone's awareness.
4. `peers` to everyone.

**Sync and updates:**

- **Sync step 1** from a synced client gets a sync step 2: the diff against its state vector.
- **Updates** (step 2 or update) from an editor are applied and relayed as updates to the other
  synced clients.
- **Viewers.** Updates from a viewer are dropped and counted. Roles are enforced here, by the
  server.

**Awareness:**

- Awareness is relayed. A connection owns the awareness client ids it first claims, and an
  update for another connection's client is dropped (no spoofing).
- When a connection goes, its states are announced as removed (`null`, clock+1), as
  y-websocket servers do.

**Persistence:**

- The whole state goes to `rooms/<sha256>/state.yjs` at most every 2 s while edits arrive (off
  the async runtime), with `meta.json` (`seeded`, `dirty`, `version`, `name`, `wopiSrc`), and on
  shutdown. A crash loses at most the last 2 s, and the apps keep the room's state with their
  AutoRecover data [P1 §6].
- An empty room waits `close_grace_secs` (a reload, a reconnect), unlocks, persists and closes.
  The next session reopens it from disk.

**Chat.** Chat ([P2]) rides in the Yjs document (`chat`, `chatMeta`). Nothing about it is
special server-side.

**No end-to-end encryption.** Nextcloud rooms aren't end-to-end encrypted. craft-live must read
the document to answer sync and to have it saved. It is trusted by Nextcloud with the file
anyway, as Collabora is. The add-in refuses a join whose `collab/joined` says `encrypted`.

## 5. Seeding

The server can't turn file bytes into a Yjs document: that's each app's code, and repos don't
share code. So **the first editor seeds the room** with `collab.share` semantics [P1 §6]:

1. **Lease.** When a session is created and the room has no document, the room grants the
   **seed lease** to that session, if it can edit and nobody holds the lease. The lease waits
   `seed_lease_secs` (default 120) for its client to connect. It is answered as:
   - `seed: true` in the embed contract (web);
   - `mode: "share"` in `collab.join` (desktop, with `path` = the file the add-in downloaded
     through WebDAV, or no path for Share Live on the open document).
2. **Loading the file:**
   - A web seeder downloads it from `GET /api/v1/rooms/<room>/file` (craft-live proxies WOPI
     GetFile with the session's WOPI token; `X-Craft-Version` carries `X-WOPI-ItemVersion`).
   - The desktop add-in uses its own WebDAV access.
3. **Seeding.** The seeder builds the shared document in one transaction (origin `seed`) and
   connects. The server's sync step 1 (empty state vector) makes it send the whole document. The
   first update that leaves the server's document non-empty marks the room **seeded**:
   - the lease is cleared;
   - the room is not dirty, because it matches the file;
   - the state is persisted.
4. **Joiners during seeding.** Joiners that arrive meanwhile get `welcome {seeded: false}` and
   wait. Their sync step 1 is held and nothing is sent to them. They don't leave "Joining…"
   (protocol 2: read-only until the first state), so nobody edits an empty document that would
   then merge with the seed. Once seeded, each gets a sync step 2 (diff, or the whole state) and
   a sync step 1.
5. **The seeder leaves before seeding.** The lease is dropped, and waiting clients get `reseed`.
   A web app calls `POST /api/v1/prepare` and gets the lease if free. The add-in leaves with "The
   person loading the file left before it was ready. Open the file again." (an add-in can't
   change an app's join mode by itself).
6. **A client in share mode whose lease was lost** gets `welcome {seed: false, seeded: false}`
   and leaves. Two seeds would duplicate the document's content, because their ids differ.

## 6. Saving

The file bytes come from an app (`collab/snapshot {file: true}` [P1 §3]). The server decides
**when**, and **who** delivers them.

### When

The room saves when it is seeded, not paused, has unsaved edits and no save in flight, and any of
these holds:

- `autosave.updates` updates since the last save (default 200);
- `autosave.idle_secs` without an update (30);
- `autosave.max_secs` since the last save (300);
- only one editor is left and `autosave.last_editor_idle_secs` passed without an update (5).

The last rule makes sure the file is saved before the last person goes: once they're gone,
nobody can produce file bytes. An explicit save (Nextcloud's `Action_Save`, `POST /api/v1/save`,
a client's `saveNow`) runs at once. A failed save waits `retry_secs`, and a client that doesn't
deliver within `timeout_secs` counts as failed.

### Who

- **Upload mode** is used when the room has a usable WOPI token (an edit-capable web session
  that hasn't expired, and that the host hasn't refused). The room picks the editor who edited
  most recently and can `upload` (web or desktop). That client gets
  `save {id, mode: "upload", url, name, format}` and uploads the bytes:
  `PUT /api/v1/rooms/<room>/file?save=<id>` with `X-Craft-File-Name`.
- **WebDAV mode** is used otherwise (only desktop people are in the room). A desktop editor gets
  `mode: "webdav"` with `etag`, the WebDAV etag the room is based on. It saves through its own
  WebDAV access with `If-Match` on that etag (412: a copy beside it), and reports
  `saveResult {id, ok, version?|copy?|error?}`; the new etag becomes the room's. After a WOPI save
  the room doesn't know the new etag, so `etag` is empty and the next WebDAV save writes over the
  file unconditionally. That is a known gap: it applies only when the web people have left
  between two saves.

### Writing the file

On an upload, a background task writes the file with the room's tokens, newest first:

1. **Format check.** If the extension of the name the app gave differs from the file's (the app
   saved `.xlsx` for an `.ods`), the bytes go to a copy (`PutRelativeFile`) and never over the
   file.
2. **Version check.** Unless forced, CheckFileInfo is called again: the person must still be
   allowed to write. If `Version` differs from the one the room is based on, the file changed
   outside, so the bytes go to a copy (see §7).
3. **PutFile** with `X-WOPI-Lock` (when held), `X-WOPI-Editors` (the editors' user ids),
   `X-COOL-WOPI-IsAutosave` and `X-COOL-WOPI-IsModifiedByUser` [N14]. **ASSUMPTION**: Nextcloud
   uses these headers to tell autosaves from explicit saves.
   - **200:** the new version is `X-WOPI-ItemVersion` (or CheckFileInfo's).
   - **409 with an empty lock while we thought we held one:** re-Lock and retry once.
   - **409 with another lock:** save to a copy.
   - **401:** try the next token. With none left, `notice` "the editing sessions expired".
4. **After saving.** The room is clean only if no update arrived since the save was requested;
   otherwise it stays dirty for the next round.

**After a copy.** People are told ("…saved as “Budget (live copy 2026-10-16 18-02).xlsx”"), and
autosave pauses. A forced save (`/api/v1/save?force=1`) writes over the file.

**A room closed with unsaved edits** keeps them on disk (`craft-live rooms` marks them). They are
saved when someone opens the file again and the file hasn't changed meanwhile. If it has, they
are set aside (§7).

## 7. Locks, versions and conflicts

### Locks

WOPI locks are 30-minute leases with opaque ids of at most 1024 ASCII characters. They aren't
tied to a user, so any editor's token may refresh or unlock them [M6][M7]. When CheckFileInfo
says `SupportsLocks`:

- the room locks the file with `craft-live-<random>` once it has an edit token;
- it refreshes every `lock_refresh_secs` (600);
- it unlocks when the room closes.

Lock results:

| Answer | Room's lock state |
|---|---|
| 409 with another lock | `foreign`: the people are told, and saves go to a copy |
| 501, or 409 without a lock | `unsupported` |

**ASSUMPTION** about Nextcloud's own locks: Nextcloud supports WOPI locks only with the
`files_lock` app, and then ignores the client's lock value [N15]. craft-live works either way,
because PutFile without a lock is the unlocked path.

### Versions

`Version` must change with every change and never repeat [M8]:

- the room keeps the `Version` it was seeded from, or last saved;
- **when the room opens empty:** if the file's `Version` differs from the stored state's, the
  state is stale. A clean state is dropped and reseeded; a dirty one is set aside as
  `orphan-<ms>.yjs` and reseeded;
- **while the room is open:** a changed `Version` at save time sends the save to a copy.

A Nextcloud **Restore version** while the room is open is a change outside, handled the same way
(a copy, then the person decides). **ASSUMPTION**: Nextcloud's `Version` changes on every write.

## 8. Desktop clients

**No user-level API returns a WOPI token.** The research found none:

- The richdocuments OCS API `POST /ocs/v2.php/apps/richdocuments/api/v1/document {fileid}` returns
  a one-time URL to a page that embeds the editor [N16].
- The generic Direct Editing API returns a one-time URL too [N17].
- The "external apps" route `wopi/extapp/data/{fileId}` needs an admin-configured app id and
  secret [N8].

Scraping the direct page's HTML would be fragile.

**What craft-live does instead: identity sessions.** craft-live issues its own session tokens
after the client proves who it is:

1. The add-in signs in with Login Flow v2 [N18], or reuses an account that craft-nextcloud
   already signed in (same `accounts.json` format), and browses with WebDAV.
2. It sends `POST /api/v1/sessions {nextcloud, path, fileId}` with
   `Authorization: Basic <login>:<app password>`. craft-live checks that `nextcloud` is an
   allowed host, then:
   - `GET <nc>/ocs/v2.php/cloud/user` gives the user id and display name;
   - `PROPFIND <nc>/remote.php/dav/files/<user>/<path>` (depth 0) gives `oc:fileid` (it must
     equal `fileId`) and `oc:permissions` (`W` = edit).

   It then forgets the password: it is never stored or logged.
3. The answer has the same fields as the embed contract, plus `seeded`, `expires`, `app` and
   `etag`. The session lasts `session_hours`. When a reconnect is refused (401/403), the add-in
   renews the session with the same proof.
4. **Finding craft-live.** The add-in uses `CRAFT_LIVE_URL`, or the address saved in its settings,
   or the Office server Nextcloud names in `/ocs/v2.php/cloud/capabilities`
   (`richdocuments.config.wopi_url`; **ASSUMPTION** on that path), if that server's capabilities
   carry `craftLive.api`.

**Trade-off.** The app password passes through craft-live, over TLS, once per session. An
operator who doesn't want that sets `desktop_sessions = false`, and only Nextcloud Office in the
browser can open files. Desktop sessions have no WOPI token: they seed from WebDAV, and they save
in WebDAV mode when no web session is in the room.

### The add-in (nextcloud-live)

The add-in is a protocol-2 transport [P1][P3]: manifest `collab: {transport: "nextcloud-live",
label: "Live (Nextcloud)"}`, apps gridcraft, wordcraft and deckcraft, and
`requires: {protocol: ">=2.0", features: [dialogs, open, push, collab], addins: [craft-nextcloud,
optional]}`.

**Commands:**

| Command | Does |
|---|---|
| `open` | Browse Nextcloud and pick a file. Seeds from its download, or joins |
| `share {account?, path?}` | The open document goes live as its Nextcloud file. Without a path, the document must be a copy that this add-in or craft-nextcloud downloaded; the room is seeded from the open document |
| `join {link}` | From a Nextcloud link `…/f/<id>`, found with WebDAV `SEARCH` |
| `leave`, `status`, `connect`, `finishLogin`, `chooseAccount`, `settings`, `saveSettings`, `dismiss` | As named |

**Relaying.** `collab/sync1|sync2|update|awareness` map one to one onto y-websocket. The server's
sync step 2 reaches the app as `collab/sync2 {snapshot: true}`, because it is the room's stored
state. Control messages are translated:

| craft-live sends | The add-in does |
|---|---|
| `welcome` | `collab/status online`, then sends `hello {clientId, can: [upload, webdav]}` |
| `peers` | `collab/status` with peers |
| `saved`, `notice`, `saveFailed` | `collab/status` with a message |
| `reseed`, permission denied | `collab/leave` with the reason |
| `save` | `collab/snapshot {file: true, format}`, then upload or WebDAV in a worker thread |

**Connection.** The WebSocket reconnects with backoff (1–30 s); frames queue while offline, and
the app syncs again on `online`.

## 9. The craft-live embed contract (for the apps' web builds)

Until now the apps' web builds had no live editing. To work inside craft-live, a web build
implements this contract. It is the client side of [§4](#4-rooms)–[§6](#6-saving), and the same
thing protocol 2 asks of web builds ("they speak the y-websocket binding natively" [P1 §6]).

### 9.1 Detecting and configuring

The page URL is `index.html#craft-live=1&<params>`: form-encoded parameters in the **fragment**,
so the token never reaches a server log or a `Referer`.

| Param | Value |
|---|---|
| `craft-live` | `1` (contract version) |
| `ws` | WebSocket URL of the room, e.g. `wss://live.example.com/ws/nc%3Acloud.example.com%3A288`; connect to `<ws>?token=<token>` |
| `room` | the room id (protocol 2's `room`) |
| `token` | the session token (secret); also used as `Authorization: Bearer` on the API |
| `user` | JSON `{id, name, color}`: the person, from Nextcloud. Use it as the join's `user` |
| `role` | `edit` or `view`. A viewer's document is read-only (the server drops a viewer's updates anyway) |
| `seed` | `1`: this client must seed the room from the file; `0`: join |
| `file` | absolute URL of `GET` for the file bytes (seeders), with `Authorization: Bearer <token>` |
| `name` | the file name (`Budget.xlsx`) |
| `format` | its extension (`xlsx`, `ods`…): the format to save in |
| `api` | the API base (`https://live.example.com/api/v1`) |
| `lang` | Nextcloud's UI language (may be empty) |

The same configuration also arrives as a message (below). `GET <api>/session` with the bearer
token returns it again, without `token` and `seed`.

### 9.2 Talking to the shell (postMessage, same origin)

The app posts to `window.parent` with `targetOrigin = location.origin`. Messages are objects
with a `type`.

| App → shell | When |
|---|---|
| `{type: "craft-live:hello"}` | As soon as the app's script runs. Without it within 20 s, the shell says the build can't edit live |
| `{type: "craft-live:loaded"}` | The document is shown (joined, or seeded) |
| `{type: "craft-live:modified", modified}` | The room's unsaved state changed: control `modified` / `saved` / `seeded` |
| `{type: "craft-live:saved", success}` | After a save the person asked for (control `saved` or `saveFailed`) |
| `{type: "craft-live:peers", peers: [{clientId, user, role, self?}]}` | On control `peers` (include yourself with `self: true`) and on `craft-live:getPeers` |
| `{type: "craft-live:close"}` | The person closes the document (File › Close, the app's close button). The shell tells Nextcloud (`UI_Close`) |
| `{type: "craft-live:error", message}` | It can't open the document |

| Shell → app | Meaning |
|---|---|
| `{type: "craft-live:config", config}` | Answer to hello: the §9.1 parameters as an object (`seed` a boolean, `user` an object) |
| `{type: "craft-live:save", notify}` | Nextcloud asked to save (the shell has already told the server; nothing to do but show progress) |
| `{type: "craft-live:close"}` | Nextcloud closes the editor: leave the room |
| `{type: "craft-live:getPeers"}` | Answer with `craft-live:peers` |
| `{type: "craft-live:host", message, values}` | Any other Nextcloud PostMessage (`Hide_Menu_Item`…), for apps that want to honour them |

### 9.3 The room (y-websocket)

1. **Seeding** (`seed=1`): `GET <file>` with the bearer token, load the bytes as File › Open
   does, then build the shared document in one transaction (origin `seed`), exactly as
   `collab.share` does. A 0-byte file is a new, empty document of the format.
2. **Connecting:** `new WebSocket(ws + "?token=" + token)`, binary frames (lib0 encoding):
   - **Send** `hello` (control, type 100): `{"type":"hello","clientId":<Yjs client id>,
     "can":["upload"]}`.
   - **Then sync step 1** with your state vector.
   - **Answer** the server's sync step 1 with sync step 2.
   - **Apply** sync step 2 and updates with the remote origin.
   - **Send** one update per committed command (the command's whole effect), exactly as
     protocol 2's `collab/update`.
   - **Awareness** (type 1): y-protocols awareness updates, with `{user, cursor, selection,
     typing?}` in the app's selection format, at most 20 per second.
3. **Join mode** (`seed=0`): show "Joining…" read-only until the first sync step 2 arrives. While
   the room is being seeded, the server holds your sync step 1 and answers it when the seed
   arrives.
4. **Control messages** (type 100, JSON) to handle:

   | Message | What to do |
   |---|---|
   | `welcome {room, user, role, seeded, seed, modified, fileName}` | Start of the session |
   | `peers {peers: [{clientId?, user, role, kind}]}` | The others; show them, and send `craft-live:peers` |
   | `save {id, mode: "upload", url, name, format, autosave}` | Produce the document's file bytes in `format` (the file's own format; never another), then `PUT <url>` with `Authorization: Bearer <token>`, `X-Craft-File-Name: <percent-encoded name>` and the bytes as the body. It answers `200 {saved, version}`, or `{saved, copy}`, once Nextcloud has the file. Don't block editing while you do this |
   | `saved {at, modified}` / `saveFailed {message}` | Status bar; `craft-live:saved` / `modified` |
   | `modified {modified}` | Unsaved changes in the room (or none) |
   | `seeded` | The room has its document (for clients that waited) |
   | `notice {level, text}` | Show the text (for example "the file changed in Nextcloud: saved as a copy") |
   | `reseed {message}` | The seeder left before the room had a document: `POST <api>/prepare` with the bearer token; with `{seed: true}`, load the file and seed (then reconnect), else show the message |
   | `expired` | The session ended: the WebSocket closes with 4401. Tell the person to reopen the file |

   The app may also send:
   - `{"type":"saveNow"}`: the person pressed Save;
   - `{"type":"saveNow","force":true}`: Save after a copy, writing over the file in Nextcloud.
5. **Close codes:**
   - **4401:** the session expired;
   - **4403:** refused (the reason also comes as y-websocket's permission-denied message);
   - **4408:** too slow, so reconnect;
   - **anything else:** reconnect with backoff and send sync step 1 again.
6. **Chat** is part of the document ([P2]). Nothing changes.

### 9.4 What each app must implement

The same list for GridCraft, WordCraft and DeckCraft (each in its web crate, e.g.
`apps/gridcraft-web`):

1. Read the fragment, send `craft-live:hello`, and accept `craft-live:config`.
2. A y-websocket client over `web_sys::WebSocket` (binary frames, the lib0 framing above), wired
   to the existing protocol-2 collab engine. It is the same engine the desktop drives through
   `collab/*`, with the WebSocket in place of the transport plugin.
3. Seeding from bytes fetched with `fetch` + bearer token (`collab.share` semantics).
4. File bytes for `save` in the asked `format` (`collab/snapshot {file: true}` semantics),
   uploaded with `fetch` `PUT`.
5. The control messages of 9.3, and the shell messages of 9.2 (`loaded`, `modified`, `peers`,
   `close`, `error`).
6. A Close button or menu item that sends `craft-live:close`, because Nextcloud shows none of
   its own.
7. Read-only presentation for `role=view`.

The desktop apps need nothing new: nextcloud-live speaks protocol 2. Accepting the `format` hint
in `collab/snapshot` (`{room, file: true, format}`) is a small addition each app should make, so
an `.ods` or `.xltx` is saved in its own format. The server refuses to write another format over
a file anyway (§6).

## 10. Control messages

Message type 100, a JSON varstring. Summary (normative detail in §5, §6 and §9.3):

| Direction | `type` | Fields |
|---|---|---|
| server → client | `welcome` | `room, user, role, app, fileName, seeded, seed, modified, server` |
| server → client | `peers` | `peers: [{user, role, kind, clientId?}]` (the others) |
| server → client | `seeded` | `modified` |
| server → client | `save` | `id, mode (upload\|webdav), name, format, url, autosave, etag (webdav)` |
| server → client | `saved` / `saveFailed` | `at, modified` / `message` |
| server → client | `modified` | `modified` |
| server → client | `notice` | `level (info\|warning\|error), text` |
| server → client | `reseed` | `message` |
| server → client | `expired` | |
| client → server | `hello` | `clientId, can: [upload, webdav], client?` |
| client → server | `saveNow` | `force?` |
| client → server | `saveResult` | `id, ok, version?, copy?, error?` (WebDAV mode, or a failed upload) |
| client → server | `bye` | |

Unknown types are ignored and counted.

### HTTP endpoints

| Endpoint | Does |
|---|---|
| `GET /hosting/discovery`, `GET /hosting/capabilities`, `POST /hosting/wopiAccessCheck` | §2 |
| `POST /edit/<app>` | The editor page (§3) |
| `GET /apps/<app>/<path>` | The app's web build (no hidden files, no `..`, no symlinks out) |
| `GET /static/<app>.svg` | The apps' icons (drawn here) |
| `GET /ws/<room>?token=` | The room |
| `POST /api/v1/sessions` | Desktop sessions (§8) |
| `GET /api/v1/session` | The embed configuration |
| `POST /api/v1/prepare` | Ask again for the seed lease |
| `POST /api/v1/save[?force=1]` | Save now |
| `GET\|PUT /api/v1/rooms/<room>/file` | Seed download / save upload |
| `GET /api/v1/rooms/<room>/status` | The room's state, for people in it |
| `GET /healthz` | Health check |

## 11. Security, limits, never-crash

**Allow-list.** `wopi_hosts` names the Nextcloud servers that may open files, so a WOPISrc on
another host is refused before any request is made. Nextcloud itself restricts which servers may
call its WOPI endpoints with `wopi_allowlist`, which must hold craft-live's outgoing address
[N8][N19].

**Proof keys.** Microsoft's algorithm [M9][M10]:

- **Key:** a 2048-bit RSA key in `data_dir/proof-key.pem`, made on first start. `craft-live
  rotate-proof-key` keeps the old key as `proof-key-old.pem`.
- **Signed data:** the token's length (4 bytes BE) + the token, the URL's length + the absolute
  URL in upper case (with `access_token`), then 8 + the timestamp (64-bit BE .NET ticks).
- **Signature:** RSA PKCS#1 v1.5 SHA-256 in `X-WOPI-Proof` (current key) and `X-WOPI-ProofOld`
  (old key), with `X-WOPI-TimeStamp`.
- **Discovery:** `modulus`/`exponent` are base64 big-endian; `value` is a CryptoAPI
  PUBLICKEYBLOB (bType 6, version 2, aiKeyAlg CALG_RSA_KEYX, "RSA1", bit length, exponent, then
  the modulus little-endian).
- **ASSUMPTIONS:** the timestamp's byte order isn't stated, and big-endian is inferred from the
  length fields. The aiKeyAlg Microsoft uses isn't documented.
- **Nextcloud** verifies proofs only when a request carries one, and refuses a bad one [N20]. If
  Nextcloud refuses craft-live's proofs, `proof_keys = false` turns them off.
- **Tests:** a host-side check written independently in the fake Nextcloud (`testkit`) verifies
  every request.

**Tokens:**

- The WOPI token stays server-side. It is sent as both the `access_token` query parameter and
  `Authorization: Bearer` [M3][M11].
- Session tokens are 256-bit random and compared in constant time. They are kept in
  `sessions.json` (owner-only, as are room states and keys).
- A web session expires with its WOPI token's TTL, or after `session_hours` when the TTL is 0.
  The room then sends `expired` and closes with 4401.

**Pages.** The editor page sends `frame-ancestors` for the allowed Nextcloud origins only,
`no-referrer` and `no-store`. The app builds may only be framed by craft-live itself.

**Limits:**

| What | Limit |
|---|---|
| A frame | 16 MiB update + 64; WebSocket messages 17 MiB |
| Awareness | 64 KiB, 4096 entries, 64 client ids per connection |
| A state vector | 1 MiB |
| Files | `max_file_mb` (100) |
| Rooms | `max_rooms` (1000) |
| Connections per room | 100 |
| Queued messages per connection | 4096: a client that falls behind is closed (4408) |
| CheckFileInfo answers | 1 MiB |

**Never crash:**

- Crates deny `unwrap`, `expect`, `panic`, indexing and the like, and forbid `unsafe`.
- Every decoder (lib0, awareness, multistatus, JSON) is bounded.
- yrs decoding runs under `catch_unwind`.
- A room is an actor, so a failure there costs that room (it reloads from disk), never the
  server.
- Hostile input is dropped and counted (`/api/v1/rooms/<room>/status` → `dropped`), and tests
  feed it on purpose.

## 12. To verify against a real Nextcloud

Check each of these against a live Nextcloud (with CODE as a reference where useful), by
capturing traffic:

1. Nextcloud accepts the discovery: MIME apps with `ext=""`, both zones, the Capabilities entry,
   and `productVersion` from `compat_version`. Its setup checks are all green
   (`occ richdocuments:activate-config`).
2. Which MIME type Nextcloud's "valid mimetype" check probes. craft-live must publish it, so the
   check fails when GridCraft isn't configured.
3. The form fields of the editor POST, multipart vs urlencoded, and the WOPISrc form
   `…/wopi/files/<fileid>_<instance>`. Do versions or shares produce other forms?
4. `PostMessageOrigin` in Nextcloud's CheckFileInfo. Does `Document_Loaded` clear the loading
   timeout? Which PostMessages does Nextcloud handle beyond `UI_Close`, `Doc_ModifiedStatus` and
   `Views_List` (`UI_FileVersions`, `UI_Share`, `File_Rename`)?
5. Proof keys. Nextcloud verifies craft-live's signatures (CSP blob `aiKeyAlg`, timestamp byte
   order), and refetches the discovery after a rotation.
6. Locks. `SupportsLocks` with and without `files_lock`, what Lock/Unlock answer, and PutFile
   while locked.
7. `Version`. Does it change on every write (PutFile, WebDAV upload, version restore), and does
   `X-WOPI-ItemVersion` come back?
8. `PutRelativeFile` in suggested mode (UTF-7 names), and `UserCanNotWriteRelative`.
9. `X-COOL-WOPI-IsAutosave`: does Nextcloud keep explicit saves as versions?
10. Token lifetime. `token_ttl` defaults, and what Nextcloud answers when a token expires (401?).
11. `/hosting/wopiAccessCheck`. Is it called from the browser or server-side, and which statuses
    are shown?
12. `/ocs/v2.php/cloud/capabilities`: the path of the configured Office server
    (`richdocuments.config.wopi_url`?) for the add-in's discovery of craft-live.
13. WebDAV `SEARCH` by `oc:fileid` with app passwords, and `oc:permissions` for shared files
    (`W` for edit shares).
14. Mobile clients (the richdocuments direct-editing page) opening craft-live's editor page.
15. `wopi_allowlist` with craft-live's outgoing address, and craft-live served behind a reverse
    proxy (WebSocket upgrade, `X-Forwarded-*`).

## 13. Sources

Microsoft WOPI (CSPP = Cloud Storage Partner Program documentation, MS-WOPI = the open
specification):

- [M1] Discovery: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/online/discovery
- [M2] [MS-WOPI] discovery elements: https://learn.microsoft.com/en-us/openspecs/office_protocols/ms-wopi/665e6bb3-28bd-444a-8131-40a1b922f128 (wopi-discovery), …/289042fd-9f15-41fb-a76d-1d4850be402c (net-zone), …/219bb42e-f63b-4f54-a354-18b76dc8f7f0 (app), …/01f8f72c-cb5b-48be-b747-4bb35cbd0d4f (action), …/bf6d73f5-03a2-40ef-8842-ae390ed3d49e (zone names), …/4a615210-43f8-4491-837b-ec1b49f2ca75 (proof-key)
- [M3] Key concepts (tokens, TTL, locks): https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/rest/concepts
- [M4] Host page / token POST: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/online/hostpage ; [MS-WOPI] https://learn.microsoft.com/en-us/openspecs/office_protocols/ms-wopi/adb48ba9-118a-43b6-82d7-9a508aad1583
- [M5] PostMessage: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/online/scenarios/postmessage
- [M6] Lock, RefreshLock, Unlock, GetLock, UnlockAndRelock: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/rest/files/lock (and …/refreshlock, …/unlock, …/getlock, …/unlockandrelock)
- [M7] Co-authoring: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/online/scenarios/coauth
- [M8] CheckFileInfo: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/rest/files/checkfileinfo/checkfileinfo-response ; GetFile …/rest/files/getfile ; PutFile …/rest/files/putfile ; PutRelativeFile …/rest/files/putrelativefile
- [M9] Proof keys: https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/online/scenarios/proofkeys
- [M10] CryptoAPI key blobs: https://learn.microsoft.com/en-us/windows/win32/seccrypto/base-provider-key-blobs ; https://learn.microsoft.com/en-us/windows/win32/api/wincrypt/ns-wincrypt-publickeystruc ; https://learn.microsoft.com/en-us/windows/win32/api/wincrypt/ns-wincrypt-rsapubkey
- [M11] [MS-WOPI] authorization header: https://learn.microsoft.com/en-us/openspecs/office_protocols/ms-wopi/70254fa6-9d3d-4f93-9b1a-e6597a63b900 ; common headers https://learn.microsoft.com/en-us/microsoft-365/cloud-storage-partner-program/rest/common-headers

Nextcloud Office / Collabora (documentation pages, and the text of issues and pull requests; no
source code). sdk.collaboraonline.com was unreachable to the research tools (bot challenge), so
the PostMessage list comes from a public fork of the same documentation [N11] and Collabora's
FOSDEM talk [N12]:

- [N1] Discovery parsing and caching (issue/PR text): https://github.com/nextcloud/richdocuments/pull/3252
- [N2] Discovery and capabilities caching: https://github.com/nextcloud/richdocuments/pull/1226 ; https://github.com/nextcloud/richdocuments/pull/5367
- [N3] The editor POST and WOPISrc form: https://github.com/nextcloud/richdocuments/issues/3573 ; https://github.com/nextcloud/richdocuments/issues/5279 ; https://github.com/nextcloud/richdocuments/issues/4611 ; https://github.com/nextcloud/richdocuments/blob/main/docs/frontend-integration.md
- [N4] Setup checks: https://github.com/nextcloud/richdocuments/issues/4083 ; https://github.com/nextcloud/richdocuments/pull/4470
- [N5] `public_wopi_url` from discovery, `occ richdocuments:setup`: https://github.com/nextcloud/richdocuments/pull/3315 ; https://github.com/nextcloud/richdocuments/pull/5639
- [N6] Sub-path lost: https://github.com/nextcloud/richdocuments/issues/4696
- [N7] Capabilities samples: https://github.com/nextcloud/all-in-one/discussions/3378 ; https://forum.collaboraonline.com/t/page-or-api-call-to-retrieve-version/2987 ; https://help.nextcloud.com/t/collabora-healthcheck/241738 ; https://github.com/nextcloud/richdocuments/issues/4349
- [N8] App settings (preview generation, external apps, allow list): https://github.com/nextcloud/richdocuments/blob/main/docs/app_settings.md ; https://github.com/nextcloud/richdocuments/issues/4766
- [N9] Settings iframe: https://github.com/nextcloud/richdocuments/issues/4588 ; https://github.com/nextcloud/richdocuments/pull/4373
- [N10] wopiAccessCheck: https://github.com/CollaboraOnline/online/pull/9202 ; https://github.com/CollaboraOnline/online/pull/10840
- [N11] PostMessage API (documentation fork): https://docs.ossii.com.tw/books/oxoffice-online-8LU/page/postmessage-api
- [N12] Collabora Online integration, FOSDEM 2020: https://archive.fosdem.org/2020/schedule/event/integrate_collabora_online_with_web_applications/
- [N13] Loading timeout: https://github.com/nextcloud/richdocuments/issues/1237 ; https://github.com/nextcloud/richdocuments/issues/2188
- [N14] PutFile autosave headers (FOSDEM slides above)
- [N15] Locks need files_lock: https://github.com/nextcloud/richdocuments/issues/2835 ; https://github.com/nextcloud/richdocuments/issues/5627 ; https://github.com/nextcloud/richdocuments/issues/5962
- [N16] Mobile editor API: https://github.com/nextcloud/richdocuments/blob/main/docs/mobile_editor.md ; https://github.com/nextcloud/richdocuments/blob/main/docs/mobile_flow.md ; https://github.com/nextcloud/richdocuments/issues/5839
- [N17] Direct Editing: https://docs.nextcloud.com/server/stable/developer_manual/digging_deeper/direct_editing.html
- [N18] Login Flow v2: https://docs.nextcloud.com/server/latest/developer_manual/client_apis/LoginFlow/index.html
- [N19] Office configuration and troubleshooting: https://docs.nextcloud.com/server/latest/admin_manual/office/configuration.html ; https://docs.nextcloud.com/server/latest/admin_manual/office/troubleshooting.html ; https://github.com/nextcloud/richdocuments/blob/main/docs/install.md
- [N20] Proof verification in Nextcloud: https://github.com/nextcloud/richdocuments/pull/4902 ; https://github.com/nextcloud/richdocuments/issues/5351

craft documents:

- [P1] Craft plugin protocol 2: https://github.com/sirmmo/craft-collab/blob/main/docs/protocol-2.md
- [P2] Document chat: https://github.com/sirmmo/craft-collab/blob/main/docs/chat.md
- [P3] Plugins, manifest 1.3 requirements: https://github.com/sirmmo/craft-collab/blob/main/docs/plugin-requirements.md

Note on the market: since mid-2026, Nextcloud's default "Nextcloud Office" is the Euro-Office
connector, and `richdocuments` is listed as "Nextcloud Office (Collabora)"
(https://docs.nextcloud.com/server/latest/admin_manual/office/euro-office/index.html). craft-live
targets `richdocuments` and its `wopi_url` setting.

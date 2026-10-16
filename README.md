# craft-live

Nextcloud Office with the craft apps. craft-live is a WOPI editor server: Nextcloud Office (the
`richdocuments` app) uses it in place of Collabora Online, through its **Use your own server**
setting. Spreadsheets then open in **GridCraft**, documents in **WordCraft** and presentations in
**DeckCraft**, with live co-editing. People in the craft apps' desktop builds join the same
sessions through the **nextcloud-live** add-in.

| | What |
|---|---|
| [`craft-live`](crates/server) | The server: discovery and capabilities for Nextcloud, the editor page, rooms over y-websocket, saving through WOPI |
| [`nextcloud-live`](crates/nextcloud-live) | The desktop add-in: a craft plugin protocol 2 transport that joins craft-live rooms |
| [`craft-live-testkit`](crates/testkit) | Tests: a fake Nextcloud (WOPI host, WebDAV, OCS, Login Flow) and a y-websocket client that plays an app |
| [docs/design.md](docs/design.md) | How it works, the **embed contract** the apps' web builds implement, what's assumed, the sources |
| [docs/deploy.md](docs/deploy.md) | Running it, configuring Nextcloud, TLS and proxies, the apps' web builds |

## Architecture

```text
 Nextcloud Files (browser)                    Desktop app (GridCraft, WordCraft, DeckCraft)
   iframe: craft-live editor page               nextcloud-live add-in (protocol 2 transport)
     iframe: the app's web build                  │ Login Flow v2, WebDAV │ wss + /api/v1
       │ y-websocket + /api/v1                    │                       │
       ▼                                          ▼                       ▼
 ┌───────────────────────────────────────────────────┐            ┌────────────────────┐
 │ craft-live                                        │  WOPI      │ Nextcloud          │
 │  /hosting/discovery, /hosting/capabilities ◄──────┼────────────┤  richdocuments     │
 │  /edit/<app>  (WOPISrc + access_token)            │  (proof-   │  WOPI: CheckFile-  │
 │  /ws/<room>   one room per file: a generic Yjs    ├──signed)──►│  Info, GetFile,    │
 │               document, relayed and persisted     │            │  PutFile, Lock     │
 │  data dir     room states, sessions, proof keys   │            │  OCS, WebDAV       │
 └───────────────────────────────────────────────────┘            └────────────────────┘
```

**Rooms.** A room is one Nextcloud file (`nc:<host>:<fileid>`). Web and desktop people meet in
the same room.

**What the server does and doesn't do.** The server reads the shared document only as generic
Yjs (sync, updates, awareness, persistence). It never depends on the apps' code. File bytes
become a document, and a document becomes file bytes, only in an app:

- **Seeding.** The first editor gets the **seed lease**, loads the file and seeds the room.
- **Saving.** When the room saves (after N edits, when idle, or before the last editor is alone
  to go), a connected editor hands over the file bytes. craft-live writes them with WOPI PutFile
  under its lock. With only desktop people in the room, the add-in saves through its own WebDAV
  access.

**Conflicts.** A file changed in Nextcloud meanwhile, someone else's lock, or bytes in another
format are never written over the file: they go to a copy beside it, and people are told.

## Status (0.1.0)

**Works now, and tested against a fake Nextcloud written from the WOPI documentation:**

- **For Nextcloud:**
  - discovery (by MIME type and by extension, view and edit, the Capabilities entry, proof keys);
  - capabilities;
  - `wopiAccessCheck`;
  - the editor page (multipart or urlencoded POST, the WOPISrc allow-list, CheckFileInfo roles,
    the PostMessage handshake).
- **Rooms:**
  - y-websocket sync with roles enforced (viewers' updates dropped);
  - awareness without spoofing;
  - seeding with a lease, and handing it over when the seeder leaves;
  - persistence across restarts;
  - a file changed outside while the room was closed (stale states reseeded, unsaved edits set
    aside).
- **Saving:**
  - autosave and explicit saves;
  - WOPI Lock/RefreshLock/Unlock;
  - PutFile, with 409 and a lapsed lock handled;
  - PutRelativeFile copies;
  - expired tokens;
  - WebDAV saves from desktops.
- **Proof keys:** signed exactly as Microsoft documents, and checked by an independent host-side
  check in the tests.
- **Desktop add-in:**
  - identity sessions (app password, checked once with Nextcloud);
  - Open (browse), Share Live, Join by link, Leave, Status, Settings;
  - `collab/*` relayed over y-websocket;
  - reconnects with renewal;
  - saves through `collab/snapshot {file: true}`.

  It passes craft-collab's transport conformance checker.

**Waiting for the apps:**

- **Web builds.** The apps' web builds need to speak the
  [embed contract](docs/design.md#9-the-craft-live-embed-contract-for-the-apps-web-builds)
  (y-websocket, seeding from the file, file bytes for saves, a few postMessages). Until they do,
  the editor page loads the build and says it can't edit live yet, and Nextcloud closes cleanly.
- **The `format` hint.** The desktop apps work through nextcloud-live today. Accepting the
  `format` hint in `collab/snapshot` keeps `.ods`/`.xltx` files in their own format; otherwise
  craft-live saves those as a copy.

**To verify against a real Nextcloud:** the list is in
[design §12](docs/design.md#12-to-verify-against-a-real-nextcloud). It covers discovery checks,
form fields, PostMessage handling, proof-key details, locks with `files_lock`, version semantics
and the capabilities path the add-in reads.

## Security model

**Who may open files.**
- Only the Nextcloud servers in `wopi_hosts`. A WOPISrc elsewhere is refused before any request.
- Nextcloud, in turn, accepts WOPI calls only from craft-live's address (`wopi_allowlist`), and
  craft-live signs every request with WOPI proof keys.

**Tokens.**
- The WOPI access token stays on the server. Browsers and add-ins hold a craft-live session
  token: 256-bit random, bound to one person and one file, and expiring with the WOPI token (or
  after `session_hours`).
- Roles come from Nextcloud (CheckFileInfo `UserCanWrite`, WebDAV `oc:permissions`) and are
  enforced by the server.

**Desktop sessions.**
- They prove the person's Nextcloud identity with their app password. craft-live asks Nextcloud
  (OCS user, WebDAV PROPFIND of the file) and forgets the password.
- `desktop_sessions = false` turns this off.

**Not end-to-end encrypted.** craft-live reads the documents, as Collabora does, to sync and save
them. The data folder holds document states and live tokens; it is owner-only.

**Hostile input.** It is dropped and counted, never a crash:
- the crates deny `unwrap`, `expect`, `panic` and indexing, and forbid `unsafe`;
- decoders are bounded, and Yjs decoding is isolated;
- each room is an actor, so a failure costs that room, which reloads from disk.

## Building and testing

```sh
cargo build --release -p craft-live -p nextcloud-live
cargo test --workspace
# craft-collab's transport checker against the add-in:
CRAFT_COLLAB_CONFORMANCE=/path/to/craft-collab-conformance cargo test -p nextcloud-live --test e2e craft_collab_conformance
nextcloud-live --manifest > crates/nextcloud-live/plugin.json   # after changing the manifest
```

Rust only, apart from the editor page's small inline script: the PostMessage API can only be
spoken from script in the page ([design §3](docs/design.md#3-the-editor-page)). This is a
clean-room implementation from public documentation; no AGPL/GPL code was read or used.

## License

MIT OR Apache-2.0.

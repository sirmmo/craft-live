# Deploying craft-live

craft-live replaces Collabora Online for Nextcloud Office (the `richdocuments` app). Its
spreadsheets, documents and presentations then open in GridCraft, WordCraft and DeckCraft.
Nextcloud's admin setting **Office › Use your own server** points at craft-live.

What it needs:

1. **A host of its own**, with TLS, at the root: `https://live.example.com/`. Nextcloud derives the
   editor's browser address from the scheme and host of the discovery's URLs, and drops a path
   or a non-default port ([design §2](design.md#2-discovery-and-capabilities)).
2. **Network access both ways:**
   - Nextcloud reaches craft-live, for discovery and capabilities (server-side).
   - craft-live reaches Nextcloud, for the WOPI calls to the WOPISrc that Nextcloud sends.
   - Browsers reach both, including WebSockets to craft-live.
3. **The craft apps' web builds**, one folder each ([below](#the-apps-web-builds)).
4. **A data folder.** It holds the rooms' state, the sessions (with WOPI tokens) and the proof
   keys. Keep it and back it up; it is owner-only.

## Running it

**Binary.** Each release has `craft-live-<version>-<platform>.tar.gz`. The Linux binary is
static.

```sh
craft-live check --config /etc/craft-live.toml   # prints the configuration and what's missing
craft-live --config /etc/craft-live.toml         # serves
```

**Docker:**

```sh
docker build -t craft-live .
docker run -d --name craft-live -p 127.0.0.1:9980:9980 \
  -v craft-live-data:/data -v /srv/craft-apps:/apps:ro \
  -e CRAFT_LIVE_PUBLIC_URL=https://live.example.com \
  -e CRAFT_LIVE_WOPI_HOSTS=https://cloud.example.com \
  craft-live
```

In the image, the apps' builds are expected in `/apps/gridcraft`, `/apps/wordcraft` and
`/apps/deckcraft`, the data goes in `/data`, and the server runs as uid 10001. A full example
with Nextcloud, Postgres and Caddy is in
[`deploy/docker-compose.yml`](../deploy/docker-compose.yml).

**Commands:**

| Command | Does |
|---|---|
| `craft-live rooms` | Lists the stored rooms. `*` marks unsaved edits (they're saved when someone opens the file again) |
| `craft-live rotate-proof-key` | Makes a new proof key and keeps the previous one valid. Afterwards, restart and run `occ richdocuments:activate-config` |

### Configuration

The configuration is a TOML file (`--config`, or `CRAFT_LIVE_CONFIG`). Environment variables
override it. [`craft-live.example.toml`](../craft-live.example.toml) shows every key.

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `listen` | `CRAFT_LIVE_LISTEN` | `127.0.0.1:9980` | Address and port |
| `public_url` | `CRAFT_LIVE_PUBLIC_URL` | `http://127.0.0.1:9980` | The address browsers and Nextcloud use |
| `data_dir` | `CRAFT_LIVE_DATA_DIR` | `craft-live-data` | Rooms, sessions, keys |
| `wopi_hosts` | `CRAFT_LIVE_WOPI_HOSTS` (comma-separated) | none | The Nextcloud servers allowed to open files (base URLs, with a path if Nextcloud has one) |
| `proof_keys` | `CRAFT_LIVE_PROOF_KEYS` | `true` | Sign WOPI requests and publish the keys |
| `verify_tls` | `CRAFT_LIVE_VERIFY_TLS` | `true` | Check Nextcloud's certificate |
| `desktop_sessions` | `CRAFT_LIVE_DESKTOP_SESSIONS` | `true` | Let the desktop add-in open files ([design §8](design.md#8-desktop-clients)) |
| `apps.gridcraft` … | `CRAFT_LIVE_APP_GRIDCRAFT`, `…_WORDCRAFT`, `…_DECKCRAFT` | none | Each app's web build folder |
| `compat_version` | | `25.04.0.0` | The version `/hosting/capabilities` reports |
| `[autosave]` | | | `updates` 200, `idle_secs` 30, `max_secs` 300, `last_editor_idle_secs` 5, `retry_secs` 30, `timeout_secs` 120 |
| `[limits]` | | | `max_file_mb` 100, `max_rooms` 1000, `max_connections_per_room` 100, `session_hours` 10, `lock_refresh_secs` 600, `close_grace_secs` 30, `seed_lease_secs` 120 |

Logs go to standard error. `CRAFT_LIVE_LOG` sets the level, as a tracing filter: `info`,
`debug`, `craft_live=debug`.

## Configuring Nextcloud

With the `richdocuments` app ("Nextcloud Office (Collabora)" in recent app stores):

1. **Install the app.** `occ app:install richdocuments`, or enable it in Apps.
2. **Point it at craft-live.** In the admin UI: **Administration settings › Office › Use your own
   server**, URL `https://live.example.com`. With `occ`:

   ```sh
   occ config:app:set richdocuments wopi_url --value=https://live.example.com
   occ richdocuments:activate-config
   ```

   `occ richdocuments:setup --wopi-url=https://live.example.com` does the same on versions that
   have it. `activate-config` fetches the discovery and capabilities again, and prints the checks.
3. **Allow craft-live to call Nextcloud's WOPI endpoints.** In the admin UI: **Allow list for WOPI
   requests**, or with `occ`:

   ```sh
   occ config:app:set richdocuments wopi_allowlist --value=<craft-live's outgoing IP or CIDR>
   ```

   "Unauthorized WOPI host" in Nextcloud's log means this list is wrong. Behind a reverse proxy,
   Nextcloud sees the proxy's address unless `trusted_proxies` is set.
4. **Optional: a callback address.** When craft-live should reach Nextcloud at an address other
   than the browsers' (an internal name), set `occ config:app:set richdocuments wopi_callback_url
   --value=https://nextcloud.internal`, and put that address in craft-live's `wopi_hosts` as well.
5. **Turn previews from the Office server off.** craft-live doesn't convert documents:

   ```sh
   occ config:app:set richdocuments preview_generation --type boolean --lazy --value false
   ```

6. **Self-signed certificates** (tests only): Nextcloud's `occ config:app:set richdocuments
   disable_certificate_verification --value=yes`, and craft-live's `verify_tls = false`.

The formats craft-live publishes depend on which web builds are configured. A format whose app
isn't installed stays with Nextcloud's other editors.

**Checking.** `curl https://live.example.com/hosting/discovery` and `…/hosting/capabilities`
from the Nextcloud host must work; so must `curl https://cloud.example.com/status.php` from the
craft-live host [Nextcloud's troubleshooting page]. Then open a spreadsheet in Files. The things
still to verify on a real Nextcloud are listed in
[design §12](design.md#12-to-verify-against-a-real-nextcloud).

**Discovery is cached.** Nextcloud keeps it for about an hour. After changing craft-live's apps
or keys, run `occ richdocuments:activate-config`.

## TLS and reverse proxies

Terminate TLS in a reverse proxy, and forward to `listen`.

**WebSockets.** Pass the upgrade on `/ws/`. Allow long-lived connections: craft-live pings every
30 s, and drops a connection silent for 120 s.

**nginx:**

```nginx
server {
    listen 443 ssl http2;
    server_name live.example.com;
    ssl_certificate     /etc/letsencrypt/live/live.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/live.example.com/privkey.pem;
    client_max_body_size 110m;            # saves upload whole files (max_file_mb + room)

    location /ws/ {
        proxy_pass http://127.0.0.1:9980;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
    }
    location / {
        proxy_pass http://127.0.0.1:9980;
        proxy_set_header Host $host;
        proxy_read_timeout 300s;          # a save answers once Nextcloud has the file
    }
}
```

**Caddy:** `live.example.com { reverse_proxy 127.0.0.1:9980 }` (see `deploy/Caddyfile`).

**Pages and framing.**
- craft-live sends `Content-Security-Policy: frame-ancestors` for the allowed Nextcloud origins.
  Don't add `X-Frame-Options` in the proxy.
- Serve `.wasm` as `application/wasm`. craft-live does; a proxy shouldn't rewrite it.
- Compression (gzip, Brotli) of `.wasm` and `.js` in the proxy helps a lot.

**The browser's WebSocket** must come from craft-live's own origin (the editor page is served by
craft-live). Keep `public_url` equal to the address browsers see.

## The apps' web builds

Each craft app builds a static web version (Rust compiled to wasm, built with `trunk`):

| App | Build | Output |
|---|---|---|
| GridCraft | `packaging/web/package.sh` in the GridCraft repository (needs `trunk` and the `wasm32-unknown-unknown` target) | `dist/web/` (and `dist/gridcraft-web-<version>.zip`); the web crate is `apps/gridcraft-web` |
| WordCraft | `packaging/web/package.sh` | `dist/web/`, `apps/wordcraft-web` |
| DeckCraft | `packaging/web/package.sh` | `dist/web/`, `apps/deckcraft-web` |

Each release of an app also carries the zip `<app>-web-<version>.zip`. Unzip it, then point
`apps.<app>` at the folder holding `index.html`. craft-live serves it at `/apps/<app>/`, so the
build's relative URLs work as they are (`public_url = "./"` in each `Trunk.toml`).

**Live editing needs app support.** A build works inside craft-live only once it implements the
[craft-live embed contract](design.md#9-the-craft-live-embed-contract-for-the-apps-web-builds).
Until then, the editor page loads it and then says that the build can't edit live yet; Nextcloud
gets a proper close.

## The desktop add-in (nextcloud-live)

People using the desktop apps install **nextcloud-live**: Options › Add-ins › install from GitHub,
`sirmmo/craft-live`, or the release zip for their platform. In the add-in:

1. **Connect** (Login Flow v2 in the browser), or reuse an account the craft-nextcloud add-in
   already has.
2. **Open Live** opens a Nextcloud file; **Share Live** puts the open Nextcloud document live;
   **Join Live** takes a file's `…/f/<id>` link.
3. **The craft-live address** is found through Nextcloud's capabilities when it can be. Otherwise
   set it in **Live Settings** (it's the Office server address), or in the environment variable
   `CRAFT_LIVE_URL`.

**Desktop sessions** prove the person's identity to craft-live with their app password, once per
session: craft-live asks Nextcloud and then forgets it. Set `desktop_sessions = false` to allow
only the browser.

## Security checklist

- **`wopi_hosts`** lists only your Nextcloud(s). Any other WOPISrc is refused before a request is
  made.
- **Nextcloud's `wopi_allowlist`** lists only craft-live's address.
- **Proof keys are on** (`proof_keys = true`, the default), so Nextcloud can tell craft-live's
  requests from anyone else's.
- **The data folder** is owner-only, on an encrypted disk if documents are sensitive. It holds
  document states and live WOPI tokens.
- **Rooms aren't end-to-end encrypted.** craft-live reads the documents, as Collabora does.
- **Limits:** see `[limits]`. Large files (`max_file_mb`) take memory on the server and on every
  client.

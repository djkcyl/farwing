# OpenUUYC self-host signaling server

This service provides local device pairing, host presence, and signaling for the Windows host adapter and iOS or web controllers. It uses Node.js 22.13 or newer, built-in SQLite, Socket.IO 4, and `ws`. The server makes no NetEase or other upstream account calls.

## Install and run

```sh
cd server
npm ci
npm start
```

By default the service listens on `127.0.0.1:8787`, uses `server/data/devices.sqlite`, and advertises loopback `ws://` signaling URLs for local verification. The SQLite file contains device token hashes and is created with restricted directory permissions. Back it up while the service is stopped or use SQLite's online backup facilities.

## Enroll devices

Create a host pairing code and enter it in the host adapter:

```sh
node server/cli.js pair-code --role host
```

After the host is paired, use its 16-character device id to make a controller code. A controller code is permanently bound to that single enrolled host:

```sh
node server/cli.js list
node server/cli.js pair-code --role controller --host DEVICE_ID
```

Pair codes are random, single-use, and expire after 10 minutes by default. Change the lifetime with `--ttl-minutes`. The local operator CLI can list and revoke devices:

```sh
node server/cli.js list
node server/cli.js revoke DEVICE_ID
```

Revocation closes active sessions and host rooms within the server's short maintenance interval. It also invalidates existing bearer tokens and pairing relationships.

## HTTPS deployment

For direct TLS, set `HOST`, `PORT`, `TLS_CERT_FILE`, `TLS_KEY_FILE`, and `PUBLIC_ORIGIN` to the externally reachable HTTPS origin. The server then advertises `wss://` signaling URLs.

For a reverse proxy, keep the server bound to loopback and set `PUBLIC_ORIGIN=https://your-hostname`. Configure the proxy to pass `Upgrade` and `Connection` headers for `/socket.io/` and `/v1/session`. The server trusts the configured origin and does not infer public addresses from forwarded headers. Plain HTTP cannot bind to a non-loopback interface.

The browser signaling endpoint checks the browser `Origin` against `PUBLIC_ORIGIN`; native clients may omit `Origin`. Configure a trusted TLS endpoint before exposing controller sessions on a network.

## API outline

- `POST /v1/pair` consumes an operator-issued code and returns a device id and device bearer token once.
- `POST /v1/host/room` creates a 24-hour host room ticket and `/socket.io/` URL. Native host reconnects use `X-NRD-AUTH`, `X-NRD-CONTROLLING: 0`, and the optional reconnect key.
- `PUT /v1/host/controllable` changes whether the host accepts controllers.
- `GET /v1/devices` returns the caller's authorized host record (and, for a host, its paired controllers).
- `POST /v1/sessions` creates a short-lived, single-use controller ticket for the controller's one bound host.
- `GET /healthz` is an unauthenticated process health check.

Device bearer credentials, room tickets, and session tickets are separate. The controller ticket expires after one minute if it is not used to authenticate a `/v1/session` WebSocket. Only one controller session may be active for a host at a time. LAN sessions use no STUN or TURN servers.

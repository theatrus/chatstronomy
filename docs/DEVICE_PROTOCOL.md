# Observatory device protocol v1

AutoPierCam is a user-owned `pier_camera`, not a N.I.N.A. telescope/profile.
The Hub's **Telescopes** tab includes a separate **Pier cameras & devices**
section. Create a camera, generate a code, and explicitly choose its channels.
Register the server under **Discord delivery** first. The owner must also manage
that server; a live bot membership and text-channel check is required.

Camera feeds can share a telescope's channel without changing slash-command
routing. A server manager can remove a camera feed from Discord delivery but
cannot pair, revoke or delete another user's camera. Device ownership does not
grant telescope or N.I.N.A. permissions. Share codes do not authorize devices.

## Pairing

Use HTTPS (loopback HTTP only in development). Secrets never go in URLs.
`POST /v1/devices/pair`, JSON, at most 4096 bytes:

```json
{
  "protocol_version": 1,
  "kind": "pier_camera",
  "installation_id": "9c18405b-ddcc-4a31-9010-e19ccf1bb567",
  "pairing_token": "csdp_<owner-issued-secret>"
}
```

Response: `{"protocol_version":1,"device_id":42,"credential":"csdc_<secret>"}`.
The installation UUID is persistent per AutoPierCam installation, not a camera
serial, N.I.N.A. profile or Windows username. Protect the returned credential
with the operating system's credential store. Do not write secrets into logs,
configuration files, command-line arguments, crash reports or screenshots.

Codes expire after one hour and are consumed atomically with credential creation.
A new code invalidates an earlier unused code. Pairing rotates the previous
credential. Revoke removes both the credential and unused code. Only hashes
are stored on the Hub. Credentials bind to the installation UUID and device;
telescope credentials and codes are not interchangeable with device secrets.
Invalid credentials/codes get 401, unsupported versions get 400, and abusive
pairing attempts get 429. Pairing responses are `Cache-Control: no-store`.
If the response is lost, issue a new code in the Hub; never retry indefinitely.

## Transport boundary

The transport uses an outbound WSS connection so the Hub can request
a current snapshot without opening inbound ports at the observatory. This is
not a generic RPC or camera-control channel. Local sharing consent governs
both event uploads and snapshot responses; no automatic pairing enables images.
Event detection and snapshot sharing must be opt-in in AutoPierCam.

### Connection and consent

Connect to `/v1/devices` over WSS. Within ten seconds send:

```json
{"type":"authenticate","protocol_version":1,"installation_id":"9c18405b-ddcc-4a31-9010-e19ccf1bb567","credential":"csdc_<secret>","snapshots":true}
```

The Hub responds with `{"type":"ready","protocol_version":1,"device_id":42}`.
Only one connection per device is permitted; a duplicate gets an
`already_connected` error. Answer WebSocket pings (every 30 seconds). Idle peers
are disconnected after 120 seconds. Reconnect with exponential backoff and
jitter, capped at 60 seconds; never automatically retry pairing. Revocation,
credential rotation, deletion, or route removal closes the current connection.
Use the camera card's **Refresh camera status** button to update its online state.

`snapshots` advertises the client's current, explicit local consent. A client
must recheck that consent when receiving every request, and disconnect on any
privacy change. The Hub cannot turn sharing on. Disabling event transmission
locally must drop queued images, not just stop creating new events.

### Events and receipts

```json
{
  "type": "event",
  "event": {
    "event_id": "a5651f7d-b8a7-43cf-8135-b420a8a2b112",
    "kind": "scene_change",
    "captured_at": "2026-09-25T17:00:00Z",
    "summary": "Persistent scene change detected",
    "jpeg_base64": "<base64 JPEG>",
    "request_id": null
  }
}
```

Kinds are `scene_change`, `day_night_transition`, and `snapshot`. These are
observations, not AI classifications of meteors, people, animals, or clouds.
The image must be at most 512 KiB, have a JPEG envelope, and be captured no more
than five minutes ago (30 seconds future clock tolerance). The Hub validates
the envelope and size, not decoded pixels. Summary text is at most 300 printable
characters. WebSocket messages are capped at 720 KiB. Arbitrary URLs, client
channel IDs, and unknown message fields are rejected. Discord mentions are
disabled. Only owner-selected channels receive an attachment and timestamp.

Ack: `{"type":"event_ack","event_id":"...","status":"delivered","retry_after_seconds":0}`.
`retry` or `rate_limited` asks the client to retry the **same immutable event and
UUID** after at least 60 seconds while it is still fresh. Other statuses
(`invalid_event`, `invalid_image`, `event_conflict`, `invalid_request`,
`no_destinations`) are terminal. An event is never backfilled into channels
added later. Removing/re-adding a route does not authorize replay of old images.

New events have a device-wide 60-second cooldown, including requested snapshots;
retries have a separate 12-attempts/minute ceiling. There are at most eight
destinations per device. Successful per-route receipts survive restarts and
allow partial failures to retry without re-sending successful channels. Only
hashes, event IDs, route IDs and receipt times enter SQLite; JPEGs are transient.
Receipts are pruned after seven days when accepting new events. Device deletion
removes all receipts. This is **at-least-once**, not exactly-once: a crash after
Discord accepts a post but before the receipt commits can duplicate that post.
Revocation cancels in-flight work but cannot recall a message Discord accepted.

### Snapshot now

The owner can choose **Snapshot now** on the device card. It posts to that
device's selected channels, not an arbitrary channel or private viewer.
`POST /api/devices/{id}/snapshot` requires the owner's session and CSRF token.
There must be a destination, an online camera, and advertised local consent.
The Hub sends:

```json
{"type":"snapshot_request","request_id":"5fe70313-0a01-43b2-97a8-bab512a45ba7","expires_at":1790355690,"max_jpeg_bytes":524288,"max_frame_age_seconds":120}
```

The client returns a `snapshot` event with that `request_id`, or
`{"type":"snapshot_unavailable","request_id":"..."}`. No unsolicited snapshot
is accepted. The request expires after 90 seconds; the HTTP call waits at most
95 seconds and reports success only after delivery. Only one snapshot can be
pending; busy/denied/offline/timeout conditions are reported to the owner.
Snapshot retries are limited to the originating live request and connection.

"Now" means the latest **completed** frame, at most 120 seconds old, labeled
with its real capture time. A camera may wait for its next long exposure but
must not interrupt capture, reset exposure, or return a frame from an old session.
This supports 30–60+ second night exposures without acquiring a second SDK handle.
Hardware control, a Discord snapshot slash command, and arbitrary image analysis
are intentionally outside this protocol. AutoPierCam's production client and
local consent/configuration UI are a separate implementation step.

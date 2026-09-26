# Observatory device protocol (pairing v1, transport v1/v2)

[AutoPierCam](https://github.com/theatrus/autopiercam) is a user-owned
`pier_camera`, not a N.I.N.A. telescope/profile. Cameras set up the same way as
telescopes, in the **Pier cameras** card on the Hub's **Observatory devices** tab:

1. Register the server under **Discord delivery**.
2. Add a pier camera.
3. Attach it to a server you manage (`POST /api/devices/{id}/attach`).
4. Pick its channels in that server's card under **Discord delivery**. A live
   bot membership and text-channel check is required.
5. Choose **Pair camera…** and paste the code into AutoPierCam.

Camera feeds can share a telescope's channel without changing slash-command
routing. Only the owner picks a camera's channels, and only in a server where
the owner attached it. A server manager can remove a camera's channel or detach
it from that server, but cannot pair, revoke or delete another user's camera.
Detaching removes that server's channels and closes the camera's connection.
Device ownership does not grant telescope or N.I.N.A. permissions. Share codes
do not authorize devices. The Hub audit log records camera creation, pairing
codes, access resets, attachments and deletion.

Pairing is not carried in the Direct WebSocket handshake as it is for N.I.N.A.
rigs; cameras keep the HTTP exchange below.

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
Handshake errors use `{"type":"error","code":"..."}` before closing. Stop
and show operator action for `authentication_failed`, `unsupported_version`, or
`invalid_message`; back off on `rate_limited` and `already_connected`.
Use the **Refresh camera status** button to update the camera's online state.

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
(`elided`, `invalid_event`, `invalid_image`, `event_conflict`,
`invalid_request`, `no_destinations`) are terminal. Clients must treat any
unknown status as terminal. An event is never backfilled into channels
added later. Removing/re-adding a route does not authorize replay of old images.

The Hub posts at most one image per camera per minute. A new event that arrives
within 60 seconds of the last accepted one is swallowed and acknowledged as
`elided`; the camera must drop it, not retry it. When that minute ends, the Hub
posts one text notice to the camera's channels saying how many images it
skipped. The count lives in memory, so a Hub restart can lose it. A requested
snapshot inside the window is refused to the owner instead and is not counted.
Retries of an already accepted event are not subject to the cooldown, but all
attempts share a separate 12-per-minute ceiling that answers `rate_limited`. There are at most eight
destinations per device. Camera delivery observes Discord's `Retry-After`, global
scope, and exhausted-bucket reset headers across all device connections; a retry
acknowledgment never bypasses that backoff. See [Discord rate limits](https://docs.discord.com/developers/topics/rate-limits).
Requests failing with 401/403/404 are also backed off. Each delivery attempt has
a 30-second overall deadline, and snapshot deadlines use a monotonic clock.
Successful per-route receipts survive restarts and
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
Hardware control and arbitrary image analysis remain outside this protocol.
AutoPierCam implements the production client and local consent/configuration UI.

## Trigger extension (transport v2)

Pairing remains v1. An updated camera authenticates with `protocol_version:2`
when periodic sends, chat configuration or telescope triggers are enabled; the
Hub echoes version 2 in `ready`. Existing v1 cameras keep their unchanged
handshake and receive no new command types. Old Hubs reject version 2, which
must be shown as an operator-action error rather than retried indefinitely.

After v2 `ready`, the client advertises explicit local permission:

```json
{"type":"trigger_capabilities","chat_configuration":true,"telescope_events":true}
```

Both capabilities default false on every connection. The Hub exposes a
`piercam` group under `/chatstronomy` (not telescope hardware commands):

- `/chatstronomy piercam snapshot` uses the existing snapshot consent.
- `/chatstronomy piercam triggers interval_minutes:10 scene_changes:true day_night:false telescope_events:true burst_count:3 spacing_seconds:60`
  sets the complete active rule set, not a partial patch.

Like telescope commands, `camera` is optional. Without it, the command uses the
invoker's camera routed to the current channel; if several are, it lists their
names and asks for `camera:<exact name>`.

The invoking Discord user must own the camera AND invoke in one of its exact
guild/channel routes. DMs, unrelated routes, and non-owner server managers are
denied. Replies are ephemeral; images go to all configured camera destinations.
Local/self-hosted bots without a Hub return an unsupported-operation explanation.

The Hub validates minutes 0–1440 (0 off), burst count 1–3 and spacing 60–600
seconds, checks the advertised chat gate, then sends:

```json
{"type":"configure_triggers","request_id":"5fe70313-0a01-43b2-97a8-bab512a45ba7","rules":{"interval_minutes":10,"scene_changes":true,"day_night":false,"telescope_events":true,"burst_count":3,"spacing_seconds":60}}
```

The camera must recheck local consent and rate/size ceilings, persist atomically,
clear pending images/bursts when accepted, and reply:

```json
{"type":"trigger_configuration_result","request_id":"5fe70313-0a01-43b2-97a8-bab512a45ba7","accepted":true}
```

Only a matching live pending request can complete a configuration command.
One configuration may be in flight per device; it expires after 15 seconds and
the caller waits at most 20. Lost/late replies are ambiguous: inspect the camera's
active settings before retrying. A successful reply means saved, not that an
image was posted. The Hub does not store or replay configuration on reconnect.

AutoPierCam's local switches and interval/burst/spacing settings are ceilings.
Chat cannot enable sharing or a locally disabled source, shorten the local
interval or spacing, increase the burst cap, grant snapshot access, change
destinations/credentials, or control hardware. Chat rules persist on the camera;
a local settings save resets overrides and revision checks prevent stale saves.
Pairing/forgetting resets every permission. Disabling/pause/session changes cancel
pending work; accepted Discord posts cannot be recalled.

The Hub forwards fresh, deduplicated, chat-enabled live telescope events only
to opted-in cameras with the same owner and an exact shared guild/channel route.
Startup history is not forwarded. Events older than 30 seconds or future-dated
are ignored. The fixed initial event allowlist is slew start/end and sequence
start/finish:

```json
{"type":"telescope_event","event":"mount_slew_started","expires_at":1790355630}
```

Other `event` values: `mount_slewed`, `sequence_started`, `sequence_finished`.
The expiration is Unix seconds, 30 seconds from admission. This is an observation,
never an instruction to slew or operate a camera. There is a bounded four-entry
control queue; full/busy queues drop event notifications. The camera checks expiry
and its local event gate, coalesces triggers while a burst is active, and waits for
new completed frames in the same capture session. It never interrupts exposures
or buffers pre-event images. Burst plans expire after 180 seconds plus inter-image
spacing; reconnects discard unfinished plans. Failed delivery may reduce a burst.

V2 adds event kinds `periodic` and `telescope_event`, with the same JPEG envelope,
immutable event IDs, routing and receipt rules as other automatic observations.
They have no `request_id`. The device-wide 60-second cooldown and Discord backoff
still apply; bursts do not bypass either. Periodic schedules send one distinct
recent frame, start after a full interval and reset on reconnect without backfill.
Scene detection is not a semantic roof/person detector; no such classifier is
provided by this extension.

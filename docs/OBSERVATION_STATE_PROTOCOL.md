# Pier-camera observation state — transport v4

This is the final Hub → camera overlay contract. It is a **state feed**, not an
image trigger, command, or an extension of the v3 `telescope_event.context` object.
All numbers representing temperature are degrees Celsius. All timestamps are
offset-aware RFC3339 **UTC** (`Z` or `+00:00`). Null always means unknown/cleared.

## Negotiation

Use the existing `/v1/devices` WebSocket and device credential/installation ID:

```json
{"type":"authenticate","protocol_version":4,"installation_id":"<uuid>","credential":"<device credential>","snapshots":false}
```

The Hub answers the unchanged ready envelope, with `protocol_version:4`. Overlay
consent is independent of snapshots, timelapses, telescope image triggers, and
chat configuration. **Do not subscribe unless the camera's local overlay option
is enabled.** Then send:

```json
{"type":"observation_capabilities","telemetry":true}
```

The Hub answers:

```json
{"type":"observation_ready","telemetry":true,"interval_seconds":5,"max_age_seconds":30}
```

Disable with the same capability message and `telemetry:false`; the Hub cancels
pending reads and acknowledges false. The camera must clear its own retained
overlay values immediately on disable, disconnect, pause, or capture-session change.
State does not require `trigger_capabilities.telescope_events:true`.

Transport v1/v2/v3 messages remain unchanged. **Never append these fields to a
v3 context or send these new message types to a v3 client.** Older Hubs reject
v4; report that overlay support requires an updated Hub. Existing v3 operation
remains usable when overlays are not requested. Pairing is still v1.

## Complete snapshot

One message per eligible telescope, initially after subscription and thereafter
every five seconds. At most 16 same-owner telescopes with an exact shared
guild/channel route are eligible per camera. Do not merge different telescope IDs.

```json
{
  "type": "observation_state",
  "telescope_id": 17,
  "telescope_name": "UC131",
  "state_epoch": "6a9a7146-60eb-47ea-abec-43ad43af71e1",
  "revision": 42,
  "observed_at": "2026-10-22T05:10:00Z",
  "valid_until": "2026-10-22T05:10:30Z",
  "available": true,
  "target": {
    "run_id": "2d3768d4-3bf2-4a6e-a568-776aa6a22a39",
    "name": "NGC 7023",
    "started_at": "2026-10-22T04:00:00Z"
  },
  "temperatures": {
    "focuser": {
      "celsius": 21.3,
      "observed_at": "2026-10-22T05:09:58Z",
      "valid_until": "2026-10-22T05:11:58Z"
    },
    "external": null
  },
  "completed_exposures": {
    "count": 505,
    "image_type": "LIGHT",
    "basis": "successfully_saved",
    "as_of": "2026-10-22T05:09:55Z"
  }
}
```

- `telescope_id`: positive Hub record ID, not a hardware identifier.
- `telescope_name`: bounded to 80 Unicode characters; nullable when unavailable.
- `state_epoch`: opaque plugin-owned UUID, stable across network reconnects and
  Hub restarts; changes on source resets, sequence lifecycle and relevant privacy
  boundaries. Nullable when the source is unavailable.
- `revision`: nonnegative integer ≤ 2^53−1 within an epoch; nullable when
  unavailable. An unchanged revision can accompany a fresh poll timestamp.
- `observed_at` / `valid_until`: freshness of the source snapshot, maximum 30
  seconds. Reception is **not** a new measurement time.
- `available:false`: clear all values immediately. Target/count/sensor readings
  are null; epoch/revision/name may also be null. Sent for unsupported/old
  plugins, disconnected or timed-out sources, and route removal. Silence must
  also expire state at `valid_until`.
- `target`: nullable; `run_id` identifies one target **visit**, not the display
  name. Name is bounded to 120 Unicode characters. `started_at:null` means the
  visit's beginning was not observed, so its count is unknown.
- `temperatures.focuser` / `.external`: independently nullable. External is the
  weather/observatory sensor, never the imaging-camera cooler. Each has its real
  measurement timestamp and expiry, at most two minutes after measurement.
  A measurement may predate the current target. Choose the desired sensor locally
  and label its source; never fabricate a fallback reading or renew a timestamp.
- `completed_exposures`: nullable. Count means confirmed, successfully **saved
  LIGHT imaging frames during this visit**. It is not a planned count, raw
  exposure-start count, N.I.N.A. history length, or pier-camera frame count.
  `as_of` is the latest included completion (or the confirmed visit start for 0).
  AF, aborted/failed saves, DARK/FLAT/BIAS and duplicate saves do not increment it.

Every message replaces the preceding state for that telescope, including nulls.
On an epoch change discard the previous revision/target associations. Repeated
snapshots, initial baselines, reconnects and count changes **must not trigger
snapshot requests, image posts, or action bursts**.

For frame overlays, retain a bounded timestamped history and use only state
known at/before that frame's capture time and within its validity window. Do not
apply a later target/count to an earlier frame. Do not interpolate counts or
fill gaps after a disconnect. A counter's new value becomes effective at
`as_of`; a target's confirmed beginning is `started_at`. Unknown remains unknown.

## Source and reset semantics

The N.I.N.A. plugin advertises additive Direct capability `observation_state:true`.
Only then may the Hub send the read-only `observation_state` query. It returns
the source snapshot directly as `query_result.payload` (the fields above minus
`type`, `telescope_id`, `telescope_name`, `valid_until`, which the Hub supplies).
Older plugin capabilities omit the flag and are never sent the query.

The plugin has a per-profile **Share observation state for pier-camera overlays**
switch. Target sharing additionally requires Target Scheduler category consent;
counts require Images and Sequence; focuser readings require filter/focuser/rotator
consent, and external readings require Weather changes consent. Disabled fields
are null before the payload leaves N.I.N.A. No coordinates, project IDs, device
IDs, paths, raw events, or camera temperature are part of this projection.

Native target-container status transitions establish visits. Target Scheduler
`NewTargetStart` establishes a new visit; ordinary `TargetStart` observations of
the same visit do not reset it when per-exposure plan objects change. Returning
to the same target name after a new visit or sequence run gets a new `run_id`.
Pause/resume and transport-only reconnects do not reset a visit.

Saved-frame counters are source-owned, independent of the 500-entry history ring.
They require a confirmed start and matching target metadata plus a zoned exposure
start. Missing provenance makes the total unknown rather than publishing a
partial count. Attaching mid-visit, privacy gaps, unsupported/dynamic containers,
or ambiguous parallel targets may therefore legitimately produce null counts.
There is no reconstruction from retained history. Local readings can expire
between saved frames; fresh permitted sensor callbacks also update temperatures.

## Timelapses and compatibility

V4 retains the v3 media capability and raw MP4 upload envelopes; overlay messages
do not change an advertised media limit or permit uploads. The camera must still
negotiate media sharing separately. Video overlays, transcoding, byte-size-capped
sharing copies and rendering remain AutoPierCam's responsibility. No live camera
or Discord traffic is needed to test this contract.

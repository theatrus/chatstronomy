# N.I.N.A. plugin architecture

The Chatstronomy N.I.N.A. plugin is the only observatory data source. It reads
public N.I.N.A. mediators and bounded native histories in-process, then answers
typed Direct queries from either a local bundled runtime or the hosted Hub.

## Data flow

```text
N.I.N.A. mediators / sequence / notifications / log
                         |
                  Chatstronomy plugin
                    /            \
       local named pipe        TLS WebSocket
              |                     |
     bundled local runtime      Chatstronomy Hub
              |                     |
       Discord / Matrix        Discord / commands
```

Local Direct mode uses a random current-user-only bootstrap pipe for secrets and
a node-scoped data pipe for typed queries. The child runtime starts and stops
with the plugin by default. No TCP listener, pairing token, or observatory URL is
required.

Hosted mode connects from the plugin to `/v1/direct` on the Hub. A one-time
pairing token becomes a profile-and-node-bound credential stored with Windows
Credential Manager. Multiple profiles and systems can connect concurrently.

## Source contract

`RigSource` is the transport-neutral boundary used by status polling, chart
rendering, Discord slash commands, and Matrix/Discord delivery. It includes:

- bounded event, image, and sequence snapshots;
- thumbnails, the matching completed autofocus report, guider data, and
  rendered graph inputs;
- mount, camera, filter wheel, guider, rotator, and focuser snapshots;
- durable safety-monitor state and active safety-wait state;
- mount-slew completion; dome/shutter and flat-panel lifecycle; and connection
  state for weather and switch devices;
- opt-in, unit-explicit observing-condition changes and high-wind
  alert/recovery state;
- image-save failures and sequence-item failures with explicit sequence
  completion outcomes;
- typed commands such as park, guide, cool, autofocus, and sequence control.

Direct v1 envelopes currently advertise additive payload contract v3. Version
2 added sequence-operation reporting and Hub image attachments; version 3 adds
`ChatEnabled`, explicit target names, N.I.N.A. logs, and N.I.N.A. popup
notifications. Older plugin payloads without `ChatEnabled` remain accepted and
default to delivery enabled. The server labels unmarked payloads as legacy
Direct v1; this is Direct protocol compatibility, not a second data-source
mode.

For compatibility with existing Direct v1 runtimes, position-redacted mount
snapshots keep their required location-related fields but replace sensitive
numbers and strings with zero or empty sentinels. The additive
`LocationRedacted: true` marker lets newer consumers omit those rows entirely.
The existing `StatusCode: 202` response marks asynchronous commands as accepted
rather than completed. Hardware-command failures become
`CHATSTRONOMY-COMMAND-FAILED` events. Once N.I.N.A. accepts a locally permitted
command, its terminal failure is always delivered as part of that command
exchange rather than optional event chatter.

## Event delivery and state

Event-delivery settings are enforced before events leave N.I.N.A. Events from
disabled categories are not sent to the local runtime or the hosted Hub; this
includes previously captured events whose category has since been disabled.
There is no state-reconstruction exception. The sole delivery exception is a
terminal failure for a locally permitted command that N.I.N.A. already
accepted; it remains part of that command exchange. Log events are sent only
when their individual log level is enabled.

Disabling image delivery also returns an empty image history and blocks
thumbnail retrieval, including explicit last-image requests. Images captured
while delivery is disabled remain unavailable if delivery is later re-enabled.

The Rust updater can reconstruct available state from permitted events and
independent, allowed sequence or equipment snapshots requested on demand.
Disabling an event family can reduce historical or intermediate target, wait,
cooling, guider, or sequence details; the privacy boundary takes precedence over
that additional state. Older peers that supply `ChatEnabled` continue to be
accepted for Direct payload compatibility.

The plugin observes native safety-monitor connection and safe/unsafe changes.
The updater retains the resulting unknown, disconnected, safe, or unsafe state
for status output while safety delivery remains enabled. Safety events have
their own delivery switch. A **Wait Until Safe** operation is visible only when
both sequence and safety delivery are enabled, and its state comes from the
safety-monitor mediator rather than a potentially stale sequence item property.

Sequence snapshots identify N.I.N.A.'s built-in timed, altitude, Moon-altitude,
Sun-altitude, horizon, and safety waits, camera cooling and warming, mount slews,
centering, and standalone plate solves. They also identify selected long-running
Sequencer+ waits: **Wait Until Safe**, condition waits, and manual waits. The
wire projection includes only operational state such as status and polling
interval; condition expressions and free-form pause reasons stay inside
N.I.N.A. Other Sequencer+ items continue to use the existing generic sequence
representation. Root sequence-item failures are emitted separately, and final
sequence events carry an explicit outcome; an ambiguous end is rendered
neutrally rather than being presented as a successful completion.

Dome/shutter actions and flat-panel cover, light, and brightness changes are
governed by the plugin's dedicated **Observatory and flat panel** switch.
Connect/disconnect events for dome, flat-panel, weather, and switch devices are
governed by **Equipment connections**. Weather measurements have two independent
controls that both start off: **Weather changes** and **High-wind alerts**.
`WEATHER-CHANGED` carries a complete snapshot of the available unit-explicit
numeric readings plus the display labels for fields that changed. The plugin
learns the first snapshot silently, publishes only meaningful deltas, and
rate-limits routine changes to one every five minutes while allowing rain-start
events immediately. `WEATHER-HIGH-WIND` compares the greater available
wind speed or gust with the profile's threshold and publishes explicit
high/recovered state; recovery uses hysteresis of at least 1 m/s or 10 percent.
Its payload is restricted to wind speed, gust, threshold, and alert state.
Threshold changes and station reconnects can refresh an active Hub latch, while
the updater suppresses a duplicate chat alert when the state remains high;
missing sensor readings cannot prove recovery. The two event families have
separate privacy-revocation scopes. Neither carries a device identity, raw
driver object, or site location. Weather output is informational, can be
delayed, missing, or inaccurate, and does not replace N.I.N.A.'s safety monitor
or physical interlocks. The contract still does not expose switch
values or LiveStack data as structured telemetry. Enabled popup notifications
and opt-in raw N.I.N.A. logs remain unstructured text and may contain
operational details.

For mixed payload-v3 deployments, changing mount, sequence, or safety delivery
invalidates the existing Direct session before the plugin publishes the new
policy. Reconnection creates a fresh updater baseline, so an older peer cannot
interpret a newly opaque item as the completion of details cached under the
previous consent state. Current peers also understand the explicit suppression
tombstone and discard that tracked path silently.

Autofocus completion is matched to the corresponding finished report before it
is used for graph input. Both the plugin and updater retry bounded transient
report-availability failures so a preceding run is not rendered as the new
autofocus result.

Target Scheduler integration follows its N.I.N.A. message-broker topics and
projects the active container's `Target.TargetName`, avoiding the generic
“Sequential Instruction Set” wrapper name.

Popup notifications are observed from N.I.N.A.'s toast lifetime supervisor.
Raw N.I.N.A. logs are tailed from the active process log and can be enabled by
level. Log delivery is opt-in due to volume and possible private path/device
content.

## Security boundaries

- Local secrets are sent only over a current-user named pipe and are not placed
  in arguments, environment variables, or generated configuration files.
- Hosted connections are outbound TLS WebSockets.
- Matrix homeserver URLs must use HTTPS.
- Hardware control is disabled by default for every profile and delivery mode.
  A local master switch and individual per-command permissions must both be
  enabled in the N.I.N.A. plugin; sequence-validation bypass has its own
  separate opt-in. A connection advertises command support only when at least
  one operation has local approval.
- Hub commands are typed, expire, and are authorized against telescope routing
  and guild policy before reaching the plugin's independent local permission
  check. Hardware commands have only five seconds of clock-skew tolerance
  beyond their deadline; legacy read queries keep their existing two-minute
  tolerance. Server policy can narrow local consent but cannot grant it.
- In local Discord-bot mode, an empty write allowlist permits only managers of
  the invoking guild. A nonempty allowlist permits only those explicit users;
  commands must come from their telescope's configured Discord channel. Direct
  messages, other servers, and commands disabled in N.I.N.A. are always denied.
- Observatory location and location-derived values are private unless the
  N.I.N.A. owner explicitly enables sharing. Stable device identifiers are
  never forwarded.
- Direct histories are bounded to prevent unbounded plugin memory growth.

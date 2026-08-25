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
- thumbnails, autofocus data, guider data and rendered graph inputs;
- mount, camera, filter wheel, guider, rotator, and focuser snapshots;
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
`CHATSTRONOMY-COMMAND-FAILED` events and are transmitted only when their event
category is enabled in the N.I.N.A. profile.

## Event delivery and state

Event-delivery settings are enforced before events leave N.I.N.A. Events from
disabled categories are not sent to the local runtime or the hosted Hub; this
includes previously captured events whose category has since been disabled.
There is no state-reconstruction exception and command-failure events follow the
same category settings as every other event. Log events are sent only when their
individual log level is enabled.

Disabling image delivery also returns an empty image history and blocks
thumbnail retrieval, including explicit last-image requests. Images captured
while delivery is disabled remain unavailable if delivery is later re-enabled.

The Rust updater can reconstruct available state from permitted events and
independent, allowed sequence or equipment snapshots requested on demand.
Disabling an event family can reduce historical or intermediate target, wait,
cooling, guider, or sequence details; the privacy boundary takes precedence over
that additional state. Older peers that supply `ChatEnabled` continue to be
accepted for Direct payload compatibility.

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

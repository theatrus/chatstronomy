# Direct protocol v1

Direct v1 carries N.I.N.A. identity, pairing/authentication, source-neutral
queries, typed commands, results, and heartbeats. JSON frames use the tagged
envelope described by `schema.json`.

The normative implementation is `src/direct/protocol.rs`. The schema and
fixtures are durable cross-repository compatibility inputs. Additive optional
fields may be introduced within v1; incompatible wire changes require a new
protocol directory and protocol version.

The additive parameterless commands `slew_to_target`, `center_target`, and
`center_rotate_target` require a supporting N.I.N.A. plugin. Their target and
position angle are resolved locally; these messages contain no coordinates.
Support is advertised by the optional hello/bootstrap capability
`target_commands: true`, shown in `fixtures/client-hello-target-commands.json`.
Missing or false means unsupported: Hub and local runtime must reject these
three command kinds before sending them to older plugins. The separate
`commands` capability and per-command local permissions still govern authority.
`fixtures/query-slew-target.json`, `query-center-target.json`, and
`query-center-rotate-target.json` cover their wire shape. Existing command
values, including `start_autofocus`, `cancel_autofocus`, `change_filter`,
`start_sequence`, and `stop_sequence`, remain unchanged.

The plugin chooses idle execution or a matching advanced-sequence trigger
before a light exposure for autofocus, filter changes, and target operations.
All queued or asynchronously accepted commands retain the existing command
response envelope with `StatusCode: 202` and `Success: true`. This acknowledges
acceptance, not hardware completion. A rejected request remains a failed
command response or rejected `query_result` with its reason; clients must not
retry it as a transient query. Sequence and capture ownership, local consent,
and request lifetime are checked in N.I.N.A., independently of event sharing.

Failed `query_result` frames may optionally carry
`error_code: "resource_not_ready"` when an asynchronous resource exists but is
still being produced. A peer that understands the code can retry it separately
from policy or validation failures. The field is additive: older frames omit
it, and unknown future codes retain the legacy terminal-rejection behavior.

`payload_version` marks the additive data contract independently of the Direct
envelope. Current clients advertise payload version 3. Version 2 added
sequence-operation reporting and Hub image attachments; version 3 adds event
delivery flags, explicit target names, N.I.N.A. logs, and N.I.N.A. popup
notifications. A Direct v1 hello that omits the field is an explicitly
supported legacy payload-version-1 client; the Hub echoes version 1 in its
agent hello and keeps accepting its original frames.
`fixtures/client-hello-legacy.json` is the frozen unmarked legacy form.

Payload version 3 permits additive autofocus-completion and safety-monitor
event details plus the `safety_wait`, `condition_wait`, and `manual_wait`
sequence operation kinds. Peers that do not send these optional details retain
their existing behavior.

Payload version 3 also permits additive motion diagnostics. A plugin may emit
`MOUNT-SLEW-STARTED` before the existing `MOUNT-SLEWED` event and
`ROTATOR-MOVE-STARTED` before the existing `ROTATOR-MOVED` or
`ROTATOR-MOVED-MECHANICAL` event. Motion IDs, requested targets, observed
logical/mechanical positions, elapsed observation time, and end-detection provenance are
optional so older completion payloads remain valid. State-observed mount starts
and ends can carry altitude and azimuth only when the N.I.N.A. profile permits
location sharing. A callback-only recovery sets `ObservedInProgress`, carries no
inferred duration, and lets the receiver label the reconstructed pair
accordingly. Because N.I.N.A.'s completion callback has no start timestamp, its
recovered `From` position contains RA/Dec but no historical altitude or azimuth.

`fixtures/query-result-motion.json` covers state-observed and callback-recovered
motion, including a location-redacted mount pair. The frozen
`fixtures/query-result-motion-legacy.json` completion payloads omit motion IDs,
start events, provenance, and delivery flags. Both use the existing unrestricted
query-result payload in the v1 envelope schema.

An event-history response may include `ElidedEvents`, an array of cumulative
plugin rate-limit counters. Each entry contains `Event`, optional `Level`,
`Count` (an unsigned 64-bit integer), and `Epoch` (an opaque identifier for the
current profile/permission epoch). Counters contain no message text or event
details. A counter counts events actually dropped by the plugin rate limiter;
locally disabled categories and log levels must not produce counters.

Clients that support counters send every supported, locally permitted slot,
including slots with `Count: 0`. Omitted slots revoke permission to publish any
pending notice for that category/level; zero counters allow a receiver to keep
its own pending notice when it, rather than the plugin, dropped events. An
explicit empty array authoritatively clears all prior counter state. Missing
metadata means an older client and is not a permission update. A profile or
permission change rotates the epoch and zeros all counters; counts from an
earlier epoch must never replay. Consumers baseline the first snapshot without
a notice and report only later increases, so repeated polls do not repeat a
notice. `fixtures/query-result-elided-events.json` is an additive payload-v3
example; older event-history fixtures remain unchanged.

The receiver considers at most 128 entries. Event names and epochs are limited
to 64 ASCII letters, digits, hyphens, or underscores; levels use the same
characters with a 16-character limit. All identifiers must be nonempty.
A malformed container, any malformed entry, duplicate canonical event/log-level
keys, or more than 128 entries makes the entire metadata snapshot absent.
Partial snapshots must not revoke valid
permissions or reset cumulative watermarks. Valid events in the same history
response remain available even when its metadata is ignored.

`last_autofocus` keeps N.I.N.A.'s common autofocus report as its required
surface. Hocus Focus can add optional final-measurement provenance, fit-quality
statistics, accepted-star counts, normalized region geometry, selected fit
model, and the allowlisted `HocusFocusAlgorithm` object. These fields are
additive within payload v3: pure N.I.N.A. reports omit them and use the same
query, chat, and graph paths. Raw Hocus settings objects are never part of this
contract.

When local event consent is revoked for an operation that previously appeared
in the sequence tree, its stable tree slot can contain an opaque privacy
tombstone: `Suppressed: true`, `ChatEnabled: false`, a generic name/status, and
no operation fields. Consumers must silently discard any operation previously
tracked at that path. A tombstone is not an operation completion and must not
produce a chat message.

The N.I.N.A. plugin invalidates its current Direct session before publishing a
mount, sequence, or safety delivery-setting change, then reconnects with a
fresh updater baseline. This ordering keeps payload-v3 peers that predate the
tombstone field safe: an older peer can ignore a tombstone in its initial
snapshot, but can never compare one with operation details cached under the
previous consent state.

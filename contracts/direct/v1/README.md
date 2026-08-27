# Direct protocol v1

Direct v1 carries N.I.N.A. identity, pairing/authentication, source-neutral
queries, typed commands, results, and heartbeats. JSON frames use the tagged
envelope described by `schema.json`.

The normative implementation is `src/direct/protocol.rs`. The schema and
fixtures are durable cross-repository compatibility inputs. Additive optional
fields may be introduced within v1; incompatible wire changes require a new
protocol directory and protocol version.

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

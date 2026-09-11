# Hosted Chatstronomy Hub

The Hub is the primary centralized mode. It owns the Discord application, web
login, routing policy, SQLite state, and live connections from N.I.N.A. plugins.
The public service is [hub.chatstronomy.com](https://hub.chatstronomy.com).

## Components

- Axum web application and `/v1/direct` WebSocket endpoint
- Discord OAuth login and guild management
- Discord bot gateway and slash commands
- SQLite users, guilds, telescopes, routing, sessions, and credentials
- one updater per connected and routed telescope

The Hub has no observatory polling configuration. A telescope is online only
while its N.I.N.A. plugin has an authenticated Direct WebSocket connection.

## Pairing

1. Sign in to the Hub with Discord.
2. Register a guild and create a telescope.
3. Attach feed channels and choose a command policy. Hardware control remains
   disabled until the telescope owner separately authorizes it in N.I.N.A.
4. Mint a single-use `cspt_…` pairing token.
5. In the N.I.N.A. plugin, review **Security and privacy** and **Event delivery**
   before pairing. Most event categories, including images, start enabled;
   weather changes, high-wind alerts, hardware control, observatory location
   sharing, and log forwarding start off.
6. Choose Hosted Hub, confirm
   `https://hub.chatstronomy.com`, enter the token, and connect.
7. The plugin stores its pairing token and returned credential in Windows
   Credential Manager, not in the N.I.N.A. profile or a configuration file.

Review the hosted [privacy statement](https://chatstronomy.com/hub-privacy.html)
and [terms of service](https://chatstronomy.com/hub-terms.html) before signing
in or pairing. Both are also linked from the Hub and the N.I.N.A. plugin.

Credentials are bound to the telescope plus the plugin's node/profile identity.
They can be revoked from the Hub. Pairing tokens are hashed, expire, and are
consumed once.

## Local hardware-control consent

Hardware control is disabled by default in each N.I.N.A. profile. The telescope
owner must enable the plugin's local master switch and separately approve every
kind of hardware command they want to expose. Starting a sequence without
validation additionally requires its own explicit local permission. The Hub
cannot enable or expand these permissions: its server-manager and role policies
only restrict operations already permitted by the N.I.N.A. profile.

The Hub labels online rigs as locally locked until at least one operation is
approved. If a caller requests a different operation, the plugin rejects it
before touching any N.I.N.A. mediator. Asynchronous commands report that they
were accepted; if an accepted operation later fails, that terminal failure is
always posted as part of the command exchange and is not controlled by optional
event switches.

## Autofocus and sequence commands

The hosted and local Discord bots share the `/chatstronomy` command group:

Use plugin **0.1.0.28 or newer** for the local sequence guards and trigger
queues described here. Existing command kinds retain the connected plugin's
behavior; updating the Hub alone cannot add local safety checks to an older
plugin.

| Command | Behavior enforced in N.I.N.A. |
| --- | --- |
| `autofocus` | Starts while idle only when camera ownership can be acquired; during an advanced sequence, queues a request for the Chatstronomy Autofocus trigger. |
| `autofocus cancel:true` | Cancels only Chatstronomy's own queued or running autofocus request. |
| `change-filter filter:<name>` | Changes the named filter while idle, or queues it before a light exposure through the matching trigger. |
| `slew-target` | Slews to the target resolved inside N.I.N.A., while idle or through the matching sequence trigger. |
| `center-target` | Plate-solves and centers the locally resolved target, while idle or through the matching trigger. |
| `center-rotate-target` | Centers and rotates to the locally resolved target and position angle, while idle or through the matching trigger. |
| `start-sequence` | Starts the loaded sequence while idle; pre-run validation is enabled unless separately permitted and explicitly skipped. |
| `stop-sequence` | Requests a coordinated stop of the currently active sequence. |
| `cool` / `warm` | Changes camera temperature while idle or during a sequence, when locally permitted. |

For requests during an advanced sequence, add the corresponding trigger to the
target's enclosing instruction set or an ancestor containing its exposures:

| Command | N.I.N.A. trigger |
| --- | --- |
| `autofocus` | Chatstronomy Autofocus |
| `change-filter` | Chatstronomy Filter Change |
| `slew-target` | Chatstronomy Slew to Target |
| `center-target` | Chatstronomy Center Target |
| `center-rotate-target` | Chatstronomy Center and Rotate Target |

A queued request runs before the next eligible **light** exposure. If the
running sequence has no matching trigger, the plugin rejects the request; the
simple sequencer does not support this queue. Only one Chatstronomy autofocus
request can be pending or running at once. Requests expire and do not carry
into another sequence, profile, or permission session. Canceling a Chatstronomy
autofocus request never cancels a run started by N.I.N.A. or another plugin.

Target motion requires an unambiguous target selected locally in N.I.N.A.
The commands have no coordinate or rotation-angle arguments; center-and-rotate
also uses the target's local position angle. Each operation requires its own
local permission. When idle, operations using the camera must acquire capture
ownership. Missing targets or equipment are reported as rejections.

Unpark, home, park, guiding changes, exposure abortion, and another sequence
start are rejected while a sequence is active. Use `stop-sequence` instead of
aborting an exposure owned by the sequencer. Cooling and warming remain
available during a sequence. These decisions use live state inside N.I.N.A.,
even when sequence event sharing is disabled or the Hub's displayed state is
stale.

Confirmation sends a request to N.I.N.A.; it does not prove that the operation
has started. Queued and asynchronously accepted requests carry status 202 and
are shown with a pending indicator. The plugin's rejection reason is preserved
in the response. Later failures of accepted commands remain visible through
the command exchange independently of optional event delivery.
The bot rechecks server policy and attachment access when dispatching and uses
the confirming interaction's current roles and permissions. A confirmation
cannot carry across a replaced telescope connection or a different telescope
that reused its name. Older plugins that do not advertise current-target
command support receive no unknown command; chat asks the owner to update.
Matrix and Discord webhooks support outbound notifications; interactive
hardware commands are provided through the Discord bot.

## Locally enforced event transmission and privacy

Event switches in the N.I.N.A. plugin are transmission and privacy controls, not
just notification preferences. Disabled event categories never leave N.I.N.A.
for the hosted Hub or a local runtime, even when those events were buffered
before the category was disabled. There is no exception for reconstructing Hub
state. A terminal failure for a locally permitted command that N.I.N.A. already
accepted remains part of that command exchange and is delivered independently
of optional event categories.

Turning off image delivery also blocks existing image history and historical
thumbnail requests. Images captured while delivery is disabled cannot be
retrieved later by turning the category back on. Independently permitted
equipment, sequence, and status snapshots remain available, but target,
cooling, wait, guiding, or other historical and intermediate state may be
incomplete without its events.

Most event categories, including images and popup notifications, are enabled by
default. N.I.N.A. log forwarding is separate, disabled by default, and opt-in per
log level; no N.I.N.A. log is read until at least one level is enabled.

Observatory latitude, longitude, elevation, location-derived sky coordinates,
and stable device identifiers are not forwarded by default. The owner can
explicitly opt in to location sharing in the same N.I.N.A. profile; device
identifiers remain private. Enabled images, notifications, target names, and
selected log lines can still reveal identifying information and should be
reviewed before sharing. Enabled sequence sharing can also include user-authored
annotation and message text. Failure summaries can contain sanitized N.I.N.A.
operational error text; local path-shaped strings are redacted before
transmission.

## Self-hosting

```bash
chatstronomy hub --hub-config hub.json --init
chatstronomy hub --hub-config hub.json
```

The generated configuration covers:

- public base URL and bind address;
- SQLite database path;
- session signing key;
- Discord client ID, secret, public key, bot token, and API base URL.

Use HTTPS/WSS at the public edge. Health is exposed at `/healthz`.

## Runtime behavior

Diagnostic flood protection runs per telescope before chat delivery, including
for older plugins. The same event and details can be posted once per minute.
Errors and warnings share a budget of five initial messages, replenished at one
every 12 seconds; ordinary logs and popup notifications have a separate budget
of ten, replenished at one every six seconds. Excess records are dropped and
marked consumed, so repeated history polls or a slow Discord connection cannot
turn them into a delivery backlog. Sequence failure state still updates.
Safety transitions, equipment recovery, sequence lifecycle, autofocus results,
and accepted remote-command outcomes are not subject to this diagnostic limit.
The budgets survive ordinary plugin transport reconnects.

One aggregate **Messages elided (+N skipped)** notice reports dropped
diagnostics, no more than once per minute. Empty history polls flush any final
pending count after a flood stops. With supporting plugins, this includes
records dropped inside N.I.N.A. before transmission. Their optional cumulative
`ElidedEvents` metadata carries only counts and permission-epoch identifiers;
the Hub tracks deltas so retries and reconnects do not repeat them. Startup
adopts existing totals silently. Revoking event sharing clears pending counts
for that category, and no discarded error or log text is included in notices.

The plugin answers independently permitted event, image, sequence, chart,
equipment, and typed-command queries. Hub updaters reconstruct available target,
sequence, built-in timed and astronomical waits, supported Sequencer+ waits,
durable safety and active safety-wait state, camera cooling and warming, guider,
mount, and image state from enabled event families and permitted status
snapshots. They also report mount-slew completion, sequence-item and image-save
failures, explicit sequence outcomes, center and plate-solve results,
dome/shutter and flat-panel lifecycle, and weather/switch connection state.
The dedicated **Observatory and flat panel** switch controls dome/shutter
actions and flat cover, light, and brightness changes; **Equipment connections**
controls the corresponding connection events and weather/switch connectivity.
Two independent controls, **Weather changes** and **High-wind alerts**, expose
structured observing conditions and both start off. Weather-change events carry
only available, unit-explicit readings: ambient and sky temperature, dew point,
humidity, pressure, cloud cover, rain rate, wind speed, gust, direction,
sky brightness, sky quality, and star FWHM. Routine changes require a meaningful
sensor delta and are limited to one post per five minutes; rain starting bypasses
that interval. High-wind alerts use the greater available wind speed or gust
speed, a locally configured threshold, and recovery hysteresis of at least
1 m/s or 10 percent. Alert and recovery edges are both reported. The payload
contains only wind speed, gust, threshold, and alert state. An active alert may
be resent after a reconnect or threshold change so durable Hub status remains
accurate without producing a duplicate chat alert. Missing sensor values do not
prove recovery. Neither weather event contains a weather-device identity, raw
driver object, or observatory location. Weather posts may be delayed, missing,
or inaccurate and do not replace N.I.N.A.'s safety monitor or physical
interlocks.
Switch values and LiveStack data are not captured. Enabled popup notifications
and opt-in raw N.I.N.A. logs remain unstructured text and may contain
operational details.
Autofocus graphs use the report matched to the completed run, and graph delivery
tolerates bounded transient query failures. Native N.I.N.A. focus and fitting
modes share one presentation; available Hocus Focus fit-quality, star-count,
region, validation, and algorithm fields enrich it without being required.
Only allowlisted result and algorithm fields cross Direct; raw Hocus settings,
paths, device IDs, images, star lists, optimizer feedback, and complete
Aberration Inspector analysis remain local.
Guider graph failures remain non-fatal and the image notification is sent
without the graph.
Disabled events or images can make state incomplete. Only approved
notifications are routed to attached Discord channels. Disconnects remove the
live source; reconnecting with the stored credential replaces the stale session
for the same rig identity.

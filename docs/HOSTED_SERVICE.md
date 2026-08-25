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
3. Attach feed/control channels and choose the write policy.
4. Mint a single-use `cspt_…` pairing token.
5. In the N.I.N.A. plugin choose Hosted Hub, confirm
   `https://hub.chatstronomy.com`, enter the token, and connect.
6. The plugin stores the returned credential in Windows Credential Manager.

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
were accepted; later failures are posted to the configured chat channels.

Observatory latitude, longitude, elevation, location-derived sky coordinates,
and stable device identifiers are not forwarded by default. The owner can
explicitly opt in to location sharing in the same N.I.N.A. profile; device
identifiers remain private.

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

The plugin answers event, image, sequence, chart, equipment, and typed command
queries. Hub updaters maintain target, sequence, wait, cooling, guider, mount,
and image state and route configured notifications to attached Discord channels.
Disconnects remove the live source; reconnecting with the stored credential
replaces the stale session for the same rig identity.

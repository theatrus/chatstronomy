# Chatstronomy

Chatstronomy bridges N.I.N.A. with Discord and Matrix, including bot slash
commands for observatory control. The N.I.N.A. plugin reads the running profile
directly and either starts a private local chat runtime or connects outbound to
the hosted Hub.

## Choose a mode

| Mode | N.I.N.A. data path | Chat credentials |
|---|---|---|
| Hosted Hub | Plugin → encrypted WebSocket → [hub.chatstronomy.com](https://hub.chatstronomy.com) | Managed by the Hub |
| Local webhook | Plugin → current-user named pipe → bundled runtime | Discord webhook in the N.I.N.A. profile |
| Local bot | Plugin → current-user named pipe → bundled runtime | Discord application token/channel in the N.I.N.A. profile |
| Local Matrix | Plugin → current-user named pipe → bundled runtime | HTTPS homeserver login and room in the N.I.N.A. profile |

Every N.I.N.A. instance runs the plugin. Multiple instances, including ones on
different systems, can connect to one Hub account and be routed independently.
Local mode is intentionally machine-local; use Hub mode when several systems
must share a centralized Discord application.

## Install the N.I.N.A. plugin

Install **Chatstronomy** from N.I.N.A.'s plugin manager. Use the official plugin
repository when the release is available, or add the
[Chatstronomy development repository](https://github.com/theatrus/chatstronomy-nina-plugin)
to N.I.N.A.'s repository list for development builds. Restart N.I.N.A. after
installation, open **Options → Plugins → Chatstronomy**, and choose Hosted Hub or
a local delivery method.

The plugin captures native equipment, image, autofocus, guider, sequence,
cooling, wait, slew, center, and Target Scheduler state. Per-profile controls
choose which event families produce chat messages. N.I.N.A. popup notifications
are enabled by default; raw log levels are separately opt-in because logs can be
frequent and contain local equipment or path details.

Telescope control is **disabled by default in N.I.N.A.** for every delivery
mode. To allow hardware commands, the telescope owner must enable the plugin's
local master switch and separately approve each individual operation; sequence
validation bypass requires an additional explicit permission. Discord server
permissions can narrow that access but can never enable an operation that the
N.I.N.A. profile has not approved. Asynchronous operations are reported as
accepted, not completed, and later hardware failures are delivered to chat.
In local Discord-bot mode, only managers of the invoking Discord server can
request approved operations unless an explicit user allowlist is configured;
requests must come from the telescope's configured channel, and direct messages
never gain control authority.

The plugin does not share the observatory's geographic location, derived local
sky coordinates, or stable equipment identifiers by default. An owner can
explicitly opt in to sharing the observatory location for that N.I.N.A. profile;
equipment identifiers remain private. Review the hosted service's
[privacy statement](https://chatstronomy.com/hub-privacy.html) and
[terms of service](https://chatstronomy.com/hub-terms.html) before pairing.

## Run the Hub

The production path is [hub.chatstronomy.com](https://hub.chatstronomy.com).
Self-hosters can run the same service:

```bash
chatstronomy hub --hub-config hub.json --init
chatstronomy hub --hub-config hub.json
```

Edit `hub.json` after initialization to configure the public URL, Discord OAuth
application, bot token, signing key, bind address, and SQLite database. See
[docs/HOSTED_SERVICE.md](docs/HOSTED_SERVICE.md).

## Build and test

```bash
cargo build --release
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

On Windows the release artifact also contains the plugin-owned local runtime.
The plugin repository downloads that signed artifact, verifies its checksum and
signature metadata, and packages it with the N.I.N.A. plugin.

Release archives include the Apache-2.0 application license and the SIL Open
Font License notice for the embedded Liberation Sans chart font. Standalone
executables also expose both notices with `chatstronomy licenses`.

## Architecture

- `src/direct/` — versioned named-pipe and WebSocket protocol
- `src/hub/` — Hub server, authentication, routing, storage, and connected rigs
- `src/chat/` — Discord and Matrix delivery plus slash-command routing
- `src/chat_updater.rs` — state reconciliation and chat notifications
- `src/plugin_runtime.rs` — secure local runtime bootstrap from the plugin
- `contracts/direct/` — published Direct protocol fixtures

The Direct transport is outbound-only from N.I.N.A. and exposes semantic read
queries and typed commands. It does not open an observatory HTTP listener.

## License

Apache-2.0. Author: Yann Ramin.

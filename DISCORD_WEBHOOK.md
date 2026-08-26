# Discord webhook delivery

Chatstronomy can post N.I.N.A. events, images, autofocus results, and guider
graphs through a Discord webhook in local mode. N.I.N.A. data comes from the
Chatstronomy plugin over its private Direct named pipe.

## Configure

In **N.I.N.A. > Options > Plugins > Chatstronomy**:

1. Select **Discord webhook — simple local notifications**.
2. Paste an HTTPS Discord webhook URL.
3. Choose the event families to send and save the profile.

The plugin creates the local runtime bootstrap configuration and starts the
bundled runtime. There is no separate observatory URL or background service to
configure.

Webhook URLs are credentials. Do not put them in logs, screenshots, issue
reports, or version control. Use **Hosted Hub** instead when several machines
need one centrally managed Discord application and slash commands.

## Delivery behavior

- Event families can be enabled independently per N.I.N.A. profile.
- Popup notifications are enabled by default.
- Raw log levels are opt-in because they can be noisy and contain local paths.
- Image notifications include the Direct thumbnail produced by the plugin.
- Autofocus and guiding histories are rendered as PNG graph attachments.
- Image cooldown limits routine image posts without suppressing state updates.

Discord delivery errors are logged and do not interrupt the N.I.N.A. sequence.

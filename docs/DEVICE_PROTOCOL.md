# Observatory device protocol v1

AutoPierCam is a user-owned `pier_camera`, not a N.I.N.A. telescope/profile.
The Hub's **Telescopes** tab includes a separate **Pier cameras & devices**
section. Create a camera, generate a code, and explicitly choose its channels.
Register the server under **Discord delivery** first. The owner must also manage
that server; a live bot membership and text-channel check is required.

Camera feeds can share a telescope's channel without changing slash-command
routing. A server manager can remove a camera feed from Discord delivery but
cannot pair, revoke or delete another user's camera. Device ownership does not
grant telescope or N.I.N.A. permissions. Share codes do not authorize devices.

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

The follow-up transport uses an outbound WSS connection so the Hub can request
a current snapshot without opening inbound ports at the observatory. This is
not a generic RPC or camera-control channel. Local sharing consent governs
both event uploads and snapshot responses; no automatic pairing enables images.
Event detection and snapshot sharing must be opt-in in AutoPierCam.

The foundation change only provides device management, destination authorization,
and pairing. It does not yet transmit images or label devices as online.

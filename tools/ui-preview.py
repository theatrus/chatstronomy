#!/usr/bin/env python3
"""Render the hub page with stubbed API data for layout screenshots.

Extracts INDEX_HTML from src/hub/web_ui.rs and injects a fetch() stub
before the page script, so the real markup/CSS/JS runs against fixture
data — no server, no login. Variants: full, delivery, needs-server, empty,
long-name, loggedout, token, control-enabled
(token = full plus an auto-click on the second telescope's "Connect rig"
so the pairing box is visible in the screenshot).

Usage:
    python3 tools/ui-preview.py full
    python3 tools/ui-preview.py delivery
    npx -y playwright install chromium   # once
    npx -y playwright screenshot --viewport-size "1280,900" --full-page \
        --wait-for-timeout 1200 target/ui-preview/preview-full.html out.png

Look at the screenshot before shipping UI changes.
"""
import os
import json
import sys

SRC = "src/hub/web_ui.rs"
OUT_DIR = "target/ui-preview/"

session = {
    "authenticated": True,
    "csrf_token": "test",
    "user": {"id": "1", "username": "atrus", "email": "a@example.com",
             "email_verified": True, "avatar_url": None},
}

options = {
    "bot_configured": True,
    "channels": [
        {"id": "101", "name": "astro-images"},
        {"id": "102", "name": "observatory-status"},
        {"id": "103", "name": "general"},
        {"id": "104", "name": "alerts"},
    ],
    "roles": [
        {"id": "201", "name": "Astronomers"},
        {"id": "202", "name": "Imaging Team"},
        {"id": "203", "name": "Members"},
    ],
}

guilds = {
    "bot_configured": True,
    "guilds": [
        {"id": "1000", "name": "Backyard Observatory", "registered": True,
         "bot_installed": True, "install_url": "https://example.com"},
        {"id": "2000", "name": "Astro Club", "registered": True,
         "bot_installed": True, "install_url": "https://example.com"},
        {"id": "3000", "name": "New Server", "registered": False,
         "bot_installed": False, "install_url": "https://example.com"},
    ],
}

telescopes = {
    "telescopes": [
        {"id": 1, "name": "c925", "owner_id": "1", "image_cooldown_seconds": 60,
         "connected": True, "commands_enabled": False,
         "attachments": [
             {"attachment_id": 11, "telescope_id": 1, "guild_id": "1000",
              "guild_name": "Backyard Observatory", "can_command": True,
              "write_policy": "roles", "allowed_role_ids": ["201"],
              "channels": [{"route_id": 31, "guild_id": "1000", "channel_id": "101",
                            "channel_name": "astro-images", "guild_name": "Backyard Observatory"}]},
             {"attachment_id": 12, "telescope_id": 1, "guild_id": "2000",
              "guild_name": "Astro Club", "can_command": False,
              "write_policy": "admins", "allowed_role_ids": [],
              "channels": [{"route_id": 32, "guild_id": "2000", "channel_id": "104",
                            "channel_name": "alerts", "guild_name": "Astro Club"}]},
         ]},
        {"id": 2, "name": "esprit100", "owner_id": "1", "image_cooldown_seconds": 120,
         "connected": False, "commands_enabled": False, "attachments": []},
    ],
}

attachments_1000 = {
    "attachments": [
        {"attachment_id": 11, "telescope_id": 1, "guild_id": "1000",
         "telescope_name": "c925", "owner_name": "atrus", "owned_by_me": True,
         "can_command": True, "write_policy": "roles", "allowed_role_ids": ["201"],
         "connected": True, "commands_enabled": False,
         "channels": [{"route_id": 31, "guild_id": "1000", "channel_id": "101",
                       "channel_name": "astro-images", "guild_name": "Backyard Observatory"},
                      {"route_id": 33, "guild_id": "1000", "channel_id": "102",
                       "channel_name": "observatory-status", "guild_name": "Backyard Observatory"}]},
    ],
}

attachments_2000 = {
    "attachments": [
        {"attachment_id": 12, "telescope_id": 1, "guild_id": "2000",
         "telescope_name": "c925", "owner_name": "somebody-else", "owned_by_me": False,
         "can_command": False, "write_policy": "admins", "allowed_role_ids": [],
         "connected": True, "commands_enabled": False,
         "channels": [{"route_id": 32, "guild_id": "2000", "channel_id": "104",
                       "channel_name": "alerts", "guild_name": "Astro Club"}]},
    ],
}

variant = sys.argv[1] if len(sys.argv) > 1 else "full"
if variant == "needs-server":
    telescopes = {"telescopes": [telescopes["telescopes"][1]]}
    guilds = {
        "bot_configured": True,
        "guilds": [
            {"id": "3000", "name": "New Server", "registered": False,
             "bot_installed": False, "install_url": "https://example.com"},
        ],
    }
elif variant == "empty":
    telescopes = {"telescopes": []}
    attachments_1000 = {"attachments": []}
    attachments_2000 = {"attachments": []}
elif variant == "long-name":
    session["user"]["username"] = "observatory-operator-with-a-long-discord-name"
elif variant == "loggedout":
    session = {"authenticated": False}
elif variant == "control-enabled":
    telescopes["telescopes"][0]["commands_enabled"] = True
    attachments_1000["attachments"][0]["commands_enabled"] = True

fixtures = {
    "/api/session": session,
    "/api/guilds": guilds,
    "/api/telescopes": telescopes,
    "/api/guilds/1000/attachments": attachments_1000,
    "/api/guilds/2000/attachments": attachments_2000,
    "/api/guilds/1000/options": options,
    "/api/guilds/2000/options": options,
    "/api/telescopes/2/pairing-token":
        {"token": "cspt_EXAMPLEEXAMPLEEXAMPLEEXAMPLEEXAMPLE", "expires_in_seconds": 3600},
}

click_script = ""
if variant == "token":
    click_script = ("<script>setTimeout(() => {\n"
                    "  const rows = document.querySelectorAll('.telescope');\n"
                    "  if (rows[1]) rows[1].querySelector('.b-token').click();\n"
                    "}, 500);</script>\n")
elif variant == "delivery":
    click_script = ("<script>setTimeout(() => {\n"
                    "  document.querySelector('#tab-delivery').click();\n"
                    "}, 500);</script>\n")

stub = """<script>
const FIXTURES = %s;
window.fetch = async (path, opts) => {
  const data = FIXTURES[path.split('?')[0]];
  return {
    ok: data !== undefined,
    headers: { get: () => 'application/json' },
    json: async () => data,
    text: async () => data === undefined ? 'not found in fixtures' : JSON.stringify(data),
  };
};
</script>
""" % json.dumps(fixtures)

src = open(SRC, encoding="utf-8").read()
start = src.index('r#"') + 3
end = src.index('"#;', start)
html = src[start:end]
html = html.replace("<script>", stub + "<script>", 1) + click_script
os.makedirs(OUT_DIR, exist_ok=True)
out = OUT_DIR + "preview-" + variant + ".html"
open(out, "w", encoding="utf-8").write(html)
print(out)

# web-debug

Browser-based WebSocket debug client for the mlink daemon. Pure static HTML/CSS/JS, no build step, no dependencies.

## Usage

1. Start the mlink daemon so it exposes a WS endpoint, e.g. `ws://127.0.0.1:7890/ws`.
2. Open `index.html` directly in a browser (double-click, or `open index.html` on macOS).
3. Confirm the URL in the top bar matches your daemon, click **Connect**.
4. After the status dot turns green (`ready`), join a room code and start sending messages.

No server needed — it's a single file, runs from `file://`.

## What you can do

- Connect / disconnect, see `app_uuid` from `ready`
- Join / leave rooms; switch the active room from the left list
- Send text messages to the selected room (wrapped as `{type:"send", payload:{room, payload:{text}}}`)
- See peers from `room_state` events in the right column
- Watch the raw JSON log (bottom panel) for every frame in and out; toggle `ping/pong` noise; manual `ping` button

## Not shipped

This example is for local development only — it is not packaged into releases.

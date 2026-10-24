# macOS Tailscale HTTP service

This template runs the Rust HTTP API as a user LaunchAgent. It binds to loopback and is reached through private Tailscale Serve. It does not run Media's Temporal worker, hold R2 credentials, or read a `.env` file.

Edit [co.byulmaru.hope-api.plist](co.byulmaru.hope-api.plist) and replace `YOUR_USER` in the binary and model paths. The template uses the release binary, `api/models`, `127.0.0.1:8787`, and Nightshade. Startup verifies all three pinned model files but initializes only the selected Nightshade session.

Build the API and prepare the models using [the API guide](../README.md). Check port 8787 and the current Serve configuration before launch. Keep `HOPE_API_ADDR` on a loopback IP, configure private HTTPS forwarding through Tailscale Serve to that loopback listener, and keep Funnel disabled. If port 8787 is occupied, leave its listener alone and select another loopback port for the API and Serve target. The API has no application-level authentication; Tailscale controls remote access. This template does not change Tailscale Serve configuration.

## Operator sequence

1. Review the `YOUR_USER` paths and confirm the binary and model directory. A user LaunchAgent runs while that account is logged in; sleep pauses it, so keep the Mac mini awake while Media uses the API.
2. Build the release binary and prepare all three pinned model files as described in [the API guide](../README.md).
3. Inspect port 8787 and existing Tailscale Serve configuration. Leave any existing listener in place and review Serve routes before changes:

   ```sh
   lsof -nP -iTCP:8787 -sTCP:LISTEN
   tailscale serve status
   ```

4. Install the reviewed plist and load it into the logged-in user's launchd domain:

   ```sh
   mkdir -p "$HOME/Library/LaunchAgents"
   cp /path/to/reviewed/co.byulmaru.hope-api.plist "$HOME/Library/LaunchAgents/co.byulmaru.hope-api.plist"
   launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/co.byulmaru.hope-api.plist"
   launchctl print "gui/$(id -u)/co.byulmaru.hope-api"
   ```

5. Check the private HTTPS endpoint without credentials. `GET /health` should return `200` and report only Nightshade. Then run `POST /protect?algorithm=nightshade&iterations=2&max_edge=1024` with a 1280×1280 image. Require `200 image/webp`, `X-Hope-Model-Calls: 576`, and 1024×1024 dimensions; decode the output and compare pixels with the unprotected resize/encode baseline. A 30-minute request timeout accommodates the Media Activity budget.
6. During a separate Media cutover, keep the new HTTP consumer disabled while starting and testing the API service. After private Tailscale access and Nightshade model checks pass, stop the old `media-hope` Temporal poller, then enable the Media HTTP consumer. Run a protected upload, a fail-closed case, and an Activity cancellation check. For rollback, disable the HTTP consumer before restarting the old poller; never run both pollers on the same queue. The API LaunchAgent does not manage either consumer.

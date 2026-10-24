# macOS HTTP service template

This template runs the pure Rust HTTP API as a user LaunchAgent. It reads one API token from the current user's login Keychain at process start. It does not load Media's Temporal worker, R2 credentials, or any `.env` file.

The generic-password item must use service `co.byulmaru.hope-api` and account equal to the macOS login name from `id -un`. The wrapper prints only a short error if the item is missing or unreadable; the token is passed to the Rust process through `HOPE_API_TOKEN` and is not written to disk or command arguments.

Edit [co.byulmaru.hope-api.plist](co.byulmaru.hope-api.plist) and replace `YOUR_USER` in each path. The sample uses the release binary and `api/models`, sets the API to `127.0.0.1:8787`, and selects Nightshade. The model directory must contain all three pinned files because startup verifies every file before listening. Only the Nightshade session is initialized.

Build the API and prepare the pinned models using the steps in [the API guide](../README.md). Keep the LaunchAgent's `HOPE_API_ADDR` on loopback. Configure Tailscale Serve separately to provide private HTTPS access to `http://127.0.0.1:8787`; keep Funnel disabled and preserve the API Bearer authentication.

The template does not create the Keychain item, install or load the LaunchAgent, start a service, or change Tailscale settings. Provisioning and live Mac mini smoke testing require separate authorization.

## Operator sequence after approval

1. Review every `YOUR_USER` path in the plist and confirm the release binary and model directory. A user LaunchAgent runs only while that account is logged in; sleep pauses it, so keep the Mac mini awake while Media uses the API.
2. Build the release binary and prepare all three pinned model files as described in [the API guide](../README.md). Selected mode still requires all three files. If the Keychain item is not present, add or update the generic-password item interactively; `-w` at the end prompts without putting the token in shell history or process arguments:

   ```sh
   /usr/bin/security add-generic-password -U \
     -s co.byulmaru.hope-api \
     -a "$(/usr/bin/id -un)" \
     -T /usr/bin/security \
     -w
   ```

3. Before installing the service, inspect port 8787 and existing Tailscale Serve configuration:

   ```sh
   lsof -nP -iTCP:8787 -sTCP:LISTEN
   tailscale serve status
   ```

   Resolve existing listeners and review Serve routes before changing them. The API must bind to `127.0.0.1:8787`; Serve should provide private HTTPS forwarding to that loopback address. Keep Funnel disabled.

4. After separate approval to install, copy the reviewed plist to `~/Library/LaunchAgents/co.byulmaru.hope-api.plist` and load it into the logged-in user's launchd domain:

   ```sh
   mkdir -p "$HOME/Library/LaunchAgents"
   cp /path/to/reviewed/co.byulmaru.hope-api.plist "$HOME/Library/LaunchAgents/co.byulmaru.hope-api.plist"
   launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/co.byulmaru.hope-api.plist"
   launchctl print "gui/$(id -u)/co.byulmaru.hope-api"
   ```

5. Check the private HTTPS endpoint without credentials; `GET /health` should return `401`. Then use an authenticated client that reads the token from Keychain to confirm health reports only `nightshade` and to run `POST /protect?algorithm=nightshade&iterations=2&max_edge=1024` with a 1280×1280 image. Require `200 image/webp`, `X-Hope-Model-Calls: 576`, and 1024×1024 dimensions; decode the output and compare pixels with the unprotected resize/encode baseline. A 30-minute request timeout accommodates the Media Activity budget. Do not print the token or put it in a command argument, plist, or file.
6. During a separately approved Media cutover, keep the new HTTP consumer disabled while starting and testing the pure HTTP service. After its private TLS, auth, and Nightshade model checks pass, stop the old `media-hope` Temporal poller, then enable the Media HTTP consumer. Run a protected upload, a fail-closed case, and an Activity cancellation check. If rolling back, disable the HTTP consumer before restarting the old poller; never run both pollers on the same queue. The API LaunchAgent does not manage either consumer.

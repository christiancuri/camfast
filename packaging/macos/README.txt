CamFast — installation

1. Drag "CamFast" to the "Applications" folder in this window.

2. First launch (the app is signed locally, not by Apple):
   - Open CamFast from Applications. macOS will warn that it cannot verify it.
     Click "Done" / "OK".
   - Go to System Settings > Privacy & Security, scroll down to the notice about
     "CamFast" and click "Open Anyway". Confirm with your password/Touch ID.
   - Alternative from the Terminal (does the same):
       xattr -dr com.apple.quarantine /Applications/CamFast.app

3. When macOS asks for LOCAL NETWORK access, click "Allow".
   Without it the app cannot reach the cameras.

4. The "Cameras" window opens by itself on first launch: add each camera
   (name, IP, username, password), use "Test Connection", "Save", and pick the
   layout positions. "Open at login" can be turned on under Preferences.

Shortcuts: double-click expands/restores a tile · right-click switches the
tile's camera · ⌘, opens "Cameras" · ⌘I toggles statistics.

Configuration: ~/Library/Application Support/camfast/config.toml
Logs:          ~/Library/Logs/camfast/
Requires macOS 13 or newer on a Mac with Apple silicon (M1 or later).

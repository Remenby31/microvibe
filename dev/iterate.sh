#!/bin/bash
# Full microvibe TUI iteration cycle:
# 1. Build release
# 2. Kill old microvibe (only microvibe)
# 3. Launch fresh in Ghostty
# 4. Send a test message
# 5. Wait and capture screenshot
#
# Usage: ./iterate.sh [message] [wait_seconds]

set -e
cd ~/projects/microvibe

MSG="${1:-hello}"
WAIT="${2:-8}"

echo "=== Building ==="
cargo build --release 2>&1 | grep -E "error|Finished"

echo "=== Killing old microvibe ==="
pkill -9 -f "microvibe --tui" 2>/dev/null || true
sleep 1

echo "=== Launching ==="
osascript -e '
tell application "Ghostty"
    set cfg to new surface configuration
    set w to new window with configuration cfg
    set t to focused terminal of selected tab of w
    input text "cd ~/projects/microvibe && ./target/release/microvibe --tui" to t
    send key "enter" to t
end tell
'
sleep 2

echo "=== Sending: $MSG ==="
osascript -e "
tell application \"Ghostty\"
    repeat with w in (every window)
        if name of w contains \"microvibe\" then
            activate window w
            set t to focused terminal of selected tab of w
            input text \"$MSG\" to t
            send key \"enter\" to t
            exit repeat
        end if
    end repeat
end tell
"

echo "=== Waiting ${WAIT}s for response ==="
sleep "$WAIT"

echo "=== Capturing ==="
bash dev/capture.sh microvibe /tmp/mv_iter.png
echo "=== Done: /tmp/mv_iter.png ==="

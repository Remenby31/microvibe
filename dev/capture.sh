#!/bin/bash
# Capture a Ghostty window by name fragment
NAME="${1:-microvibe}"
OUTPUT="${2:-/tmp/capture.png}"

# Focus the window
osascript <<EOF 2>/dev/null
tell application "Ghostty"
    repeat with w in (every window)
        if name of w contains "$NAME" then
            activate window w
            exit repeat
        end if
    end repeat
end tell
EOF

sleep 0.5

# Get position as 4 separate values
read -r X Y W H <<< $(osascript <<'ENDSCRIPT'
tell application "System Events"
    tell process "Ghostty"
        repeat with w in (every window)
            if name of w contains "microvibe" then
                set p to position of w
                set s to size of w
                return "" & (item 1 of p) & " " & (item 2 of p) & " " & (item 1 of s) & " " & (item 2 of s)
            end if
        end repeat
    end tell
end tell
ENDSCRIPT
)

if [ -z "$X" ]; then
    echo "ERROR: window not found"
    exit 1
fi

screencapture -x -R "${X},${Y},${W},${H}" "$OUTPUT"
echo "$OUTPUT (${X},${Y},${W},${H})"

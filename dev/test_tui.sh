#!/bin/bash
# Automated TUI test suite
# Launches microvibe, sends inputs, captures screenshots, validates visually
set -e
cd ~/projects/microvibe

RESULTS=""
pass() { RESULTS="${RESULTS}\n✅ $1"; echo "✅ $1"; }
fail() { RESULTS="${RESULTS}\n❌ $1"; echo "❌ $1"; }

capture() {
    local name="$1"
    osascript -e '
    tell application "Ghostty"
        repeat with w in (every window)
            if name of w is "microvibe" or name of w contains "--tui" then
                activate window w
                exit repeat
            end if
        end repeat
    end tell
    '
    sleep 0.5
    read -r X Y W H <<< $(osascript -e '
    tell application "System Events"
        tell process "Ghostty"
            set fw to front window
            set p to position of fw
            set s to size of fw
            return "" & (item 1 of p) & " " & (item 2 of p) & " " & (item 1 of s) & " " & (item 2 of s)
        end tell
    end tell
    ')
    screencapture -x -R "${X},${Y},${W},${H}" "/tmp/test_${name}.png"
    echo "  captured: /tmp/test_${name}.png"
}

send_keys() {
    osascript -e "
    tell application \"Ghostty\"
        repeat with w in (every window)
            if name of w is \"microvibe\" or name of w contains \"--tui\" then
                set t to focused terminal of selected tab of w
                input text \"$1\" to t
                send key \"enter\" to t
                exit repeat
            end if
        end repeat
    end tell
    "
}

send_raw() {
    osascript -e "
    tell application \"Ghostty\"
        repeat with w in (every window)
            if name of w is \"microvibe\" or name of w contains \"--tui\" then
                set t to focused terminal of selected tab of w
                input text \"$1\" to t
                exit repeat
            end if
        end repeat
    end tell
    "
}

echo "=== Building ==="
cargo build --release 2>&1 | tail -1

echo "=== Killing old instances ==="
pkill -f "microvibe" 2>/dev/null || true
sleep 1

echo "=== Launching microvibe --tui ==="
osascript <<'EOF'
tell application "Ghostty"
    set cfg to new surface configuration
    set w to new window with configuration cfg
    set t to focused terminal of selected tab of w
    input text "cd ~/projects/microvibe && ./target/release/microvibe --tui" to t
    send key "enter" to t
end tell
EOF
sleep 3

echo ""
echo "=== TEST 1: Startup banner ==="
capture "01_startup"
pass "T1: Startup — banner, input box, status bar"

echo ""
echo "=== TEST 2: Simple message ==="
send_keys "salut"
sleep 8
capture "02_salut"
pass "T2: Simple message — user msg orange, response, thinking"

echo ""
echo "=== TEST 3: Slash command completion ==="
send_raw "/he"
sleep 1
capture "03_completion"
send_keys "lp"
sleep 1
capture "03b_help"
pass "T3: Completion popup + /help output"

echo ""
echo "=== TEST 4: Code block + table ==="
send_keys "ecris un hello world rust et un tableau markdown de 2 lignes"
sleep 12
capture "04_markdown"
pass "T4: Code block + table rendering"

echo ""
echo "=== TEST 5: /stats ==="
send_keys "/stats"
sleep 1
capture "05_stats"
pass "T5: /stats command"

echo ""
echo "=== TEST 6: /undo ==="
send_keys "/undo"
sleep 1
capture "06_undo"
pass "T6: /undo command"

echo ""
echo "=== TEST 7: /log ==="
send_keys "/log"
sleep 1
capture "07_log"
pass "T7: /log command"

echo ""
echo "=== RESULTS ==="
echo -e "$RESULTS"
echo ""
echo "Screenshots saved in /tmp/test_*.png"

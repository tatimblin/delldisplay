# Sourced by delldisplay-mcp and session-start. Prints the path of the
# delldisplay binary, or returns 1. Claude Code launched from the Dock
# inherits a minimal PATH, so look where Homebrew and cargo put it too.
find_delldisplay() {
    if command -v delldisplay >/dev/null 2>&1; then
        command -v delldisplay
        return 0
    fi
    for dir in /opt/homebrew/bin /usr/local/bin "${CARGO_HOME:-$HOME/.cargo}/bin"; do
        if [ -x "$dir/delldisplay" ]; then
            printf '%s\n' "$dir/delldisplay"
            return 0
        fi
    done
    return 1
}

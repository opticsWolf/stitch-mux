# Source this file to report program status to stitch-mux over the local PTY.
# It writes only an OSC 7501 escape sequence to the terminal; no daemon API,
# network socket, or privileged hook is involved.
#
#   . ./scripts/stitch-mux-status.sh
#   stitch_status working "Running tests"
#   cargo test && stitch_status done "Tests passed" || stitch_status error "Tests failed"

stitch_status() {
  state=$1
  shift
  message=$(printf '%s' "$*" | base64 | tr -d '\n')
  printf '\033]7501;state=%s:app=shell:msg=%s\033\' "$state" "$message"
}

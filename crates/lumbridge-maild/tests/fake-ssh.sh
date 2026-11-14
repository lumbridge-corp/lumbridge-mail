#!/bin/sh
# A stand-in for ssh in tests: logs its arguments, skips the options, and runs the remote command here,
# in the "server's" home, the way sshd hands it to the login shell (one string, run by sh -c).
#   FAKE_SSH_SERVER_HOME  LUMBRIDGE_MAIL_HOME on the "server" (also the directory the command starts in)
#   FAKE_SSH_LOG          append each command line here
#   FAKE_SSH_CUT          a file: the first `... pipe` creates it and is cut after a second (ssh dropped)
[ -n "$FAKE_SSH_LOG" ] && printf '%s\n' "$*" >> "$FAKE_SSH_LOG"
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-e|-p|-l|-i|-F|-S) shift 2 ;;
    --) shift; break ;;
    -*) shift ;;
    *) break ;;
  esac
done
host=$1; shift
if [ "$host" = unreachable ]; then
  echo "ssh: Could not resolve hostname $host: Name or service not known" >&2
  exit 255
fi
export LUMBRIDGE_MAIL_HOME="$FAKE_SSH_SERVER_HOME"
unset LBMAIL_REMOTE LBMAIL_AGENT
cd "$FAKE_SSH_SERVER_HOME" || exit 255
case "$*" in
  *" pipe")
    if [ -n "$FAKE_SSH_CUT" ] && [ ! -e "$FAKE_SSH_CUT" ]; then
      : > "$FAKE_SSH_CUT"
      # An async command's stdin is /dev/null in sh unless redirected from another descriptor.
      exec 3<&0
      sh -c "exec $*" <&3 3<&- &
      child=$!
      sleep 1
      kill "$child" 2>/dev/null
      echo "Connection to $host closed by remote host." >&2
      exit 255
    fi ;;
esac
exec sh -c "$*"

#!/usr/bin/env bash
# Install the Lumbridge Mail daemon on a headless server, over ssh, for the ssh user (no sudo there).
# Clients (the app, lbmail) on other machines then use it with `[daemon] remote = "<host>"`: they run
# `ssh -T <host> lbmail pipe`, so the server opens no port of its own.
#
#   deploy/install-server.sh <ssh host> [--bin-dir DIR] [--no-start] [--dry-run]
#   deploy/install-server.sh <ssh host> --uninstall [--dry-run]
#
# On the host, in the ssh user's home:
#   ~/.local/bin/lumbridge-maild, ~/.local/bin/lbmail     from DIR (default: the build for the host's CPU,
#                                                         target/<arch>-unknown-linux-musl/release)
#   ~/.config/systemd/user/lumbridge-maild.service        deploy/lumbridge-maild.service (Restart=on-failure)
#   ~/.config/lumbridge-mail/config.toml                  a commented sample with NO accounts, only when
#                                                         there is no config yet (an existing one is kept)
# then enables and starts the unit (--no-start: installs only). A fresh config has no accounts, so the
# daemon syncs nothing until accounts are added on the server (`ssh -t <host> lbmail account add`).
# User units stop at logout unless lingering is on; the script says when `sudo loginctl enable-linger`
# is needed (it never runs sudo).
#
# --uninstall stops and removes the unit and the two binaries; config, mail data and keyring entries stay.
#
# Build first, on a build machine (never on the server):  scripts/remote-arm64 <name>
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/.." && pwd)
host=""
bin_dir=""
start=1
uninstall=0
dry=0
bins=(lumbridge-maild lbmail)

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) bin_dir=${2:?--bin-dir needs a directory}; shift 2 ;;
    --no-start) start=0; shift ;;
    --uninstall) uninstall=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    *) [ -z "$host" ] || { echo "one host at a time ($host, $1)" >&2; exit 2; }; host=$1; shift ;;
  esac
done
[ -n "$host" ] || { echo "usage: deploy/install-server.sh <ssh host> [--uninstall] (see --help)" >&2; exit 2; }
case "$host" in -*|*[[:space:]]*) echo "$host is not an ssh destination" >&2; exit 2 ;; esac

ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10)
# Runs a script on the host (bash there); in --dry-run, shows it instead.
remote() {
  if [ "$dry" = 1 ]; then
    echo "+ ssh $host bash -s <<'SCRIPT'"; sed 's/^/    /' ; echo "  SCRIPT"
  else
    ssh "${ssh_opts[@]}" "$host" 'bash -s'
  fi
}

if [ "$uninstall" = 1 ]; then
  remote <<'SCRIPT'
set -u
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/lumbridge-maild.service
if [ -f "$unit" ]; then
  systemctl --user disable --now lumbridge-maild.service 2>/dev/null || true
  rm -f "$unit"
  systemctl --user daemon-reload 2>/dev/null || true
fi
rm -f "$HOME/.local/bin/lumbridge-maild" "$HOME/.local/bin/lbmail"
echo "Removed the Lumbridge Mail daemon from $(hostname). Left alone: ~/.config/lumbridge-mail, ~/.local/share/lumbridge-mail, keyring entries."
SCRIPT
  exit 0
fi

# Which build: the host's CPU decides (unless --bin-dir says).
if [ -z "$bin_dir" ]; then
  if [ "$dry" = 1 ]; then
    arch=aarch64
  else
    arch=$(ssh "${ssh_opts[@]}" "$host" uname -m </dev/null)
  fi
  case "$arch" in
    aarch64|arm64) bin_dir="$repo/target/aarch64-unknown-linux-musl/release" ;;
    x86_64) bin_dir="$repo/target/x86_64-unknown-linux-musl/release"
            [ -x "$bin_dir/lumbridge-maild" ] || bin_dir="$repo/target/release" ;;
    *) echo "$host is $arch: build for it and pass --bin-dir" >&2; exit 1 ;;
  esac
fi
missing=0
for b in "${bins[@]}"; do
  [ -x "$bin_dir/$b" ] || { echo "No $bin_dir/$b" >&2; missing=1; }
done
if [ "$missing" = 1 ]; then
  echo "Build them first: scripts/remote-arm64 <name> (aarch64), or pass --bin-dir DIR." >&2
  exit 1
fi

# Copy beside the old ones, then rename: a running daemon keeps its file, the next start gets the new one.
stage=".local/bin/.lumbridge-mail-install.$$"
if [ "$dry" = 1 ]; then
  echo "+ ssh $host mkdir -p $stage && scp ${bins[*]/#/$bin_dir/} -> $host:$stage/"
else
  ssh "${ssh_opts[@]}" "$host" "mkdir -p $stage && chmod 700 $stage" </dev/null
  scp -q "${ssh_opts[@]}" "${bins[@]/#/$bin_dir/}" "$host:$stage/"
fi

unit_text=$(cat "$here/lumbridge-maild.service")
remote <<SCRIPT
set -eu
stage="\$HOME/$stage"
for b in ${bins[*]}; do
  chmod 0755 "\$stage/\$b"
  mv -f "\$stage/\$b" "\$HOME/.local/bin/\$b"
done
rmdir "\$stage"
"\$HOME/.local/bin/lbmail" --version >/dev/null

conf=\${XDG_CONFIG_HOME:-\$HOME/.config}
mkdir -p "\$conf/systemd/user"
mkdir -p "\$conf/lumbridge-mail" && chmod 700 "\$conf/lumbridge-mail"
cat > "\$conf/systemd/user/lumbridge-maild.service" <<'UNIT'
$unit_text
UNIT
chmod 0644 "\$conf/systemd/user/lumbridge-maild.service"

cfg="\$conf/lumbridge-mail/config.toml"
if [ ! -e "\$cfg" ]; then
  umask 077
  cat > "\$cfg" <<'TOML'
# Lumbridge Mail daemon on this server (written by deploy/install-server.sh). No accounts yet: the daemon
# syncs nothing until you add one, here on the server:
#   lbmail account add                 # at a terminal: asks (demo mail, Gmail through gog, IMAP, Resend)
#   lbmail account add --email sam@example.com --provider imap --imap-host imap.example.com --readonly
# A first live account should be read-only (readonly = true). Every key: crates/lumbridge-maild/README.md.
#
# Headless: there is no desktop keyring (Secret Service) here. Give IMAP accounts a password_command
# (e.g. ["pass", "show", "mail/work"]) or password_env, never a password in this file.
#
# Clients on other machines put this in THEIR config.toml (and nothing else):
#   [daemon]
#   remote = "<this host's ssh name>"

[daemon]
# sync_interval_s = 300

[ai]
# No endpoint by default: AI stays off until base_url and model are set.
# base_url = "http://127.0.0.1:8000/v1"
# model = "my-model"
TOML
  echo "Wrote \$cfg (no accounts)."
else
  echo "Kept \$cfg."
fi

systemctl --user daemon-reload
if [ "$start" = 1 ]; then
  systemctl --user enable lumbridge-maild.service
  systemctl --user restart lumbridge-maild.service
  sleep 1
  systemctl --user --no-pager --lines=5 status lumbridge-maild.service || true
fi
linger=\$(loginctl show-user "\$(id -un)" -p Linger --value 2>/dev/null || echo unknown)
if [ "\$linger" != yes ]; then
  echo "NOTE: lingering is \$linger for \$(id -un): the daemon stops when your last session ends. Once, as root:"
  echo "      sudo loginctl enable-linger \$(id -un)"
fi
echo "Installed on \$(hostname): \$("\$HOME/.local/bin/lumbridge-maild" --version)."
SCRIPT

echo
echo "Point a client at it (in the client's ~/.config/lumbridge-mail/config.toml):"
echo "    [daemon]"
echo "    remote = \"$host\""
echo "then: lbmail status   (says: daemon ...: remote:$host)"

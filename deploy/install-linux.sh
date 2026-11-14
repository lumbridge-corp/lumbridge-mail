#!/usr/bin/env bash
# Install Lumbridge Mail for the current user on Linux (no sudo): the app, the daemon and the CLI.
#
#   deploy/install-linux.sh [--bin-dir DIR] [--systemd | --enable] [--default-mailto] [--dry-run]
#   deploy/install-linux.sh --uninstall [--dry-run]
#
# Installs, under PREFIX (default ~/.local):
#   bin/lumbridge-mail, bin/lumbridge-maild, bin/lbmail      from DIR (default target/release)
#   share/applications/com.lumbridgecorp.mail.desktop        the launcher; handles mailto: links
#   share/icons/hicolor/*/apps/com.lumbridgecorp.mail.*      the icon
# The app starts lumbridge-maild itself when it is not running (it looks next to itself first), so
# nothing else is needed. --systemd also installs a systemd user unit, so mail keeps syncing with no
# window open: then `systemctl --user enable --now lumbridge-maild`; --enable does that too (the same unit
# deploy/install-server.sh puts on a server). --default-mailto makes the app the
# default mailto: handler. --uninstall removes all of the above (and stops the unit if it was
# installed); config, mail data and the keyring are left alone.
#
# Build first, on a build machine:
#   cargo build --release -p lumbridge-mail-app -p lumbridge-maild
# The app needs glibc 2.39 (Ubuntu 24.04) or newer and the usual GPUI libraries: libxkbcommon(-x11),
# libxcb, a Vulkan driver (Mesa), fontconfig; Wayland is used when there. notify-send shows
# notifications. See README.md, "Install and run".
set -euo pipefail

app_id=com.lumbridgecorp.mail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/.." && pwd)
src="$repo/target/release"
prefix=${PREFIX:-$HOME/.local}
systemd=0
enable=0
default_mailto=0
uninstall=0
dry=0
bins=(lumbridge-mail lumbridge-maild lbmail)

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) src=${2:?--bin-dir needs a directory}; shift 2 ;;
    --systemd) systemd=1; shift ;;
    --enable) systemd=1; enable=1; shift ;;
    --default-mailto) default_mailto=1; shift ;;
    --uninstall) uninstall=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done

if [ "$(id -u)" = 0 ]; then
  echo "Run this as yourself, not root: it installs into your home directory." >&2
  exit 1
fi

bin_dir="$prefix/bin"
apps_dir="$prefix/share/applications"
icons_dir="$prefix/share/icons/hicolor"
desktop="$apps_dir/$app_id.desktop"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
unit="$unit_dir/lumbridge-maild.service"

run() {
  if [ "$dry" = 1 ]; then printf '+ %q' "$1"; shift; printf ' %q' "$@"; echo; else "$@"; fi
}

refresh() {
  if command -v update-desktop-database >/dev/null 2>&1; then run update-desktop-database -q "$apps_dir" || true; fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then run gtk-update-icon-cache -q -t "$icons_dir" || true; fi
}

have_user_systemd() {
  command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1
}

if [ "$uninstall" = 1 ]; then
  if [ -f "$unit" ]; then
    if have_user_systemd; then run systemctl --user disable --now lumbridge-maild.service || true; fi
    run rm -f "$unit"
    if have_user_systemd; then run systemctl --user daemon-reload || true; fi
  fi
  for b in "${bins[@]}"; do run rm -f "$bin_dir/$b"; done
  run rm -f "$desktop"
  for s in 32 64 128 256 512; do run rm -f "$icons_dir/${s}x${s}/apps/$app_id.png"; done
  run rm -f "$icons_dir/scalable/apps/$app_id.svg"
  refresh
  echo "Removed Lumbridge Mail. Left alone: ~/.config/lumbridge-mail (config.toml), ~/.local/share/lumbridge-mail (mail), keyring entries."
  exit 0
fi

missing=0
for b in "${bins[@]}"; do
  if [ ! -x "$src/$b" ]; then echo "No $src/$b" >&2; missing=1; fi
done
if [ "$missing" = 1 ]; then
  echo "Build all three first: cargo build --release -p lumbridge-mail-app -p lumbridge-maild (or pass --bin-dir DIR)." >&2
  exit 1
fi

run mkdir -p "$bin_dir" "$apps_dir" "$icons_dir/scalable/apps"
for b in "${bins[@]}"; do run install -m 0755 "$src/$b" "$bin_dir/$b"; done
for s in 32 64 128 256 512; do
  run mkdir -p "$icons_dir/${s}x${s}/apps"
  run install -m 0644 "$here/app/icons/lumbridge-mail-$s.png" "$icons_dir/${s}x${s}/apps/$app_id.png"
done
run install -m 0644 "$here/app/icons/lumbridge-mail.svg" "$icons_dir/scalable/apps/$app_id.svg"

# Desktop entries quote an Exec path that has spaces in it.
exe="$bin_dir/lumbridge-mail"
case "$exe" in *" "*) exe_q="\"$exe\"" ;; *) exe_q=$exe ;; esac
entry="[Desktop Entry]
Type=Application
Version=1.5
Name=Lumbridge Mail
GenericName=Mail
Comment=Read and write mail, with an assistant that stays on your machines
Exec=$exe_q %u
TryExec=$exe
Icon=$app_id
Terminal=false
Categories=Network;Email;
Keywords=mail;email;inbox;
MimeType=x-scheme-handler/mailto;
StartupNotify=true
StartupWMClass=$app_id
Actions=compose;

[Desktop Action compose]
Name=New message
Exec=$exe_q mailto:
"
if [ "$dry" = 1 ]; then
  echo "+ write $desktop:"; printf '%s' "$entry" | sed 's/^/    /'
else
  printf '%s' "$entry" > "$desktop"
  chmod 0644 "$desktop"
fi
refresh

if [ "$systemd" = 1 ]; then
  run mkdir -p "$unit_dir"
  # The unit runs the daemon from where it was just installed (PREFIX may not be ~/.local).
  if [ "$dry" = 1 ]; then
    echo "+ write $unit (ExecStart=$bin_dir/lumbridge-maild)"
  else
    sed "s|^ExecStart=.*|ExecStart=$bin_dir/lumbridge-maild|" "$here/lumbridge-maild.service" > "$unit"
    chmod 0644 "$unit"
  fi
  if have_user_systemd; then run systemctl --user daemon-reload || true; fi
  if [ "$enable" = 1 ]; then
    if have_user_systemd; then
      run systemctl --user enable --now lumbridge-maild.service
    else
      echo "No user systemd here: start it with lumbridge-maild (or open the app)." >&2
    fi
  fi
fi

if [ "$default_mailto" = 1 ]; then
  if command -v xdg-mime >/dev/null 2>&1; then
    run xdg-mime default "$app_id.desktop" x-scheme-handler/mailto
  else
    echo "xdg-mime not found: set Lumbridge Mail as the mail handler in your desktop's settings." >&2
  fi
fi

echo "Installed Lumbridge Mail to $bin_dir: lumbridge-mail, lumbridge-maild, lbmail (launcher $desktop)."
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "Note: $bin_dir is not on your PATH (the launcher works anyway; lbmail needs it)." ;;
esac
if [ "$enable" = 1 ]; then
  echo "Daemon running as a user service (journalctl --user -u lumbridge-maild -f). It runs while you are logged in;"
  echo "to keep it up without a login: sudo loginctl enable-linger $USER"
elif [ "$systemd" = 1 ]; then
  echo "Daemon as a service: systemctl --user enable --now lumbridge-maild"
else
  echo "Open Lumbridge Mail: it starts lumbridge-maild and offers \"Try with demo mail\" or \"Add an account\"."
fi

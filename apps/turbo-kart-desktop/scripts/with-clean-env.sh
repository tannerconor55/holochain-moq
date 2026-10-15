#!/bin/sh
# Runs a command (e.g. `yarn dev`) without environment that breaks Electron.
#
# - ELECTRON_RUN_AS_NODE: set by VS Code (itself Electron) for its child
#   processes; it makes Electron run as plain Node, so the app cannot start.
# - Terminals inside the VS Code *snap* also inherit the snap's GTK/GIO/XDG
#   variables (pointing into /snap/code/...), which make Electron's GTK fail
#   ("Can't create a GtkStyleContext without a display connection"). Only
#   when that snap environment is detected, unset those and restore the
#   XDG_*_DIRS the snap saved. Nothing changes outside that case.
unset ELECTRON_RUN_AS_NODE
if [ -n "${XDG_DATA_DIRS_VSCODE_SNAP_ORIG+x}" ] || [ "${SNAP_NAME:-}" = code ]; then
  unset GDK_BACKEND GTK_PATH GTK_EXE_PREFIX GTK_IM_MODULE_FILE GTK_MODULES \
    GDK_PIXBUF_MODULEDIR GDK_PIXBUF_MODULE_FILE GIO_MODULE_DIR GSETTINGS_SCHEMA_DIR \
    GIO_LAUNCHED_DESKTOP_FILE GIO_LAUNCHED_DESKTOP_FILE_PID
  export XDG_DATA_DIRS="${XDG_DATA_DIRS_VSCODE_SNAP_ORIG:-/usr/local/share:/usr/share}"
  export XDG_CONFIG_DIRS="${XDG_CONFIG_DIRS_VSCODE_SNAP_ORIG:-/etc/xdg}"
fi
exec "$@"

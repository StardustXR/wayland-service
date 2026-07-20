# Stardust XR wayland-service

This is the wayland "compositor" implementation for StardustXR, it provides PanelItems for every wayland window that can be used by PanelShells like flatland.

## Usage

the wayland service doesn't pick a wayland socket on its own, instead it uses one passed to it, to adress that this repo contains `display-socket-finder`, a fully working setup could look something like this 
```bash
export WAYLAND_DISPLAY = $(display-socket-finder wayland)
export DISPLAY = $(display-socket-finder x11)
stardust-xr-wayland-service $WAYLAND_DISPLAY &
# maybe add a sleep here
xwayland-satellite $DISPLAY &
```

## Default PanelShell

the wayland-service will try to launch whatever is placed at `$XDG_CONFIG_HOME/stardust-wayland-service/default_panel_shell`, that may be a script, binary or symlink. it will set the `SDXR_WL_DEFAULT_PANEL_SHELL` env var to `1`, it also may set `SDXR_WL_APP_ID` and `SDXR_WL_TITLE` containing the windows provided app id and title if available at the point of launching, this may be used to implement simple "window rules".

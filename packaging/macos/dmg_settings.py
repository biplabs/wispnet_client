"""Finder presentation for the Wispnet installer (dmgbuild tooling only)."""
format = "UDZO"
filesystem = "HFS+"
files = [defines["app"]]
symlinks = {"Applications": "/Applications"}
background = defines["background"]
window_rect = ((180, 140), (720, 440))
default_view = "icon-view"
show_toolbar = False
show_status_bar = False
show_tab_view = False
show_pathbar = False
show_sidebar = False
arrange_by = None
icon_size = 96
# Finder rejects zero/tiny text sizes and then discards the icon-view styling.
# Keep only its small native filenames; the background has no label panels.
text_size = 10
label_pos = "bottom"
icon_locations = {"Wispnet.app": (172, 230), "Applications": (548, 230)}
# Do not set FinderInfo on the signed app: codesign --strict rejects it.
hide_extensions = []

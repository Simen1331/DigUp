-- ShowMe in Hyprland 0.55 and later (Omarchy 4): add to ~/.config/hypr/bindings.lua. Pick a key that's free:
-- `omarchy menu keybindings --print` shows what the keys do now. For hyprland.conf, see hyprland.conf.
hl.bind("SUPER + D", hl.dsp.exec_cmd("showme-panel --toggle"), { description = "ShowMe" })

-- The panel is an overlay (layer shell): blur behind it, as behind Omarchy's menus.
hl.layer_rule({ match = { namespace = "^showme$" }, blur = true, ignore_alpha = 0 })

-- The big search window (Ctrl+O in the panel) floats.
hl.window_rule({ match = { class = "^dev\\.showme\\.Panel$" }, float = true, size = { 1280, 820 } })

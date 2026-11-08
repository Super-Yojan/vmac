-- zen: SPC t z hides the file tree and other windows, to focus on one.

vmac.command("zen-mode", function()
  if vmac.sidebar_shown() then vmac.sidebar_close() end
  vmac.delete_other_windows()
  vmac.recenter("center")
  vmac.message("zen")
end)

vmac.map("normal", "SPC t z", "zen-mode")

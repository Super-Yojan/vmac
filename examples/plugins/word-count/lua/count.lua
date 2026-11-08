local M = {}

function M.words(text)
  local n = 0
  for _ in text:gmatch("%S+") do n = n + 1 end
  return n
end

function M.lines(text)
  local n = 0
  for _ in (text .. "\n"):gmatch("[^\n]*\n") do n = n + 1 end
  return n
end

return M

-- Offline pure-Lua module baked into the sandbox image at
-- /usr/lib/lua/5.4 (the guest package.path root). The zeroboot_interpreters
-- real-VM test requires exactly this module and asserts answer() == 42.
local M = {}

function M.answer()
    return 42
end

return M

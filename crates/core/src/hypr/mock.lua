-- hyprdeck: recording mock of the Hyprland `hl` Lua API.
-- Evaluates the user's config without a compositor and records every call
-- (with source file/line) into __hd_records for the Rust side to read.

local rec = {}
__hd_records = rec
local seq = 0
local in_start = false
local submap = ""

local function caller()
  -- level 1 = caller, 2 = push, 3 = hl.* function, 4 = config code
  local info = debug.getinfo(4, "Sl")
  if not info then return "?", 0 end
  local src = info.source or "?"
  if src:sub(1, 1) == "@" then src = src:sub(2) end
  return src, info.currentline or 0
end

local function push(kind, t)
  seq = seq + 1
  t.kind = kind
  t.seq = seq
  t.file, t.line = caller()
  t.on_start = in_start
  t.submap = submap
  rec[#rec + 1] = t
end

local stub = setmetatable({}, {
  __index = function() return function() return nil end end,
})

local function dsp_proxy(path)
  return setmetatable({}, {
    __index = function(_, k)
      return dsp_proxy(path == "" and k or (path .. "." .. k))
    end,
    __call = function(_, ...)
      return { __hd_dsp = path, __hd_n = select("#", ...), ... }
    end,
  })
end

local function run_scoped(fn, start)
  local prev = in_start
  in_start = start
  local ok, err = pcall(fn)
  in_start = prev
  if not ok then push("error", { msg = tostring(err) }) end
end

local api = {
  dsp = dsp_proxy(""),
  config = function(t) push("config", { value = t }) end,
  monitor = function(t) push("monitor", { value = t }) end,
  device = function(t) push("device", { value = t }) end,
  gesture = function(t) push("gesture", { value = t }) end,
  window_rule = function(t) push("window_rule", { value = t }); return stub end,
  layer_rule = function(t) push("layer_rule", { value = t }); return stub end,
  workspace_rule = function(t) push("workspace_rule", { value = t }); return stub end,
  permission = function(t) push("permission", { value = t }) end,
  bind = function(keys, action, opts)
    push("bind", { keys = keys, action = action, opts = opts })
    return stub
  end,
  unbind = function(keys) push("unbind", { keys = keys }) end,
  env = function(k, v) push("env", { key = k, value = v }) end,
  exec_cmd = function(cmd, rules) push("exec", { cmd = cmd, rules = rules }) end,
  on = function(event, cb)
    push("on", { event = event })
    if event == "hyprland.start" and type(cb) == "function" then
      run_scoped(cb, true)
    end
    return stub
  end,
  define_submap = function(name, a, b)
    push("submap", { name = name })
    local fn = type(a) == "function" and a or b
    if type(fn) == "function" then
      local prev = submap
      submap = name
      run_scoped(fn, in_start)
      submap = prev
    end
  end,
  timer = function() return stub end,
  version = function() return "0.56.0" end,
  get_monitors = function() return {} end,
  get_windows = function() return {} end,
  get_workspaces = function() return {} end,
  get_layers = function() return {} end,
  get_loaded_plugins = function() return {} end,
  get_workspace_windows = function() return {} end,
  get_current_submap = function() return submap end,
  notification = { create = function() return stub end, get = function() return {} end },
  layout = { register = function() end },
  plugin = setmetatable({}, { __index = function() return function() return nil end end }),
}

hl = setmetatable(api, {
  __index = function() return function() return nil end end,
})

-- No side effects while evaluating someone else's config.
os.execute = function() return nil end
os.exit = function() error("os.exit is disabled in hyprdeck's evaluator") end
os.remove = function() return nil end
os.rename = function() return nil end
io.popen = function() return nil end

-- Render a Lua value as Lua source text.
local function is_ident(s)
  return type(s) == "string" and s:match("^[%a_][%w_]*$") ~= nil
end

local ser
ser = function(v, depth)
  depth = depth or 0
  local t = type(v)
  if t == "string" then
    local s = string.format("%q", v):gsub("\\\n", "\\n")
    return s
  elseif t == "number" or t == "boolean" or t == "nil" then
    return tostring(v)
  elseif t == "function" then
    return "<lua function>"
  elseif t == "table" then
    if depth > 8 then return "{ … }" end
    if v.__hd_dsp ~= nil then
      local args = {}
      for i = 1, (v.__hd_n or 0) do args[#args + 1] = ser(v[i], depth + 1) end
      return "hl.dsp." .. v.__hd_dsp .. "(" .. table.concat(args, ", ") .. ")"
    end
    local parts = {}
    local n = #v
    for i = 1, n do parts[#parts + 1] = ser(v[i], depth + 1) end
    local keys = {}
    for k in pairs(v) do
      if not (math.type(k) == "integer" and k >= 1 and k <= n) then keys[#keys + 1] = k end
    end
    table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
    for _, k in ipairs(keys) do
      local key = is_ident(k) and k or ("[" .. ser(k, depth + 1) .. "]")
      parts[#parts + 1] = key .. " = " .. ser(v[k], depth + 1)
    end
    if #parts == 0 then return "{}" end
    return "{ " .. table.concat(parts, ", ") .. " }"
  else
    return "<" .. t .. ">"
  end
end
__hd_ser = ser

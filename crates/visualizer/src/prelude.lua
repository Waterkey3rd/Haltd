-- Runtime for visualizer rules. `__host` is provided by Rust for one apply call.
local rules = {}

local V = {}
V.__index = V

local function wrap(handle)
  if handle == nil then
    return nil
  end
  return setmetatable({ h = handle }, V)
end

function V:field(name)
  local value = wrap(__host.field(self.h, name))
  if value == nil then
    error("no field " .. name)
  end
  return value
end

function V:has(name) return __host.field(self.h, name) ~= nil end
function V:num() return __host.num(self.h) end
function V:str() return __host.str(self.h) end
function V:type() return __host.type(self.h) end
function V:deref() return wrap(__host.deref(self.h)) end
function V:at(index) return wrap(__host.at(self.h, index)) end
function V:pointee_size() return __host.pointee_size(self.h) end
function V:cstring(length) return __host.cstring(self.h, length) end

function V:tuple()
  local out = {}
  for _, handle in ipairs(__host.tuple(self.h)) do
    out[#out + 1] = wrap(handle)
  end
  return out
end

function V:elements()
  local out = {}
  for _, handle in ipairs(__host.elements(self.h)) do
    out[#out + 1] = wrap(handle)
  end
  return out
end

function visualizer(rule)
  table.insert(rules, 1, rule) -- later definitions take priority
end

local function summarize(values)
  local parts = {}
  for index, value in ipairs(values) do
    if index > 8 then
      parts[#parts + 1] = "..."
      break
    end
    parts[#parts + 1] = (value.value or value):str()
  end
  return "[" .. table.concat(parts, ", ") .. "]"
end

-- `resolving` means an expression is being turned into an address rather than a
-- value being shown, so a rule should offer a child even if it holds nothing now.
function __jlinkd_apply(type_name, handle, resolving)
  local value = wrap(handle)
  for _, rule in ipairs(rules) do
    local captures = { string.match(type_name, rule.match) }
    if captures[1] ~= nil then
      local children, summary
      if rule.children then
        local ok, result = pcall(rule.children, value, captures, resolving)
        if not ok then return nil end
        children = result
      end
      if resolving then
        summary = nil
      elseif rule.summary then
        local ok, result = pcall(rule.summary, value, captures)
        if not ok then return nil end
        summary = result
      elseif children then
        local ok, result = pcall(summarize, children)
        if ok then summary = result end
      end
      local out
      if children then
        out = {}
        for index, child in ipairs(children) do
          if child.value then
            out[index] = { name = child.name, h = child.value.h }
          else
            out[index] = { name = "[" .. (index - 1) .. "]", h = child.h }
          end
        end
      end
      return summary, out
    end
  end
  return nil
end

-- Default display rules. Matched with Lua patterns against qualified type names.

local function fmt(number)
  return string.format("%.5g", number)
end

-- units library -----------------------------------------------------------

local BASE_NAMES = { "m", "kg", "s", "rad", "A", "K", "mol", "cd", "byte" }

local function unit_suffix(arguments)
  -- The first nine ratios after base_unit are the dimensions, in BASE_NAMES order.
  local base = arguments:match("base_unit<(.*)$")
  if not base then
    return ""
  end
  local numerator, denominator, index = {}, {}, 0
  for top, bottom in base:gmatch("std::ratio<(%-?%d+), (%d+)>") do
    index = index + 1
    local power = tonumber(top) / tonumber(bottom)
    local name = BASE_NAMES[index]
    if name and power > 0 then
      numerator[#numerator + 1] = power == 1 and name or (name .. "^" .. fmt(power))
    elseif name and power < 0 then
      denominator[#denominator + 1] = power == -1 and name or (name .. "^" .. fmt(-power))
    end
  end
  local text = table.concat(numerator, "·")
  if #denominator > 0 then
    text = (text == "" and "1" or text) .. "/" .. table.concat(denominator, "·")
  end
  -- The outermost ratio is the conversion from the base unit.
  local top, bottom = arguments:match("^units::unit<std::ratio<(%-?%d+), (%d+)>")
  if top and top ~= bottom then
    local scale = tonumber(top) / tonumber(bottom)
    if top == "1" and bottom == "180" then
      return "deg"
    end
    text = "×" .. fmt(scale) .. " " .. text
  end
  return text
end

visualizer {
  match = "^units::unit_t<(.+)>$",
  summary = function(v, m)
    local suffix = unit_suffix(m[1])
    local value = fmt(v:field("m_value"):num())
    return suffix == "" and value or (value .. " " .. suffix)
  end,
  children = function() return {} end,
}

-- Eigen --------------------------------------------------------------------

local function matrix_view(v, rows, columns, row_major)
  local data = v:field("m_storage"):field("m_data"):field("array"):elements()
  local out = {}
  for row = 0, rows - 1 do
    for column = 0, columns - 1 do
      local index = row_major and (row * columns + column) or (column * rows + row)
      local name = columns == 1 and ("[" .. row .. "]") or ("[" .. row .. "," .. column .. "]")
      out[#out + 1] = { name = name, value = data[index + 1] }
    end
  end
  return out
end

local function matrix_summary(cells, rows, columns)
  local row_text = {}
  for row = 0, rows - 1 do
    local parts = {}
    for column = 0, columns - 1 do
      parts[#parts + 1] = fmt(cells[row * columns + column + 1].value:num())
    end
    row_text[#row_text + 1] = table.concat(parts, ", ")
  end
  if columns == 1 or rows == 1 then
    return "[" .. table.concat(row_text, columns == 1 and ", " or "") .. "]"
  end
  return "[" .. table.concat(row_text, "; ") .. "]"
end

local function ordered_cells(v, m)
  local rows, columns = tonumber(m[2]), tonumber(m[3])
  -- Options bit 0 selects row-major storage.
  local row_major = (tonumber(m[4]) & 1) == 1
  return matrix_view(v, rows, columns, row_major), rows, columns
end

visualizer {
  match = "^Eigen::Matrix<([%w_ :]+), (%d+), (%d+), (%d+),",
  children = function(v, m)
    return (ordered_cells(v, m))
  end,
  summary = function(v, m)
    return matrix_summary(ordered_cells(v, m))
  end,
}

visualizer {
  match = "^Eigen::Quaternion<",
  summary = function(v)
    local data = v:field("m_coeffs"):field("m_storage"):field("m_data"):field("array"):elements()
    return string.format("w=%s x=%s y=%s z=%s", fmt(data[4]:num()), fmt(data[1]:num()), fmt(data[2]:num()), fmt(data[3]:num()))
  end,
  children = function(v)
    local data = v:field("m_coeffs"):field("m_storage"):field("m_data"):field("array"):elements()
    return {
      { name = "w", value = data[4] }, { name = "x", value = data[1] },
      { name = "y", value = data[2] }, { name = "z", value = data[3] },
    }
  end,
}

-- libstdc++ ----------------------------------------------------------------

visualizer {
  match = "^std::array<",
  children = function(v) return v:field("_M_elems"):elements() end,
}

local function optional_value(v)
  local payload = v:field("_M_payload")
  if payload:field("_M_engaged"):num() == 0 then
    return nil
  end
  return payload:field("_M_payload"):field("_M_value")
end

visualizer {
  match = "^std::optional<",
  summary = function(v)
    local value = optional_value(v)
    return value and value:str() or "nullopt"
  end,
  children = function(v)
    local value = optional_value(v)
    return value and { { name = "value", value = value } } or {}
  end,
}

local function variant_active(v)
  local index = v:field("_M_index"):num()
  if index == 255 or index == 65535 then
    return nil, index
  end
  local union = v:field("_M_u")
  for _ = 1, index do
    union = union:field("_M_rest")
  end
  local storage = union:field("_M_first")
  if storage:has("_M_storage") then
    storage = storage:field("_M_storage")
  end
  return storage, index
end

visualizer {
  match = "^std::variant<",
  summary = function(v)
    local value, index = variant_active(v)
    if not value then
      return "valueless"
    end
    return "index " .. math.floor(index) .. ": " .. value:str()
  end,
  children = function(v)
    local value = variant_active(v)
    return value and { { name = "value", value = value } } or {}
  end,
}

visualizer {
  match = "^std::span<.-, (%d+)>$",
  children = function(v, m)
    local count = tonumber(m[1])
    if count == 4294967295 then
      count = v:field("_M_extent"):field("_M_extent_value"):num()
    end
    local pointer = v:field("_M_ptr")
    local out = {}
    for index = 0, math.min(count, 256) - 1 do
      out[#out + 1] = pointer:at(index)
    end
    return out
  end,
}

visualizer {
  match = "^std::basic_string_view<char",
  summary = function(v)
    local length = v:field("_M_len"):num()
    return '"' .. v:field("_M_str"):cstring(math.min(length, 256)) .. '"'
  end,
  children = function() return {} end,
}

-- etl ----------------------------------------------------------------------

local function etl_vector_children(v)
  local first = v:field("p_buffer")
  local count = (v:field("p_end"):num() - first:num()) / first:pointee_size()
  local out = {}
  for index = 0, math.min(count, 256) - 1 do
    out[#out + 1] = first:at(index)
  end
  return out
end

visualizer { match = "^etl::vector<", children = etl_vector_children }
visualizer { match = "^etl::ivector<", children = etl_vector_children }
visualizer {
  match = "^etl::array<",
  children = function(v) return v:field("_buffer"):elements() end,
}
visualizer {
  match = "^etl::i?string",
  summary = function(v)
    local length = v:field("current_size"):num()
    return '"' .. v:field("p_buffer"):cstring(math.min(length, 256)) .. '"'
  end,
  children = function() return {} end,
}

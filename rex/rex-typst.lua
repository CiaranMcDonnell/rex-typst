-- rex-typst for Rex: a live Typst preview in a pane beside your editor.
--
-- The rex-typst binary does the work: it compiles the document, draws the
-- pages with kitty graphics, follows Neovim's cursor, and jumps the editor to
-- whatever you click. This module only adds the actions and opens its pane.
--
-- In init.lua:
--
--   require("rex-typst").setup{}
--   rex.bind("ctrl+space>t", "typst_toggle")
--
-- setup options:
--   bin        the rex-typst binary on the machine the session runs on
--              (default "rex-typst", found on your login shell's PATH)
--   placement  "split" (beside the editor, default), "float", or "tab"
--   width      the preview's share of the editor's width, for "split"
--   bounds     { x, y, w, h } as fractions of the window, for "float"
--   invert     start with a dark (inverted) preview
--   fit_page   start fitting whole pages instead of the width
--   follow     scroll to the editor's cursor (default true)
--   center     keep the editor's cursor in the middle of the preview
--   focus_editor  focus the editor after a click jumps it (default true)
--   image_mb   megabytes of rendered pages to keep in the pane (default 40)
--   args       extra arguments for rex-typst, such as { "--root", "~/thesis" }

local M = {}

M.options = {
  bin = "rex-typst",
  placement = "split",
  width = 0.45,
  bounds = { 0.5, 0.04, 0.48, 0.92 },
  invert = false,
  fit_page = false,
  follow = true,
  center = false,
  focus_editor = true,
  image_mb = 40,
  args = {},
}

local SHELL = "com.superlogical.terminal.shell"
local TERMINAL = "com.superlogical.terminal"
local LABEL = "rex-typst"

local function fail(message)
  error("rex-typst: " .. message, 0)
end

local function check(result, err)
  if result == nil then fail(tostring(err)) end
  return result
end

-- The preview that belongs with the focused pane: the focused pane itself if
-- it's a preview, else one in the same tab, else any. Also returns the
-- focused block.
local function locate(ctx)
  local list = check(rex.session.list_blocks{})
  local focused
  for _, b in ipairs(list.blocks or {}) do
    if b.block_id == ctx.block_id then focused = b end
  end
  if focused and focused.label == LABEL then return focused, nil end
  local any
  for _, b in ipairs(list.blocks or {}) do
    if b.label == LABEL then
      if focused and b.window_id == focused.window_id then return b, focused end
      any = any or b
    end
  end
  return any, focused
end

-- The floating layer a block is in, or nil.
local function layer_of(block_id)
  local view = check(rex.session.view{})
  for _, w in ipairs(view.windows or {}) do
    for _, layer in ipairs(w.layers or {}) do
      if layer.kind == "floating" then
        for _, b in ipairs(layer.blocks or {}) do
          if b.block_id == block_id then return layer end
        end
      end
    end
  end
end

local function cwd_of(block_id)
  local p = rex.block.call(TERMINAL, "process", { block_id = block_id })
  local fg = p and (p.foreground or p.child)
  return fg and fg.cwd
end

local function press(block_id, keys)
  check(rex.block.call(TERMINAL, "write", { block_id = block_id, data = keys }))
end

local function command(editor_id, args)
  local o = M.options
  local cmd = { o.bin, "--editor-block", editor_id }
  if o.invert then cmd[#cmd + 1] = "--invert" end
  if o.fit_page then cmd[#cmd + 1] = "--fit-page" end
  if not o.follow then cmd[#cmd + 1] = "--no-follow" end
  if o.center then cmd[#cmd + 1] = "--center" end
  if not o.focus_editor then cmd[#cmd + 1] = "--no-focus-editor" end
  if o.image_mb then
    cmd[#cmd + 1] = "--image-mb"
    cmd[#cmd + 1] = tostring(o.image_mb)
  end
  for _, a in ipairs(o.args or {}) do cmd[#cmd + 1] = a end
  if args and args.file then cmd[#cmd + 1] = args.file end
  return cmd
end

function M.open(ctx, args)
  args = args or {}
  local o = M.options
  if not ctx.block_id then fail("focus the editor pane first") end
  local block = {
    flavor = SHELL,
    label = LABEL,
    options = { command = command(ctx.block_id, args), cwd = cwd_of(ctx.block_id) },
  }
  local placement = args.placement or o.placement
  local result, err
  if placement == "split" then
    result, err = rex.session.new_split{
      anchor_id = ctx.block_id,
      direction = "horizontal",
      side = "after",
      ratio = 1 - o.width,
      focus = false,
      layout = { block = block },
    }
  elseif placement == "float" then
    result, err = rex.session.new_layer{ bounds = o.bounds, focus = false, layout = { block = block } }
  elseif placement == "tab" then
    result, err = rex.session.new_window{ layout = { block = block }, focus = false, window_label = "Typst" }
  else
    fail('placement must be "split", "float" or "tab"')
  end
  check(result, err)
  return { block_id = result.block_ids and result.block_ids[1] }
end

-- Close the pane itself rather than pressing q, so a preview that failed to
-- start, and is showing why, closes too.
function M.close(ctx)
  local preview = locate(ctx)
  if not preview then return { closed = false } end
  check(rex.call("block.close", { session_id = ctx.session_id, block_id = preview.block_id }))
  return { closed = true, block_id = preview.block_id }
end

-- Open the preview, or close it. A floating preview is hidden and shown
-- instead, so it keeps its place in the document.
function M.toggle(ctx, args)
  local preview = locate(ctx)
  if not preview then return M.open(ctx, args) end
  local layer = layer_of(preview.block_id)
  if layer then
    local state = layer.state == "minimized" and "normal" or "minimized"
    check(rex.call("session.set_layer_state", {
      session_id = ctx.session_id, layer_id = layer.layer_id, state = state, focus = false,
    }))
    return { block_id = preview.block_id, state = state }
  end
  return M.close(ctx)
end

-- Send one of the preview's keys from wherever you are.
local function key_action(keys)
  return function(ctx)
    local preview = locate(ctx)
    if not preview then fail("no preview is open") end
    press(preview.block_id, keys)
    return { block_id = preview.block_id }
  end
end

function M.setup(opts)
  for k, v in pairs(opts or {}) do
    if M.options[k] == nil then
      rex.log("warn", "rex-typst: ignoring unknown option " .. tostring(k))
    else
      M.options[k] = v
    end
  end

  local function action(name, title, description, run, args)
    rex.action{
      name = name, title = title, description = description, category = "Typst",
      keywords = "typst preview pdf document", args = args, run = run,
    }
  end

  action("typst_toggle", "Typst: Toggle Preview",
    "Open a live preview of the Typst file in the focused editor pane, or close it.",
    function(ctx, args) return M.toggle(ctx, args) end, { file = "string?", placement = "string?" })
  action("typst_open", "Typst: Open Preview",
    "Open a live preview beside the focused pane. Defaults to the file open in it, else main.typ.",
    function(ctx, args) return M.open(ctx, args) end, { file = "string?", placement = "string?" })
  action("typst_close", "Typst: Close Preview", "Close the preview.",
    function(ctx) return M.close(ctx) end)
  action("typst_next_page", "Typst: Next Page", "Scroll the preview to the next page.", key_action("n"))
  action("typst_prev_page", "Typst: Previous Page", "Scroll the preview to the previous page.", key_action("p"))
  action("typst_fit", "Typst: Toggle Fit Width / Page", "Fit the page width, or whole pages.", key_action("w"))
  action("typst_dark", "Typst: Toggle Dark Preview", "Invert the preview's colors.", key_action("i"))
  action("typst_follow", "Typst: Toggle Follow Cursor", "Scroll the preview with the editor's cursor, or stop.", key_action("f"))
  action("typst_center", "Typst: Toggle Center Cursor", "Keep the editor's cursor in the middle of the preview, or stop.", key_action("c"))
  action("typst_zoom_in", "Typst: Zoom In", "Zoom the preview in.", key_action("+"))
  action("typst_zoom_out", "Typst: Zoom Out", "Zoom the preview out.", key_action("-"))
  action("typst_goto_error", "Typst: Go to Error",
    "Jump the editor to the next compile error, or warning when there are none.", key_action("e"))
  action("typst_zoom_reset", "Typst: Reset Zoom", "Zoom the preview back to the fit.", key_action("0"))
  return M
end

return M

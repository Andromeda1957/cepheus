local deck = require("deck")
local function launch_arguments(operation)
  -- Management must validate the registration before using any of its words.
  -- The helper preserves the registered chat interpreter and state directory.
  return {deck.exe, "dot", operation, "--config-dir", deck.config_dir}
end

deck.command("dot", function(args)
  local operation = args[1]
  local active = nil
  for _, session in ipairs((deck.state({"sessions"}) or {}).sessions or {}) do
    if session.tool == "dot" and session.alive and session.host == nil then active = session end
  end
  if operation == nil or operation == "on" then
    if active then deck.select(active.id) else deck.cmd("new dot") end
    return
  end
  if operation == "off" then
    if active then
      deck.select(active.id)
      deck.cmd("close")
    end
    deck.notify("Dot bridge off. Dot's cloud work continues in ChatGPT.")
    return
  end
  if operation ~= "setup" and operation ~= "probe" and operation ~= "login" then
    deck.notify("Use :dot [on|off|setup URL|probe|login]", {error = true})
    return
  end
  if active then
    deck.select(active.id)
    deck.notify("In Dot, enter /quit first, then run :dot " .. operation .. ". Reopen Dot afterward.")
    return
  end
  local cmd = launch_arguments(operation)
  if operation == "setup" and args[2] then
    cmd[#cmd + 1] = "--url"
    cmd[#cmd + 1] = args[2]
    if args[3] then
      cmd[#cmd + 1] = "--native-host"
      cmd[#cmd + 1] = args[3]
    end
  end
  deck.term({name = "Dot " .. operation, cmd = cmd})
end, {desc = "Connect to your existing cloud Dot", complete = {"on", "off", "setup", "probe", "login"}})

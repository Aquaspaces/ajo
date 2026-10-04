-- Run from the repository root with Lune 0.10.4:
-- lune run scripts/test-studio-controls.luau
-- These are the actual TestEZ specs with injected services and a fake socket.
-- They do not run Roblox Studio. Promise, type checks, and MessagePack are real.
local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")
local task = require("@lune/task")

local function load(path, environment)
	return luau.load(fs.readFile(path), { debugName = path, environment = environment })()
end

local state = { Connecting = "Connecting", Open = "Open", Closed = "Closed", Error = "Error" }
local enum = { WebStreamClientState = state, WebStreamClientType = { WebSocket = "WebSocket" } }
local gameServices = {
	HttpService = {},
	RunService = { Heartbeat = {
		Wait = function()
			task.wait()
		end,
	} },
}
local game = {
	GetService = function(_, name)
		return assert(gameServices[name], "Unexpected service: " .. name)
	end,
}
local msgpack = load("plugin/Packages/msgpack-luau/src/msgpack.lua")
local modules = {
	StudioControls = load("plugin/src/StudioControls.lua"),
	Promise = load("plugin/Packages/Promise/lib/init.lua", { game = game, task = task }),
	t = load("plugin/Packages/t/lib/init.lua"),
	strict = load("plugin/src/strict.lua"),
	Settings = {
		get = function()
			return false
		end,
	},
	Config = {},
	Version = {},
	Log = { trace = function() end, warn = function() end, error = function() end },
	Http = {
		jsonEncode = function(value)
			return serde.encode("json", value)
		end,
		jsonDecode = function(value)
			return serde.decode("json", value)
		end,
		msgpackEncode = msgpack.encode,
		msgpackDecode = msgpack.decode,
	},
}
local function requireModule(reference)
	return assert(modules[reference], "Unexpected module: " .. tostring(reference))
end
local packages = { Http = "Http", Log = "Log", Promise = "Promise", t = "t" }
local parent = { Parent = { Packages = packages } }
for _, name in ipairs({ "StudioControls", "ApiContext", "Types", "Config", "Version", "Settings", "strict" }) do
	parent[name] = name
end
local environment = {
	script = { Parent = parent },
	require = requireModule,
	game = game,
	Enum = enum,
	task = task,
}
modules.Types = load("plugin/src/Types.lua", environment)
modules.ApiContext = load("plugin/src/ApiContext.lua", environment)

local passed = 0
local failed = 0

local specEnvironment = table.clone(environment)
specEnvironment.it = function(name, callback)
	local success, err = xpcall(callback, debug.traceback)
	if success then
		passed += 1
		print("PASS " .. name)
	else
		failed += 1
		warn("FAIL " .. name .. "\n" .. tostring(err))
	end
end
specEnvironment.expect = function(actual)
	return {
		to = {
			equal = function(expected)
				assert(actual == expected, string.format("Expected %s, got %s", tostring(expected), tostring(actual)))
			end,
		},
	}
end

for _, name in ipairs({ "StudioControls", "ApiContext" }) do
	load("plugin/src/" .. name .. ".spec.lua", specEnvironment)()
end
print(string.format("Mocked Studio controls and transport suite: %d passed, %d failed", passed, failed))
assert(failed == 0, "Studio controls tests failed")

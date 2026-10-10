-- Run with: lune run scripts/test-plugin-controller.luau
-- Executes the actual controller specs and Settings validation with no Studio UI.
local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")
local function load(path, environment)
	return luau.load(fs.readFile(path), { debugName = path, environment = environment })()
end

local modules = { Log = { trace = function() end, warn = function() end }, Roact = {} }
local function requireModule(name)
	return assert(modules[name], "Unexpected module: " .. tostring(name))
end
local environment = {
	script = {
		Parent = { Settings = "Settings", PluginController = "PluginController", activateAction = "activateAction" },
		FindFirstAncestorWhichIsA = function()
			return nil
		end,
		FindFirstAncestor = function()
			return { Packages = { Log = "Log", Roact = "Roact" } }
		end,
	},
	require = requireModule,
	game = {
		GetService = function(_, name)
			assert(name == "HttpService")
			return {
				JSONEncode = function(_, value)
					return serde.encode("json", value)
				end,
			}
		end,
	},
}
modules.Settings = load("plugin/src/Settings.lua", environment)
modules.PluginController = load("plugin/src/PluginController.lua", environment)
modules.activateAction = load("plugin/src/App/Components/Notifications/activateAction.lua", environment)

local passed, failed = 0, 0
environment.it = function(name, callback)
	local ok, err = xpcall(callback, debug.traceback)
	if ok then
		passed += 1
		print("PASS " .. name)
	else
		failed += 1
		warn("FAIL " .. name .. "\n" .. tostring(err))
	end
end
environment.expect = function(actual)
	return {
		to = {
			equal = function(expected)
				assert(actual == expected, string.format("Expected %s, got %s", tostring(expected), tostring(actual)))
			end,
		},
	}
end
load("plugin/src/PluginController.spec.lua", environment)()
load("plugin/src/App/Components/Notifications/activateAction.spec.lua", environment)()

-- Load the real App methods with a small Roact/service fixture. Session networking
-- is mocked here; the lifecycle closures, confirmation waits, and UI actions are real.
local references = {}
local function reference(name)
	if references[name] then
		return references[name]
	end
	local node = setmetatable({ name = name }, {
		__index = function(_, key)
			if key == "FindFirstAncestor" then
				return function()
					return reference("Rojo")
				end
			end
			return reference(key)
		end,
	})
	references[name] = node
	return node
end
local none = {}
local component = {}
function component:extend()
	local class = {}
	class.__index = class
	return setmetatable(class, { __index = self })
end
function component:setState(update)
	if type(update) == "function" then
		update = update(self.state)
	end
	for key, value in update do
		self.state[key] = if value == none then nil else value
	end
end
local roact = {
	Component = component,
	None = none,
	Event = { Activated = "Activated" },
	createElement = function(kind, props, children)
		return { kind = kind, props = props, children = children }
	end,
	createFragment = function(children)
		return { children = children }
	end,
}
local services = {
	Players = {
		GetPlayers = function()
			return {}
		end,
	},
	ServerStorage = {
		FindFirstChild = function()
			return nil
		end,
	},
	RunService = {
		IsRunning = function()
			return false
		end,
		IsEdit = function()
			return false
		end,
	},
	Selection = {},
	StudioService = {},
}
local appGame = {
	PlaceId = 123,
	GetService = function(_, name)
		return services[name] or {}
	end,
}
local patchSet = {
	newEmpty = function()
		return { added = {}, removed = {}, updated = {} }
	end,
	isEmpty = function(patch)
		return next(patch.added) == nil and #patch.removed == 0 and #patch.updated == 0
	end,
	hasAdditions = function(patch)
		return next(patch.added) ~= nil
	end,
	hasRemoves = function(patch)
		return #patch.removed > 0
	end,
	containsOnlyInstance = function()
		return false
	end,
}
local patchTree = {
	build = function()
		return { idToNode = {} }
	end,
	updateMetadata = function(tree)
		return tree
	end,
}
local sessions = {
	Status = { Connecting = "Connecting", Connected = "Connected", Disconnected = "Disconnected" },
	new = function(options)
		local session = { __twoWaySync = options.twoWaySync, __studioControls = { __syncDepth = 0 } }
		function session:setUpdateLoadingTextCallback(callback)
			self.loading = callback
		end
		function session:hookPrecommit(callback)
			self.precommit = callback
			return function() end
		end
		function session:hookPostcommit(callback)
			self.postcommit = callback
			return function() end
		end
		function session:onStatusChanged(callback)
			self.changed = callback
		end
		function session:setConfirmCallback(callback)
			self.confirm = callback
		end
		function session:start()
			self.status = "Connecting"
			self.changed(self.status)
		end
		function session:stop()
			self.status = "Disconnected"
			self.changed(self.status)
		end
		function session:getStatus()
			return self.status
		end
		return session
	end,
}
local appSettings = setmetatable({ _values = table.clone(modules.Settings._values) }, { __index = modules.Settings })
appSettings._values.confirmationBehavior = "Always"
appSettings._values.showNotifications = true
local appModules = {
	Roact = roact,
	Settings = appSettings,
	strict = function(_, values)
		return values
	end,
	Log = { trace = function() end, warn = function() end },
	Config = { defaultHost = "localhost", defaultPort = "34872" },
	Dictionary = {
		merge = function(...)
			local result = {}
			for _, values in { ... } do
				for key, value in values do
					result[key] = value
				end
			end
			return result
		end,
	},
	ServeSession = sessions,
	ApiContext = {
		new = function(url)
			return { __baseUrl = url }
		end,
	},
	PatchSet = patchSet,
	PatchTree = patchTree,
	Assets = { Images = {}, Sounds = {} },
	soundPlayer = {
		new = function()
			return {}
		end,
	},
	PluginController = modules.PluginController,
	getTextBoundsAsync = function()
		return { X = 100, Y = 20 }
	end,
	activateAction = modules.activateAction,
}
local bindableEvents = {}
local appEnvironment = {
	DateTime = {
		now = function()
			return { UnixTimestamp = 42 }
		end,
	},
	script = reference("App"),
	game = appGame,
	require = function(ref)
		return appModules[ref.name] or {}
	end,
	Instance = {
		new = function(kind)
			assert(kind == "BindableEvent")
			local waiters = {}
			local event = { Event = {} }
			function event.Event:Wait()
				table.insert(waiters, coroutine.running())
				coroutine.yield()
			end
			function event:Fire()
				local pending = waiters
				waiters = {}
				for _, thread in pending do
					assert(coroutine.resume(thread))
				end
			end
			function event:Destroy()
				self.destroyed = true
			end
			table.insert(bindableEvents, event)
			return event
		end,
	},
}
local appElement = load("plugin/src/App/init.lua", appEnvironment)({})
local App = appElement.kind
local function appFixture()
	local app = setmetatable({
		state = {
			appStatus = "NotConnected",
			notifications = {},
			patchData = { patch = patchSet.newEmpty(), unapplied = patchSet.newEmpty(), timestamp = 1 },
			confirmData = {},
		},
		confirmationSequence = 0,
		diffRevision = 0,
		diffUpdating = 0,
		notifId = 0,
		knownProjects = {},
		host = {
			getValue = function()
				return "localhost"
			end,
		},
		port = {
			getValue = function()
				return "34872"
			end,
		},
	}, App)
	return app
end
local changedPatch = { added = { first = {} }, removed = {}, updated = {} }
local serverInfo = { projectName = "Example" }

environment.it(
	"actual App rejects stale and duplicate confirmations and cancels a pending wait on disconnect",
	function()
		local app = appFixture()
		app:startSession()
		local firstSession = app.serveSession
		local decision
		local waiter = coroutine.create(function()
			decision = firstSession.confirm({}, changedPatch, serverInfo)
		end)
		assert(coroutine.resume(waiter))
		local firstId = app.pendingConfirmation.id
		local firstRevision = app.diffRevision
		assert(firstRevision > 0)
		assert(not pcall(function()
			app:respondConfirmation("stale", "Accept")
		end))
		assert(not pcall(function()
			app:respondConfirmation(firstId, "Reject")
		end))
		app:endSession()
		assert(app.diffRevision > firstRevision)
		assert(decision == "Abort" and coroutine.status(waiter) == "dead")
		assert(bindableEvents[#bindableEvents].destroyed)
		app:startSession()
		local currentSession = app.serveSession
		local secondWaiter = coroutine.create(function()
			decision = currentSession.confirm({}, changedPatch, serverInfo)
		end)
		assert(coroutine.resume(secondWaiter))
		local secondId = app.pendingConfirmation.id
		assert(secondId ~= firstId)
		assert(not pcall(function()
			app:respondConfirmation(firstId, "Accept")
		end))
		firstSession.changed("Disconnected", "Late error")
		firstSession.loading("Stale progress")
		firstSession.precommit(changedPatch, {})
		firstSession.postcommit(changedPatch, {}, patchSet.newEmpty())
		assert(app.serveSession == currentSession and app.pendingConfirmation.id == secondId)
		assert(app.state.appStatus == "Confirming")
		app:respondConfirmation(secondId, "Accept")
		assert(decision == "Accept" and coroutine.status(secondWaiter) == "dead")
		assert(not pcall(function()
			app:respondConfirmation(secondId, "Accept")
		end))
	end
)

environment.it("actual App preserves a confirmation resolved before its coroutine starts waiting", function()
	local app = appFixture()
	app:startSession()
	app.addNotification = function(self)
		if self.pendingConfirmation then
			self:respondConfirmation(self.pendingConfirmation.id, "Accept")
		end
	end
	assert(app.serveSession.confirm({}, changedPatch, serverInfo) == "Accept")
	assert(app.pendingConfirmation == nil and bindableEvents[#bindableEvents].destroyed)
end)

environment.it(
	"actual App revisions cover precommit and yielding metadata updates without restoring an older tree",
	function()
		local app = appFixture()
		app:startSession()
		local session = app.serveSession
		session.precommit(changedPatch, {})
		local precommitRevision = app.diffRevision
		assert(precommitRevision > 0)
		local oldTree = app.state.patchTree
		local originalUpdate = patchTree.updateMetadata
		patchTree.updateMetadata = function(tree)
			coroutine.yield()
			tree.updated = true
			return tree
		end
		local worker = coroutine.create(function()
			session.postcommit(changedPatch, {}, patchSet.newEmpty())
		end)
		assert(coroutine.resume(worker))
		assert(app.diffRevision > precommitRevision and app.diffUpdating == 1)
		local controller = modules.PluginController.new(app, appSettings)
		assert(not pcall(function()
			controller:getChanges()
		end))
		assert(not pcall(function()
			controller:getDiff("a", "Source", "old")
		end))
		session.precommit(changedPatch, {})
		local latestTree = app.state.patchTree
		assert(latestTree ~= oldTree)
		local duringRevision = app.diffRevision
		assert(coroutine.resume(worker))
		patchTree.updateMetadata = originalUpdate
		assert(app.diffUpdating == 0 and app.diffRevision > duringRevision)
		assert(app.state.patchTree == latestTree)
		local beforeCompleted = app.diffRevision
		session.postcommit(changedPatch, {}, patchSet.newEmpty())
		assert(app.diffRevision == beforeCompleted + 2)
		assert(controller:getChanges().revision == app.diffRevision)
	end
)

environment.it("actual App ignores a stale confirmation whose diff build yielded across reconnect", function()
	local app = appFixture()
	app:startSession()
	local oldSession = app.serveSession
	local build = patchTree.build
	patchTree.build = function()
		coroutine.yield()
		return { stale = true }
	end
	local decision
	local waiter = coroutine.create(function()
		decision = oldSession.confirm({}, changedPatch, serverInfo)
	end)
	assert(coroutine.resume(waiter))
	app:endSession()
	app:startSession()
	assert(coroutine.resume(waiter))
	patchTree.build = build
	assert(decision == "Abort" and app.pendingConfirmation == nil and app.state.appStatus == "Connecting")
end)

environment.it("actual App refuses lifecycle and notification mutations during a yielding sync", function()
	local app = appFixture()
	app:startSession()
	local current = app.serveSession
	current.__studioControls.__syncDepth = 1
	assert(not pcall(function()
		app:endSession()
	end))
	local called = false
	app:addNotification({
		text = "Connect",
		actions = { Connect = {
			onClick = function()
				called = true
			end,
		} },
	})
	assert(not pcall(function()
		app:notificationAction(app.notifId, "Connect")
	end))
	assert(app.serveSession == current and not called)
end)

-- Render each real notification component and invoke its rendered button.
local uiValue = setmetatable({}, {
	__index = function()
		return {}
	end,
})
appEnvironment.Enum = setmetatable({}, {
	__index = function()
		return uiValue
	end,
})
appEnvironment.UDim2 = {
	new = function()
		return {}
	end,
	fromOffset = function()
		return {}
	end,
	fromScale = function()
		return {}
	end,
}
appEnvironment.UDim = {
	new = function()
		return {}
	end,
}
appEnvironment.Vector2 = {
	new = function(x, y)
		return { X = x, Y = y }
	end,
}
appModules.Theme = {
	with = function(callback)
		return callback({
			BackgroundColor = {},
			Font = { Main = {} },
			TextSize = { Body = 14, Large = 18 },
			Notification = { InfoColor = {} },
			Header = { LogoColor = {} },
		})
	end,
}
local function findButton(tree)
	if tree.props and tree.props.onClick then
		return tree.props.onClick
	end
	for _, child in tree.children or {} do
		local found = findButton(child)
		if found then
			return found
		end
	end
	return nil
end
for _, filename in { "Notification", "FullscreenNotification" } do
	environment.it("rendered " .. filename .. " button invokes the actual App action and cleanup once", function()
		local app = appFixture()
		local calls, closes = 0, 0
		app:addNotification({
			text = "Connect",
			actions = {
				Connect = {
					text = "Connect",
					layoutOrder = 1,
					onClick = function()
						calls += 1
					end,
				},
			},
			onClose = function()
				closes += 1
			end,
		})
		local id = app.notifId
		local wrapped = table.clone(app.state.notifications[id].actions.Connect)
		wrapped.handlesDismissal = true
		wrapped.onClick = function()
			app:notificationAction(id, "Connect")
		end
		local Widget = load("plugin/src/App/Components/Notifications/" .. filename .. ".lua", appEnvironment)
		local widget = setmetatable({
			props = {
				text = "Connect",
				layoutOrder = 1,
				actions = { Connect = wrapped },
				onClose = function()
					app:closeNotification(id)
				end,
			},
			binding = {
				map = function(_, callback)
					return callback(1)
				end,
			},
			transparency = 0,
		}, Widget)
		local click = assert(findButton(widget:render()))
		click()
		assert(calls == 1 and closes == 1 and app.state.notifications[id] == nil)
		widget.props.onClose()
		assert(closes == 1)
	end)
end
print(string.format("Plugin controller suite: %d passed, %d failed", passed, failed))
assert(failed == 0, "Plugin controller tests failed")

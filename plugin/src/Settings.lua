--[[
	Persistent plugin settings.
]]

local plugin = plugin or script:FindFirstAncestorWhichIsA("Plugin")
local Rojo = script:FindFirstAncestor("Rojo")
local Packages = Rojo.Packages

local Log = require(Packages.Log)
local Roact = require(Packages.Roact)

local defaultSettings = {
	openScriptsExternally = false,
	twoWaySync = false,
	autoReconnect = false,
	showNotifications = true,
	enableSyncFallback = true,
	syncReminderMode = "Notify" :: "None" | "Notify" | "Fullscreen",
	syncReminderPolling = true,
	checkForUpdates = true,
	checkForPrereleases = false,
	autoConnectPlaytestServer = false,
	confirmationBehavior = "Initial" :: "Never" | "Initial" | "Always" | "Large Changes" | "Unlisted PlaceId",
	largeChangesConfirmationThreshold = 5,
	playSounds = true,
	typecheckingEnabled = false,
	logLevel = "Info",
	timingLogsEnabled = false,
	priorEndpoints = {},
}

local Settings = {}

local settingOptions = {
	syncReminderMode = { "None", "Notify", "Fullscreen" },
	confirmationBehavior = { "Initial", "Always", "Large Changes", "Unlisted PlaceId", "Never" },
	logLevel = { "Error", "Warning", "Info", "Debug", "Trace" },
}

function Settings:getPublicSchema()
	local schema = {}
	for name, value in defaultSettings do
		if name ~= "priorEndpoints" then
			schema[name] = {
				type = type(value),
				default = value,
				options = if settingOptions[name] then table.clone(settingOptions[name]) else nil,
				minimum = if name == "largeChangesConfirmationThreshold" then 1 else nil,
				maximum = if name == "largeChangesConfirmationThreshold" then 999 else nil,
				lockedWhileSyncing = name == "twoWaySync",
			}
		end
	end
	return schema
end

function Settings:getPublicValues()
	local values = {}
	for name in self:getPublicSchema() do
		values[name] = self:get(name)
	end
	return values
end

function Settings:setPublicValues(values, syncActive)
	assert(type(values) == "table", "settings must be an object")
	-- Validate the whole request before persisting any field.
	for name, value in values do
		assert(
			type(name) == "string" and name ~= "priorEndpoints" and defaultSettings[name] ~= nil,
			"Unknown plugin setting: " .. tostring(name)
		)
		assert(type(value) == type(defaultSettings[name]), "Invalid type for setting " .. name)
		if settingOptions[name] then
			assert(table.find(settingOptions[name], value) ~= nil, "Invalid value for setting " .. name)
		elseif name == "largeChangesConfirmationThreshold" then
			assert(
				value >= 1 and value <= 999 and value % 1 == 0,
				"Confirmation threshold must be an integer from 1 to 999"
			)
		end
		assert(
			not (name == "twoWaySync" and syncActive and value ~= self:get(name)),
			"Cannot change twoWaySync while syncing. Disconnect first."
		)
	end
	for name, value in values do
		self:set(name, value)
	end
end

Settings._values = table.clone(defaultSettings)
Settings._updateListeners = {}
Settings._bindings = {}

if plugin then
	for name, defaultValue in pairs(Settings._values) do
		local savedValue = plugin:GetSetting("Rojo_" .. name)

		if savedValue == nil then
			-- plugin:SetSetting hits disc instead of memory, so it can be slow. Spawn so we don't hang.
			task.spawn(plugin.SetSetting, plugin, "Rojo_" .. name, defaultValue)
			Settings._values[name] = defaultValue
		else
			Settings._values[name] = savedValue
		end
	end
	Log.trace("Loaded settings from plugin store")
end

function Settings:get(name)
	if defaultSettings[name] == nil then
		error("Invalid setings name " .. tostring(name), 2)
	end

	return self._values[name]
end

function Settings:set(name, value)
	self._values[name] = value
	if self._bindings[name] then
		self._bindings[name].set(value)
	end

	if plugin then
		-- plugin:SetSetting hits disc instead of memory, so it can be slow. Spawn so we don't hang.
		task.spawn(plugin.SetSetting, plugin, "Rojo_" .. name, value)
	end

	if self._updateListeners[name] then
		for callback in pairs(self._updateListeners[name]) do
			task.spawn(callback, value)
		end
	end

	Log.trace(string.format("Set setting '%s' to '%s'", name, tostring(value)))
end

function Settings:onChanged(name, callback)
	local listeners = self._updateListeners[name]
	if listeners == nil then
		listeners = {}
		self._updateListeners[name] = listeners
	end
	listeners[callback] = true

	Log.trace(string.format("Added listener for setting '%s' changes", name))

	return function()
		listeners[callback] = nil
		Log.trace(string.format("Removed listener for setting '%s' changes", name))
	end
end

function Settings:getBinding(name)
	local cached = self._bindings[name]
	if cached then
		return cached.bind
	end

	local bind, set = Roact.createBinding(self._values[name])
	self._bindings[name] = {
		bind = bind,
		set = set,
	}

	Log.trace(string.format("Created binding for setting '%s'", name))

	return bind
end

function Settings:getBindings(...: string)
	local bindings = {}
	for i = 1, select("#", ...) do
		local source = select(i, ...)
		bindings[source] = self:getBinding(source)
	end

	return Roact.joinBindings(bindings)
end

return Settings

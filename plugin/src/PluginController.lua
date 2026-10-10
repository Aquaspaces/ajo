-- The UI and the agent connection share App's actions and session lifecycle.
local Settings = require(script.Parent.Settings)
local HttpService = game:GetService("HttpService")

local PluginController = {}
PluginController.__index = PluginController

local ACTION_FIELDS = {
	connect = { host = true, port = true },
	disconnect = {},
	reconnect = {},
	setSettings = { settings = true },
	respondConfirmation = { confirmationId = true, decision = true },
	setWindow = { enabled = true },
	openSettings = {},
	closeSettings = {},
	dismissError = {},
	forgetProject = {},
	notification = { id = true, action = true },
	checkUpdates = {},
	focusChange = { id = true },
}

local function integer(value, minimum, maximum)
	return type(value) == "number" and value % 1 == 0 and value >= minimum and value <= maximum
end

local function boundedString(value, budget)
	local maximum = math.min(budget.stringLimit or 4096, budget.characters)
	if #value > maximum then
		budget.truncated = true
		local stop = maximum
		-- Do not split a UTF-8 codepoint before JSON encoding.
		while stop > 0 and utf8.len(value:sub(1, stop)) == nil do
			stop -= 1
		end
		value = value:sub(1, stop)
	end
	budget.characters -= #value
	return value
end

local function safeValue(value, budget, visited, depth)
	if budget.values <= 0 or depth > 8 then
		budget.truncated = true
		return "[truncated]"
	end
	budget.values -= 1
	local kind = typeof(value)
	if kind == "nil" then
		return { type = "nil" }
	elseif kind == "boolean" then
		return value
	elseif kind == "number" then
		return if value == value and math.abs(value) ~= math.huge then value else tostring(value)
	elseif kind == "string" then
		return boundedString(value, budget)
	elseif kind == "table" then
		if visited[value] then
			return "[cycle]"
		end
		visited[value] = true
		local result = {}
		local count = 0
		for key, item in value do
			if count >= (budget.entryLimit or 128) or budget.values <= 0 then
				budget.truncated = true
				break
			end
			if type(key) == "string" or type(key) == "number" then
				result[boundedString(tostring(key), budget)] = safeValue(item, budget, visited, depth + 1)
				count += 1
			end
		end
		visited[value] = nil
		return result
	elseif kind == "Instance" then
		return { type = kind, name = boundedString(value.Name, budget), className = value.ClassName }
	end
	return { type = kind, value = boundedString(tostring(value), budget) }
end

local function patchCounts(patch)
	local counts = { added = 0, removed = 0, updated = 0 }
	if patch then
		for key in counts do
			for _ in patch[key] or {} do
				counts[key] += 1
			end
		end
	end
	counts.total = counts.added + counts.removed + counts.updated
	return counts
end

function PluginController.new(app, settings)
	return setmetatable({ app = app, settings = settings or Settings }, PluginController)
end

function PluginController:getState()
	local app = self.app
	local host, port = app:getHostAndPort()
	local budget = { characters = 32768, values = 4096, truncated = false }
	local notifications = {}
	local ids = {}
	for id in app.state.notifications do
		table.insert(ids, id)
	end
	table.sort(ids)
	for _, id in ids do
		if #notifications >= 64 then
			budget.truncated = true
			break
		end
		local notification = app.state.notifications[id]
		local actions = {}
		for name in notification.actions or {} do
			table.insert(actions, name)
		end
		table.sort(actions)
		table.insert(notifications, {
			id = id,
			text = boundedString(notification.text, budget),
			actions = actions,
			isFullscreen = notification.isFullscreen,
		})
	end
	local confirmation = app.pendingConfirmation
	local confirmationState
	if confirmation then
		confirmationState = {
			id = confirmation.id,
			projectName = confirmation.serverInfo.projectName,
			counts = patchCounts(confirmation.patch),
			decisions = if confirmation.twoWaySync then { "Accept", "Reject", "Abort" } else { "Accept", "Abort" },
		}
	end
	local capabilities = {}
	for name in ACTION_FIELDS do
		table.insert(capabilities, name)
	end
	table.sort(capabilities)
	return {
		placeId = game.PlaceId,
		gameId = game.GameId,
		placeName = game.Name,
		appStatus = app.state.appStatus,
		sessionStatus = if app.serveSession then app.serveSession:getStatus() else "Disconnected",
		host = host,
		port = tonumber(port),
		windowEnabled = app.state.guiEnabled,
		projectName = app.state.projectName,
		address = app.state.address,
		connectingText = app.state.connectingText,
		errorMessage = if app.state.appStatus == "Error"
			then boundedString(app.state.errorMessage or "", budget)
			else nil,
		confirmation = confirmationState,
		settings = self.settings:getPublicValues(),
		settingsSchema = self.settings:getPublicSchema(),
		notifications = notifications,
		capabilities = capabilities,
		commands = { "getPluginState", "getPluginChanges", "getPluginDiff", "pluginAction" },
		diffRevision = app.diffRevision or 0,
		truncated = budget.truncated,
	}
end

function PluginController:getDiff(id, property, side, offset, limit, revision)
	assert(type(id) == "string" and type(property) == "string", "Diff requires an instance id and property")
	assert(side == "old" or side == "new", "side must be old or new")
	offset = offset or 0
	limit = limit or 8192
	assert(integer(offset, 0, 8 * 1024 * 1024), "offset must be a non-negative byte offset")
	assert(integer(limit, 1, 16384), "limit must be an integer from 1 to 16384")
	assert(revision == nil or integer(revision, 0, 9007199254740991), "revision must be a non-negative safe integer")
	assert(
		offset == 0 or revision ~= nil,
		"Continuation requests require the revision returned by the first diff chunk"
	)
	local currentRevision = self.app.diffRevision or 0
	assert(
		revision == nil or revision == currentRevision,
		"Diff revision changed; restart the property read at offset 0"
	)
	assert((self.app.diffUpdating or 0) == 0, "Diff metadata is updating; retry the property read after it finishes")
	local tree = self.app.state.patchTree
	local node = tree and tree.idToNode[id]
	assert(node ~= nil, "Changed instance is no longer available in the current diff")
	local row
	for index, candidate in node.changeList or {} do
		if index > 1 and candidate[1] == property then
			row = candidate
			break
		end
	end
	assert(row ~= nil, "Property is not present in the current diff")
	local value = row[if side == "old" then 2 else 3]
	local encoding = "text"
	if type(value) ~= "string" then
		encoding = "json"
		local budget = {
			characters = 4 * 1024 * 1024,
			stringLimit = 4 * 1024 * 1024,
			values = 50000,
			entryLimit = 50000,
			truncated = false,
		}
		local converted = safeValue(value, budget, {}, 0)
		assert(not budget.truncated, "Property exceeds the diff value limits")
		value = HttpService:JSONEncode(converted)
	end
	assert(#value <= 8 * 1024 * 1024, "Property exceeds the 8 MiB diff limit")
	assert(utf8.len(value) ~= nil, "Property contains invalid UTF-8")
	assert(offset <= #value, "offset exceeds the current property length; refresh the diff")
	local function continuation(index)
		local byte = string.byte(value, index)
		return byte ~= nil and byte >= 128 and byte < 192
	end
	assert(not continuation(offset + 1), "offset must be a UTF-8 character boundary")
	local stop = math.min(offset + limit, #value)
	while stop > offset and continuation(stop + 1) do
		stop -= 1
	end
	assert(stop > offset or offset == #value, "limit is too small for the next UTF-8 character")
	return {
		revision = currentRevision,
		id = id,
		property = property,
		side = side,
		encoding = encoding,
		value = value:sub(offset + 1, stop),
		totalBytes = #value,
		offset = offset,
		nextOffset = if stop < #value then stop else nil,
		confirmationId = if self.app.pendingConfirmation then self.app.pendingConfirmation.id else nil,
		timestamp = self.app.state.patchData and self.app.state.patchData.timestamp,
	}
end

function PluginController:getChanges(offset, limit)
	offset = offset or 0
	limit = limit or 50
	assert(integer(offset, 0, 2147483647), "offset must be a non-negative integer")
	assert(integer(limit, 1, 100), "limit must be an integer from 1 to 100")
	local app = self.app
	assert((app.diffUpdating or 0) == 0, "Diff metadata is updating; retry after it finishes")
	local tree = app.state.patchTree
	local nodes = {}
	if tree then
		for id, node in tree.idToNode do
			if id ~= "ROOT" then
				table.insert(nodes, node)
			end
		end
	end
	table.sort(nodes, function(a, b)
		return a.id < b.id
	end)
	local changes = {}
	local budget = { characters = 49152, values = 4096, truncated = false }
	for index = offset + 1, math.min(offset + limit, #nodes) do
		local node = nodes[index]
		local properties = {}
		for rowIndex, row in node.changeList or {} do
			if rowIndex == 1 then
				continue
			end
			if #properties >= 128 or budget.values <= 0 then
				budget.truncated = true
				break
			end
			table.insert(properties, {
				property = boundedString(tostring(row[1]), budget),
				old = safeValue(row[2], budget, {}, 0),
				new = safeValue(row[3], budget, {}, 0),
				unapplied = row[4] ~= nil and row[4].isWarning == true,
			})
		end
		table.insert(changes, {
			id = node.id,
			parentId = node.parentId,
			name = boundedString(node.name or "", budget),
			className = node.className,
			changeType = node.patchType,
			unapplied = node.isWarning == true,
			properties = properties,
		})
	end
	local pending = app.pendingConfirmation
	local data = app.state.patchData or {}
	return {
		revision = app.diffRevision or 0,
		confirmationId = if pending then pending.id else nil,
		timestamp = data.timestamp,
		counts = patchCounts(if pending then pending.patch else data.patch),
		unapplied = patchCounts(if pending then nil else data.unapplied),
		changes = changes,
		total = #nodes,
		offset = offset,
		nextOffset = if offset + #changes < #nodes then offset + #changes else nil,
		truncated = budget.truncated,
	}
end

function PluginController:action(action)
	assert(type(action) == "table" and type(action.type) == "string", "action requires a type")
	local fields = ACTION_FIELDS[action.type]
	assert(fields, "Unsupported plugin action: " .. action.type)
	for key in action do
		assert(key == "type" or fields[key], "Unexpected plugin action field: " .. tostring(key))
	end
	local app = self.app
	local kind = action.type
	if kind == "connect" or kind == "reconnect" then
		if action.host ~= nil then
			assert(
				type(action.host) == "string" and #action.host <= 255 and not action.host:find("[%c%s]"),
				"host must be a hostname or an HTTP(S) host"
			)
		end
		assert(action.port == nil or integer(action.port, 1, 65535), "port must be an integer from 1 to 65535")
		if kind == "connect" then
			assert(app.serveSession == nil, "A sync session is already active")
		else
			app:endSession()
		end
		if action.host ~= nil then
			app.setHost(action.host)
		end
		if action.port ~= nil then
			app.setPort(tostring(action.port))
		end
		app:startSession()
	elseif kind == "disconnect" then
		app:endSession()
	elseif kind == "setSettings" then
		self.settings:setPublicValues(action.settings, app.serveSession ~= nil)
	elseif kind == "respondConfirmation" then
		app:respondConfirmation(action.confirmationId, action.decision)
	elseif kind == "setWindow" then
		assert(type(action.enabled) == "boolean", "enabled must be a boolean")
		app:setWindow(action.enabled)
	elseif kind == "openSettings" then
		app:openSettings()
	elseif kind == "closeSettings" then
		app:closeSettings()
	elseif kind == "dismissError" then
		app:dismissError()
	elseif kind == "forgetProject" then
		app:forgetPriorSyncInfo()
	elseif kind == "notification" then
		assert(integer(action.id, 1, 2147483647), "notification id must be a positive integer")
		assert(action.action == nil or type(action.action) == "string", "notification action must be a string")
		app:notificationAction(action.id, action.action)
	elseif kind == "checkUpdates" then
		app:checkForUpdates()
	elseif kind == "focusChange" then
		assert(type(action.id) == "string", "focusChange requires an id")
		app:focusChange(action.id)
	end
	return self:getState()
end

function PluginController:execute(packet)
	if packet.command == "getPluginState" then
		return self:getState()
	elseif packet.command == "getPluginChanges" then
		return self:getChanges(packet.offset, packet.limit)
	elseif packet.command == "getPluginDiff" then
		return self:getDiff(packet.id, packet.property, packet.side, packet.offset, packet.limit, packet.revision)
	elseif packet.command == "pluginAction" then
		return self:action(packet.action)
	end
	error("Unsupported plugin command: " .. tostring(packet.command), 0)
end

return PluginController

local Packages = script.Parent.Parent.Packages
local HttpService = game:GetService("HttpService")
local Http = require(Packages.Http)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)
local Types = require(script.Parent.Types)
local Version = require(script.Parent.Version)

local validateApiInfo = Types.ifEnabled(Types.ApiInfoResponse)
local validateApiRead = Types.ifEnabled(Types.ApiReadResponse)
local validateApiSocketPacket = Types.ifEnabled(Types.ApiSocketPacket)
local validateApiSerialize = Types.ifEnabled(Types.ApiSerializeResponse)
local validateApiRefPatch = Types.ifEnabled(Types.ApiRefPatchResponse)

local function rejectFailedRequests(response)
	if response.code >= 400 then
		local message = string.format("HTTP %s:\n%s", tostring(response.code), response.body)

		return Promise.reject(message)
	end

	return response
end

local function rejectWrongProtocolVersion(infoResponseBody)
	if infoResponseBody.protocolVersion ~= Config.protocolVersion then
		local message = (
			"Found a Rojo dev server, but it's using a different protocol version, and is incompatible."
			.. "\nMake sure you have matching versions of both the Rojo plugin and server!"
			.. "\n\nYour client is version %s, with protocol version %s. It expects server version %s."
			.. "\nYour server is version %s, with protocol version %s."
			.. "\n\nGo to https://github.com/rojo-rbx/rojo for more details."
		):format(
			Version.display(Config.version),
			Config.protocolVersion,
			Config.expectedServerVersionString,
			infoResponseBody.serverVersion,
			infoResponseBody.protocolVersion
		)

		return Promise.reject(message)
	end

	return Promise.resolve(infoResponseBody)
end

local function rejectWrongPlaceId(infoResponseBody)
	if infoResponseBody.expectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.expectedPlaceIds, game.PlaceId)

		if not foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.expectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to only be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'servePlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	if infoResponseBody.unexpectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.unexpectedPlaceIds, game.PlaceId)

		if foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.unexpectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to not be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to not be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'blockedPlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	return Promise.resolve(infoResponseBody)
end

local ApiContext = {}
ApiContext.__index = ApiContext

function ApiContext.new(baseUrl, createWebSocket)
	assert(type(baseUrl) == "string", "baseUrl must be a string")
	assert(createWebSocket == nil or type(createWebSocket) == "function", "createWebSocket must be a function")

	local self = {
		__baseUrl = baseUrl,
		__sessionId = nil,
		__messageCursor = -1,
		__wsClient = nil,
		__closeWebSocket = nil,
		__studioControlsEnabled = false,
		__createWebSocket = createWebSocket or function(url)
			return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, { Url = url })
		end,
		__connected = true,
		__activeRequests = {},
	}

	return setmetatable(self, ApiContext)
end

function ApiContext:__fmtDebug(output)
	output:writeLine("ApiContext {{")
	output:indent()

	output:writeLine("Connected: {}", self.__connected)
	output:writeLine("Base URL: {}", self.__baseUrl)
	output:writeLine("Session ID: {}", self.__sessionId)
	output:writeLine("Message Cursor: {}", self.__messageCursor)

	output:unindent()
	output:write("}")
end

function ApiContext:disconnect()
	self.__connected = false
	for request in self.__activeRequests do
		Log.trace("Cancelling request {}", request)
		request:cancel()
	end
	self.__activeRequests = {}

	if self.__closeWebSocket then
		Log.trace("Closing WebSocket client")
		self.__closeWebSocket()
	elseif self.__wsClient then
		self.__wsClient:Close()
	end
	self.__wsClient = nil
end

function ApiContext:setMessageCursor(index)
	self.__messageCursor = index
end

function ApiContext:connect()
	local url = ("%s/api/rojo"):format(self.__baseUrl)

	return Http.get(url)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(rejectWrongProtocolVersion)
		:andThen(function(body)
			assert(validateApiInfo(body))

			return body
		end)
		:andThen(rejectWrongPlaceId)
		:andThen(function(body)
			self.__sessionId = body.sessionId
			self.__studioControlsEnabled = body.studioControls == true

			return body
		end)
end

function ApiContext:read(ids)
	local url = ("%s/api/read/%s"):format(self.__baseUrl, table.concat(ids, ","))

	return Http.get(url):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		assert(validateApiRead(body))

		return body
	end)
end

function ApiContext:write(patch)
	local url = ("%s/api/write"):format(self.__baseUrl)

	local updated = {}
	for _, update in ipairs(patch.updated) do
		local fixedUpdate = {
			id = update.id,
			changedName = update.changedName,
		}

		if next(update.changedProperties) ~= nil then
			fixedUpdate.changedProperties = update.changedProperties
		end

		table.insert(updated, fixedUpdate)
	end

	-- Only add the 'added' field if the table is non-empty, or else the msgpack
	-- encode implementation will turn the table into an array instead of a map,
	-- causing API validation to fail.
	local added
	if next(patch.added) ~= nil then
		added = patch.added
	end

	local body = {
		sessionId = self.__sessionId,
		removed = patch.removed,
		updated = updated,
		added = added,
	}

	body = Http.msgpackEncode(body)

	return Http.post(url, body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(responseBody)
			Log.info("Write response: {:?}", responseBody)

			return responseBody
		end)
end

function ApiContext:sendStudioResult(body)
	assert(self.__studioControlsEnabled, "Studio controls are not enabled by this server")
	assert(self.__connected and self.__wsClient ~= nil, "Studio connection is closed")
	self.__wsClient:Send(Http.jsonEncode({
		sessionId = self.__sessionId,
		packetType = "studioResult",
		body = body,
	}))
end

function ApiContext:connectWebSocket(packetHandlers, getStudioInfo)
	local url = ("%s/api/socket/%s"):format(self.__baseUrl, self.__messageCursor)
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	return Promise.new(function(resolve, reject, onCancel)
		local success, wsClient = pcall(self.__createWebSocket, url)
		if not success then
			reject("Failed to create WebSocket client: " .. tostring(wsClient))
			return
		end
		self.__wsClient = wsClient

		local connections = {}
		local finished = false
		local helloSent = false
		local function finish(err)
			if finished then
				return
			end
			finished = true
			for _, connection in ipairs(connections) do
				connection:Disconnect()
			end
			if self.__wsClient == wsClient then
				self.__wsClient = nil
				self.__closeWebSocket = nil
			end
			pcall(wsClient.Close, wsClient)
			if err then
				reject(err)
			else
				resolve()
			end
		end

		self.__closeWebSocket = function()
			finish()
		end
		onCancel(self.__closeWebSocket)

		table.insert(
			connections,
			wsClient.MessageReceived:Connect(function(msg)
				if finished or not self.__connected then
					return
				end

				local success, err = pcall(function()
					local data
					if type(msg) == "string" and msg:sub(1, 1) == "{" then
						data = Http.jsonDecode(msg)
					else
						data = Http.msgpackDecode(msg)
					end
					assert(type(data) == "table", "Expected a WebSocket packet")
					if data.sessionId ~= self.__sessionId then
						Log.warn("Received message with wrong session ID; ignoring")
						return
					end

					if data.packetType == "studioCommand" then
						if not self.__studioControlsEnabled or not helloSent then
							return
						end
						-- Agent controls must validate their inputs even when optional
						-- debug type checking is disabled for ordinary sync traffic.
						assert(Types.ApiStudioCommandPacket(data))
					else
						assert(validateApiSocketPacket(data))
					end

					Log.trace("Received websocket packet: {:#?}", data)

					local handler = packetHandlers[data.packetType]
					if handler then
						handler(data.body)
					else
						Log.warn("No handler for WebSocket packet type '{}'", data.packetType)
					end
				end)
				if not success then
					Log.error("Error processing WebSocket packet: {}", err)
				end
			end)
		)

		table.insert(
			connections,
			wsClient.Closed:Connect(function()
				if self.__connected then
					finish("WebSocket connection closed unexpectedly")
				else
					finish()
				end
			end)
		)

		table.insert(
			connections,
			wsClient.Error:Connect(function(code, msg)
				finish("WebSocket error: " .. code .. " - " .. msg)
			end)
		)

		local function onOpened()
			if finished or helloSent or not self.__connected or not self.__studioControlsEnabled then
				return
			end
			if getStudioInfo == nil then
				return
			end
			local success, err = pcall(function()
				wsClient:Send(Http.jsonEncode({
					sessionId = self.__sessionId,
					packetType = "studioHello",
					body = getStudioInfo(),
				}))
			end)
			if success then
				helloSent = true
			else
				finish("Failed to register Studio controls: " .. tostring(err))
			end
		end
		table.insert(connections, wsClient.Opened:Connect(onOpened))

		-- The handshake may complete before signal handlers are attached.
		if not self.__connected then
			finish()
		elseif wsClient.ConnectionState == Enum.WebStreamClientState.Open then
			onOpened()
		elseif wsClient.ConnectionState == Enum.WebStreamClientState.Closed then
			finish("WebSocket connection closed before initialization")
		elseif wsClient.ConnectionState == Enum.WebStreamClientState.Error then
			finish("WebSocket connection failed before initialization")
		end
	end)
end

function ApiContext:open(id)
	local url = ("%s/api/open/%s"):format(self.__baseUrl, id)

	return Http.post(url, ""):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		return nil
	end)
end

function ApiContext:serialize(ids: { string })
	local url = ("%s/api/serialize"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return Http.post(url, request_body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiSerialize(response_body))

			return response_body
		end)
end

function ApiContext:refPatch(ids: { string })
	local url = ("%s/api/ref-patch"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return Http.post(url, request_body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiRefPatch(response_body))

			return response_body
		end)
end

return ApiContext

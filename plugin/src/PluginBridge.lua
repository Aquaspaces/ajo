local ApiContext = require(script.Parent.ApiContext)

local PluginBridge = {}
PluginBridge.__index = PluginBridge

-- Only discover local brokers. Connecting a sync project to a remote endpoint
-- must not also give that endpoint control over persistent plugin settings.
local function isLocalEndpoint(url)
	local host, port = string.match(url, "^https?://([^/]+):(%d+)$")
	return host ~= nil
		and (host == "localhost" or host == "127.0.0.1" or host == "[::1]")
		and tonumber(port) >= 1
		and tonumber(port) <= 65535
end

function PluginBridge.new(options)
	return setmetatable({
		__getEndpoints = options.getEndpoints,
		__getInfo = options.getInfo,
		__execute = options.execute,
		__createApi = options.createApi or ApiContext.new,
		__retryDelay = options.retryDelay or 3,
		__generation = 0,
	}, PluginBridge)
end

function PluginBridge:start()
	if self.__running then
		return
	end
	self.__running = true
	self.__generation += 1
	local generation = self.__generation
	local function isCurrent()
		return self.__running and self.__generation == generation
	end
	self.__thread = task.spawn(function()
		while isCurrent() do
			local seen = {}
			for _, url in ipairs(self.__getEndpoints()) do
				if not isCurrent() then
					break
				end
				if seen[url] or not isLocalEndpoint(url) then
					continue
				end
				seen[url] = true
				local api = self.__createApi(url)
				self.__api = api
				local success, info = api:connect():await()
				if isCurrent() and success and info.studioControls == true and info.studioPluginControls == true then
					-- This socket never subscribes to patches or claims the sync lock.
					-- Keep it alive through connect, confirmation, and disconnect.
					api:connectStudioControls({
						studioCommand = function(packet)
							if not isCurrent() then
								return
							end
							local ok, result = pcall(self.__execute, packet, api)
							local response = { requestId = packet.requestId }
							if ok then
								response.result = result
							else
								response.error = tostring(result)
							end
							-- A command may yield while the broker disappears. Its result
							-- belongs only to the socket that delivered it.
							if isCurrent() then
								pcall(api.sendStudioResult, api, response)
							end
						end,
					}, self.__getInfo):await()
				end
				api:disconnect()
				if self.__api == api then
					self.__api = nil
				end
			end
			if isCurrent() then
				task.wait(self.__retryDelay)
			end
		end
	end)
end

function PluginBridge:refreshInfo()
	local api = self.__api
	if api and api.__studioHelloSent then
		pcall(api.sendStudioInfo, api, self.__getInfo())
	end
end

function PluginBridge:stop()
	self.__running = false
	self.__generation += 1
	if self.__api then
		self.__api:disconnect()
		self.__api = nil
	end
end

return PluginBridge

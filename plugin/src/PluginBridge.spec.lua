return function()
	local PluginBridge = require(script.Parent.PluginBridge)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function waitFor(predicate)
		for _ = 1, 50 do
			if predicate() then
				return
			end
			task.wait(0.001)
		end
		assert(predicate(), "Bridge did not reach expected state")
	end

	local function fixture(endpoints, connect, execute)
		local clients = {}
		local bridge = PluginBridge.new({
			getEndpoints = function()
				return endpoints
			end,
			getInfo = function()
				return { placeId = 123 }
			end,
			execute = execute or function(packet)
				return { action = packet.action.type }
			end,
			retryDelay = 0.01,
			createApi = function(url)
				local api = { url = url, replies = {}, closed = false }
				function api:connect()
					if connect then
						return connect(url)
					end
					return Promise.resolve({ studioControls = true, studioPluginControls = true })
				end
				function api:connectStudioControls(handlers, getInfo)
					self.handlers = handlers
					self.info = getInfo()
					return Promise.new(function(resolve)
						self.closeSocket = resolve
					end)
				end
				function api:sendStudioResult(reply)
					table.insert(self.replies, reply)
				end
				function api:disconnect()
					self.closed = true
					if self.closeSocket then
						self.closeSocket()
					end
				end
				table.insert(clients, api)
				return api
			end,
		})
		return bridge, clients
	end

	it("discovers a local broker without syncing and keeps commands available across disconnect", function()
		local bridge, clients = fixture({ "http://localhost:34872" })
		bridge:start()
		bridge:start()
		waitFor(function()
			return clients[1] and clients[1].closeSocket
		end)
		expect(#clients).to.equal(1)
		for _, action in ipairs({ "connect", "disconnect", "reconnect" }) do
			clients[1].handlers.studioCommand({ requestId = action, action = { type = action } })
		end
		expect(#clients[1].replies).to.equal(3)
		expect(clients[1].replies[2].result.action).to.equal("disconnect")
		expect(clients[1].closed).to.equal(false)
		bridge:stop()
		expect(clients[1].closed).to.equal(true)
	end)

	it("skips remote endpoints and servers without the plugin controls capability", function()
		local bridge, clients = fixture({
			"https://example.com:34872",
			"http://localhost:34872",
			"http://localhost:34872",
			"http://127.0.0.1:34873",
		}, function(url)
			return Promise.resolve({ studioControls = true, studioPluginControls = url == "http://127.0.0.1:34873" })
		end)
		bridge:start()
		waitFor(function()
			return clients[2] and clients[2].closeSocket
		end)
		expect(#clients).to.equal(2)
		expect(clients[1].url).to.equal("http://localhost:34872")
		expect(clients[1].closed).to.equal(true)
		bridge:stop()
	end)

	it("returns command errors with the original request ID and handles the next command", function()
		local bridge, clients = fixture({ "http://localhost:34872" }, nil, function(packet)
			if packet.action.type == "bad" then
				error("Rejected action", 0)
			end
			return { ok = true }
		end)
		bridge:start()
		waitFor(function()
			return clients[1] and clients[1].closeSocket
		end)
		clients[1].handlers.studioCommand({ requestId = "first", action = { type = "bad" } })
		clients[1].handlers.studioCommand({ requestId = "second", action = { type = "connect" } })
		expect(clients[1].replies[1].requestId).to.equal("first")
		expect(clients[1].replies[1].error).to.equal("Rejected action")
		expect(clients[1].replies[2].result.ok).to.equal(true)
		bridge:stop()
	end)

	it("rediscovers the broker after its socket closes", function()
		local bridge, clients = fixture({ "http://localhost:34872" })
		bridge:start()
		waitFor(function()
			return clients[1] and clients[1].closeSocket
		end)
		clients[1]:disconnect()
		waitFor(function()
			return clients[2] and clients[2].closeSocket
		end)
		expect(clients[1].closed).to.equal(true)
		bridge:stop()
	end)

	it("does not open a socket if stopped during server discovery", function()
		local finishDiscovery
		local bridge, clients = fixture({ "http://localhost:34872" }, function()
			return Promise.new(function(resolve)
				finishDiscovery = resolve
			end)
		end)
		bridge:start()
		waitFor(function()
			return finishDiscovery ~= nil
		end)
		bridge:stop()
		finishDiscovery({ studioControls = true, studioPluginControls = true })
		task.wait()
		expect(clients[1].handlers).to.equal(nil)
		expect(clients[1].closed).to.equal(true)
	end)
end

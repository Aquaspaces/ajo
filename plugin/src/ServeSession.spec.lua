return function()
	local ServeSession = require(script.Parent.ServeSession)
	local StudioControls = require(script.Parent.StudioControls)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function fixture()
		local calls = { hydrate = 0, patches = 0, sockets = 0 }
		local api = { __studioPluginControlsEnabled = true }
		function api:connect()
			return Promise.resolve({ rootInstanceId = "root", projectName = "Test" })
		end
		function api:disconnect()
			self.disconnected = true
		end
		function api:setMessageCursor() end
		function api:read()
			return Promise.resolve({ messageCursor = 0, instances = {} })
		end
		function api:connectWebSocket(_, getInfo)
			calls.sockets += 1
			calls.getInfo = getInfo
			return Promise.resolve()
		end
		local session = setmetatable({
			__status = ServeSession.Status.Connecting,
			__apiContext = api,
			__connections = {},
			__changeBatcher = { stop = function() end },
			__instanceMap = { fromInstances = {}, stop = function() end },
			__reconciler = {
				hydrate = function()
					calls.hydrate += 1
				end,
				diff = function()
					return true, { added = {}, removed = {}, updated = {} }
				end,
			},
			__updateLoadingText = function() end,
			__applyPatch = function()
				calls.patches += 1
			end,
			__applyGameAndPlaceId = function() end,
		}, ServeSession)
		return session, api, calls
	end

	it("does not start initial sync after disconnecting during server discovery", function()
		local session, api, calls = fixture()
		local resolve
		function api:connect()
			return Promise.new(function(done)
				resolve = done
			end)
		end
		session:start()
		task.wait()
		session:stop()
		resolve({ rootInstanceId = "root", projectName = "Test" })
		task.wait()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(calls.hydrate).to.equal(0)
		expect(calls.sockets).to.equal(0)
	end)

	it("ignores an initial read that completes after disconnect", function()
		local session, api, calls = fixture()
		local resolve
		function api:read()
			return Promise.new(function(done)
				resolve = done
			end)
		end
		local promise = session:__initialSync({ rootInstanceId = "root" })
		task.wait()
		session:stop()
		resolve({ messageCursor = 0, instances = {} })
		local success = promise:await()
		expect(success).to.equal(true)
		expect(calls.hydrate).to.equal(0)
		expect(calls.patches).to.equal(0)
	end)

	it("does not apply a confirmation returned after its session was disconnected", function()
		local session, _, calls = fixture()
		session:setConfirmCallback(function()
			session:stop()
			return "Accept"
		end)
		local success = session:__initialSync({ rootInstanceId = "root" }):await()
		expect(success).to.equal(true)
		expect(calls.hydrate).to.equal(1)
		expect(calls.patches).to.equal(0)
	end)

	it("registers legacy controls only when the independent plugin capability is absent", function()
		for _, enabled in ipairs({ true, false }) do
			local session, api, calls = fixture()
			api.__studioPluginControlsEnabled = enabled
			session:start()
			task.wait()
			expect(calls.sockets).to.equal(1)
			expect(calls.getInfo == nil).to.equal(enabled)
			session:stop()
		end
	end)

	it("keeps initial patch application busy and releases the guard after failure", function()
		local session = fixture()
		session.__studioControls = StudioControls.new({}, {})
		session.__applyPatch = nil
		session.__applyPatchInternal = function()
			expect(session.__studioControls.__syncDepth).to.equal(1)
			error("patch failure", 0)
		end
		expect(pcall(function()
			session:__applyPatch({})
		end)).to.equal(false)
		expect(session.__studioControls.__syncDepth).to.equal(0)
	end)
end

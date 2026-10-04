return function()
	local ApiContext = require(script.Parent.ApiContext)
	local Packages = script.Parent.Parent.Packages
	local Http = require(Packages.Http)
	local Promise = require(Packages.Promise)
	local SESSION_ID = "6c7fdd36-8ed4-4e83-a0e7-f3996cc6ce2a"
	local REQUEST_ID = "db4a6d38-cfb8-41b1-94c9-eab2f1cfc4a7"

	local function createSignal()
		local signal = { listeners = {} }
		function signal:Connect(callback)
			local connection = {}
			self.listeners[connection] = callback
			function connection:Disconnect()
				signal.listeners[self] = nil
			end
			return connection
		end
		function signal:Fire(...)
			for connection, callback in pairs(table.clone(self.listeners)) do
				if self.listeners[connection] then
					callback(...)
				end
			end
		end
		return signal
	end

	local function createFixture(enabled, state)
		local client = {
			Opened = createSignal(),
			Closed = createSignal(),
			Error = createSignal(),
			MessageReceived = createSignal(),
			ConnectionState = state or Enum.WebStreamClientState.Connecting,
			sent = {},
			closeCalls = 0,
		}
		function client:Send(message)
			table.insert(self.sent, message)
		end
		function client:Close()
			self.closeCalls += 1
			self.ConnectionState = Enum.WebStreamClientState.Closed
			self.Closed:Fire()
		end
		local api = ApiContext.new("http://localhost:34872", function(url)
			expect(url).to.equal("ws://localhost:34872/api/socket/0")
			return client
		end)
		api.__sessionId = SESSION_ID
		api.__studioControlsEnabled = enabled
		api:setMessageCursor(0)
		local function connect(handlers)
			local promise = api:connectWebSocket(handlers or {}, function()
				return { placeId = 123, gameId = 456, placeName = "Test place" }
			end)
			promise:catch(function() end)
			task.wait()
			return promise
		end
		return api, client, connect
	end

	local function command(sessionId)
		return Http.jsonEncode({
			sessionId = sessionId or SESSION_ID,
			packetType = "studioCommand",
			body = { requestId = REQUEST_ID, command = "getStatus" },
		})
	end

	it("registers opted-in Studio controls only after the socket opens", function()
		local api, client, connect = createFixture(true)
		connect()
		expect(#client.sent).to.equal(0)
		client.ConnectionState = Enum.WebStreamClientState.Open
		client.Opened:Fire()
		expect(#client.sent).to.equal(1)
		local hello = Http.jsonDecode(client.sent[1])
		expect(hello.sessionId).to.equal(SESSION_ID)
		expect(hello.packetType).to.equal("studioHello")
		expect(hello.body.placeId).to.equal(123)
		expect(hello.body.gameId).to.equal(456)
		expect(hello.body.placeName).to.equal("Test place")
		api:disconnect()
	end)

	it("registers exactly once if the socket opened before handlers were attached", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Open)
		connect()
		client.Opened:Fire()
		client.Opened:Fire()
		expect(#client.sent).to.equal(1)
		api:disconnect()
	end)

	it("never registers or executes commands when the capability is disabled", function()
		local api, client, connect = createFixture(false, Enum.WebStreamClientState.Open)
		local calls = 0
		connect({
			studioCommand = function()
				calls += 1
			end,
		})
		client.Opened:Fire()
		client.MessageReceived:Fire(command())
		expect(calls).to.equal(0)
		expect(#client.sent).to.equal(0)
		expect(pcall(function()
			api:sendStudioResult({ requestId = REQUEST_ID, result = {} })
		end)).to.equal(false)
		api:disconnect()
	end)

	it("dispatches JSON Studio commands and sends JSON results with the same request ID", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Open)
		connect({
			studioCommand = function(body)
				expect(body.command).to.equal("getStatus")
				api:sendStudioResult({ requestId = body.requestId, result = { isEdit = true } })
			end,
		})
		client.MessageReceived:Fire(command())
		expect(#client.sent).to.equal(2)
		local result = Http.jsonDecode(client.sent[2])
		expect(result.sessionId).to.equal(SESSION_ID)
		expect(result.packetType).to.equal("studioResult")
		expect(result.body.requestId).to.equal(REQUEST_ID)
		expect(result.body.result.isEdit).to.equal(true)
		expect(result.body.error).to.equal(nil)
		api:disconnect()
	end)

	it("ignores malformed and wrong-session packets and continues handling valid commands", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Open)
		local calls = 0
		connect({
			studioCommand = function()
				calls += 1
			end,
		})
		for _, packet in ipairs({
			"{bad JSON",
			string.char(0xC1),
			command("a-different-session"),
			Http.jsonEncode({ sessionId = SESSION_ID, packetType = "studioCommand", body = {} }),
		}) do
			expect(pcall(function()
				client.MessageReceived:Fire(packet)
			end)).to.equal(true)
		end
		expect(calls).to.equal(0)
		client.MessageReceived:Fire(command())
		expect(calls).to.equal(1)
		api:disconnect()
	end)

	it("continues dispatching ordinary MessagePack sync packets", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Open)
		local cursor
		connect({
			messages = function(body)
				cursor = body.messageCursor
			end,
		})
		client.MessageReceived:Fire(Http.msgpackEncode({
			sessionId = SESSION_ID,
			packetType = "messages",
			body = { messageCursor = 17, messages = {} },
		}))
		expect(cursor).to.equal(17)
		api:disconnect()
	end)

	it("disconnects every socket handler and settles the connection when stopped", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Open)
		local promise = connect()
		api:disconnect()
		expect(promise:getStatus()).to.equal(Promise.Status.Resolved)
		expect(client.closeCalls).to.equal(1)
		for _, signal in ipairs({ client.Opened, client.Closed, client.Error, client.MessageReceived }) do
			expect(next(signal.listeners)).to.equal(nil)
		end
		client.Opened:Fire()
		expect(#client.sent).to.equal(1)
		expect(api.__wsClient).to.equal(nil)
	end)

	it("rejects and cleans up if the socket fails before handlers are attached", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Error)
		local promise = connect()
		expect(promise:getStatus()).to.equal(Promise.Status.Rejected)
		expect(client.closeCalls).to.equal(1)
		expect(next(client.Opened.listeners)).to.equal(nil)
		expect(#client.sent).to.equal(0)
		api:disconnect()
	end)

	it("closes the socket and disconnects handlers when the connection promise is cancelled", function()
		local api, client, connect = createFixture(true, Enum.WebStreamClientState.Open)
		local promise = connect()
		promise:cancel()
		expect(promise:getStatus()).to.equal(Promise.Status.Cancelled)
		expect(client.closeCalls).to.equal(1)
		expect(next(client.MessageReceived.listeners)).to.equal(nil)
		expect(next(client.Opened.listeners)).to.equal(nil)
		api:disconnect()
	end)
end

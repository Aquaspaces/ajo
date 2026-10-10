return function()
	local PluginController = require(script.Parent.PluginController)
	local Settings = require(script.Parent.Settings)

	local function fixture()
		local settings = setmetatable({
			_values = table.clone(Settings._values),
			set = function(self, name, value)
				self._values[name] = value
			end,
		}, { __index = Settings })
		local app = {
			diffRevision = 0,
			state = {
				appStatus = "NotConnected",
				guiEnabled = false,
				notifications = {},
				patchData = { timestamp = 42, patch = { added = {}, removed = {}, updated = {} } },
			},
			hostValue = "localhost",
			portValue = "34872",
			starts = 0,
			stops = 0,
		}
		function app:getHostAndPort()
			return self.hostValue, self.portValue
		end
		app.setHost = function(value)
			app.hostValue = value
		end
		app.setPort = function(value)
			app.portValue = value
		end
		function app:startSession()
			self.starts += 1
			self.serveSession = {
				getStatus = function()
					return "Connecting"
				end,
			}
		end
		function app:endSession()
			self.stops += 1
			self.serveSession = nil
		end
		return PluginController.new(app, settings), app, settings
	end

	it("reports disconnected plugin controls and public settings without exposing endpoint history", function()
		local controller, app = fixture()
		app.state.notifications[8] = { text = "Second", actions = { Dismiss = {}, Connect = {} } }
		app.state.notifications[2] = { text = "First" }
		local state = controller:execute({ command = "getPluginState" })
		expect(state.sessionStatus).to.equal("Disconnected")
		expect(state.port).to.equal(34872)
		expect(state.settings.priorEndpoints).to.equal(nil)
		expect(state.settingsSchema.priorEndpoints).to.equal(nil)
		expect(state.settingsSchema.twoWaySync.lockedWhileSyncing).to.equal(true)
		expect(state.notifications[1].id).to.equal(2)
		expect(state.notifications[2].actions[1]).to.equal("Connect")
		expect(table.find(state.capabilities, "connect") ~= nil).to.equal(true)
	end)

	it("connects using the shared app lifecycle and chosen endpoint", function()
		local controller, app = fixture()
		controller:execute({ command = "pluginAction", action = { type = "connect", host = "127.0.0.1", port = 12345 } })
		expect(app.hostValue).to.equal("127.0.0.1")
		expect(app.portValue).to.equal("12345")
		expect(app.starts).to.equal(1)
		expect(pcall(function()
			controller:action({ type = "connect", host = "changed" })
		end)).to.equal(false)
		expect(app.hostValue).to.equal("127.0.0.1")
		expect(app.starts).to.equal(1)
		controller:action({ type = "reconnect" })
		expect(app.stops).to.equal(1)
		expect(app.starts).to.equal(2)
	end)

	it("rejects malformed action fields before changing an endpoint or stopping a session", function()
		local controller, app = fixture()
		for _, action in
			{
				{ type = "connect", port = 0 },
				{ type = "connect", port = 34872.5 },
				{ type = "connect", host = "bad\nhost" },
				{ type = "connect", extra = true },
				{ type = "reconnect", port = 123 },
				{ type = "setWindow", enabled = "yes" },
				{ type = "unknown" },
			}
		do
			expect(pcall(function()
				controller:action(action)
			end)).to.equal(false)
		end
		expect(app.starts).to.equal(0)
		expect(app.stops).to.equal(0)
		expect(app.hostValue).to.equal("localhost")
	end)

	it("validates every setting before applying a settings batch", function()
		local controller, _, settings = fixture()
		settings._values.playSounds = true
		for _, invalid in
			{
				{ playSounds = false, unknown = true },
				{ playSounds = false, confirmationBehavior = "Sometimes" },
				{ playSounds = false, largeChangesConfirmationThreshold = 1.5 },
				{ playSounds = false, priorEndpoints = {} },
				{ playSounds = false, logLevel = "Warn" },
			}
		do
			expect(pcall(function()
				controller:action({ type = "setSettings", settings = invalid })
			end)).to.equal(false)
			expect(settings:get("playSounds")).to.equal(true)
		end
		controller:action({
			type = "setSettings",
			settings = { playSounds = false, confirmationBehavior = "Always", logLevel = "Warning" },
		})
		expect(settings:get("playSounds")).to.equal(false)
		expect(settings:get("confirmationBehavior")).to.equal("Always")
	end)

	it("locks two-way sync throughout connection and confirmation", function()
		local controller, app, settings = fixture()
		settings._values.twoWaySync = false
		app:startSession()
		expect(pcall(function()
			controller:action({ type = "setSettings", settings = { twoWaySync = true, playSounds = false } })
		end)).to.equal(false)
		expect(settings:get("twoWaySync")).to.equal(false)
		controller:action({ type = "disconnect" })
		controller:action({ type = "setSettings", settings = { twoWaySync = true } })
		expect(settings:get("twoWaySync")).to.equal(true)
	end)

	it("reports the pending confirmation identity, available decisions, and change counts", function()
		local controller, app = fixture()
		app.pendingConfirmation = {
			id = "confirmation-2",
			serverInfo = { projectName = "Example" },
			twoWaySync = false,
			patch = { added = { first = {} }, removed = { "second" }, updated = {} },
		}
		local state = controller:getState()
		expect(state.confirmation.id).to.equal("confirmation-2")
		expect(state.confirmation.counts.total).to.equal(2)
		expect(table.find(state.confirmation.decisions, "Reject")).to.equal(nil)
		local changes = controller:getChanges()
		expect(changes.confirmationId).to.equal("confirmation-2")
		expect(changes.counts.total).to.equal(2)
	end)

	it("returns sorted pages of property diffs and marks failed changes", function()
		local controller, app = fixture()
		app.state.patchTree = {
			idToNode = {
				ROOT = {},
				b = {
					id = "b",
					name = "Second",
					changeList = { { "Property", "Old", "New" }, { "Name", "Old", "New", { isWarning = true } } },
					isWarning = true,
				},
				a = { id = "a", name = "First" },
			},
		}
		app.state.patchData.unapplied = { updated = { { id = "b" } } }
		local first = controller:getChanges(0, 1)
		expect(first.total).to.equal(2)
		expect(first.changes[1].id).to.equal("a")
		expect(first.nextOffset).to.equal(1)
		local second = controller:getChanges(first.nextOffset, 1)
		expect(second.nextOffset).to.equal(nil)
		expect(second.changes[1].properties[1].old).to.equal("Old")
		expect(second.changes[1].properties[1].new).to.equal("New")
		expect(second.changes[1].properties[1].unapplied).to.equal(true)
		expect(second.unapplied.updated).to.equal(1)
	end)

	it("bounds large source diffs and handles cycles, nil, and non-finite values", function()
		local controller, app = fixture()
		local cyclic = {}
		cyclic.self = cyclic
		app.state.patchTree = {
			idToNode = {
				a = {
					id = "a",
					changeList = {
						{ "Property", "Old", "New" },
						{ "Source", string.rep("🙂", 3000), "" },
						{ "Attributes", cyclic, { number = math.huge } },
						{ "Missing", nil, true },
					},
				},
			},
		}
		local page = controller:getChanges()
		expect(page.truncated).to.equal(true)
		local properties = page.changes[1].properties
		expect(#properties[1].old <= 4096).to.equal(true)
		expect(utf8.len(properties[1].old) ~= nil).to.equal(true)
		expect(properties[2].old.self).to.equal("[cycle]")
		expect(type(properties[2].new.number)).to.equal("string")
		expect(properties[3].old.type).to.equal("nil")
	end)

	it("rejects invalid pagination instead of producing unbounded results", function()
		local controller = fixture()
		for _, values in { { -1, 1 }, { 0, 101 }, { 0, 0 }, { 1.5, 1 }, { "0", 1 } } do
			expect(pcall(function()
				controller:getChanges(values[1], values[2])
			end)).to.equal(false)
		end
	end)

	it("retrieves the complete old Studio source beyond the preview limit in UTF-8 safe chunks", function()
		local controller, app = fixture()
		local source = string.rep("🙂hello\n", 3000) .. "unsaved Studio tail"
		app.state.patchTree = {
			idToNode = {
				a = {
					id = "a",
					changeList = {
						{ "Property", "Old", "New" },
						{ "Source", source, "new" },
					},
				},
			},
		}
		local offset = 0
		local revision
		local chunks = {}
		repeat
			local chunk = controller:execute({
				command = "getPluginDiff",
				id = "a",
				property = "Source",
				side = "old",
				offset = offset,
				limit = 503,
				revision = revision,
			})
			expect(utf8.len(chunk.value) ~= nil).to.equal(true)
			expect(chunk.totalBytes).to.equal(#source)
			table.insert(chunks, chunk.value)
			offset = chunk.nextOffset
			revision = chunk.revision
		until offset == nil
		expect(table.concat(chunks)).to.equal(source)
		expect(pcall(function()
			controller:getDiff("a", "Source", "old", 1, nil, revision)
		end)).to.equal(false)
		expect(pcall(function()
			controller:getDiff("a", "Source", "old", 0, 1)
		end)).to.equal(false)
		expect(pcall(function()
			controller:getDiff("a", "Missing", "old")
		end)).to.equal(false)
	end)

	it("requires a matching diff revision for continuation even when timestamp and length are unchanged", function()
		local controller, app = fixture()
		local row = { "Source", "first source text", "new" }
		app.state.patchTree = { idToNode = { a = { id = "a", changeList = { { "Property", "Old", "New" }, row } } } }
		app.diffRevision = 7
		local first = controller:getDiff("a", "Source", "old", 0, 5)
		expect(first.revision).to.equal(7)
		expect(controller:getChanges().revision).to.equal(7)
		expect(pcall(function()
			controller:getDiff("a", "Source", "old", first.nextOffset, 5)
		end)).to.equal(false)
		local continuation = controller:getDiff("a", "Source", "old", first.nextOffset, 5, first.revision)
		expect(continuation.value).to.equal(" sour")
		row[2] = "other source text"
		app.diffRevision += 1
		expect(#row[2]).to.equal(first.totalBytes)
		expect(app.state.patchData.timestamp).to.equal(first.timestamp)
		expect(pcall(function()
			controller:getDiff("a", "Source", "old", first.nextOffset, 5, first.revision)
		end)).to.equal(false)
		expect(pcall(function()
			controller:getDiff("a", "Source", "old", 0, 5, first.revision)
		end)).to.equal(false)
		expect(controller:getDiff("a", "Source", "old", 0, 5).value).to.equal("other")
	end)

	it("blocks diff reads while shared metadata can mutate during a yield", function()
		local controller, app = fixture()
		app.diffUpdating = 1
		expect(pcall(function()
			controller:getChanges()
		end)).to.equal(false)
		expect(pcall(function()
			controller:getDiff("a", "Source", "old")
		end)).to.equal(false)
	end)

	it("routes confirmation, window, notification, and change focus actions to shared callbacks", function()
		local controller, app = fixture()
		function app:respondConfirmation(id, decision)
			self.response = { id, decision }
		end
		function app:setWindow(enabled)
			self.state.guiEnabled = enabled
		end
		function app:notificationAction(id, action)
			self.notification = { id, action }
		end
		function app:focusChange(id)
			self.focused = id
		end
		controller:action({ type = "respondConfirmation", confirmationId = "confirmation-3", decision = "Accept" })
		controller:action({ type = "setWindow", enabled = true })
		controller:action({ type = "notification", id = 8, action = "Restore" })
		controller:action({ type = "focusChange", id = "abc" })
		expect(app.response[1]).to.equal("confirmation-3")
		expect(app.response[2]).to.equal("Accept")
		expect(app.state.guiEnabled).to.equal(true)
		expect(app.notification[2]).to.equal("Restore")
		expect(app.focused).to.equal("abc")
	end)
end

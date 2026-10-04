return function()
	local StudioControls = require(script.Parent.StudioControls)

	local function createFixture()
		local dataModel = { PlaceId = 123, GameId = 456, Name = "Test place" }
		local runService = { running = false, edit = true }
		function runService:IsRunning()
			return self.running
		end
		function runService:IsEdit()
			return self.edit
		end
		local selection = { instances = {}, setCalls = 0 }
		function selection:Get()
			return self.instances
		end
		function selection:Set(instances)
			self.instances = instances
			self.setCalls += 1
		end
		local instanceMap = { fromIds = {}, fromInstances = {} }
		local function addInstance(id, name)
			local instance = { Name = name, ClassName = "Folder", attached = true }
			function instance:GetFullName()
				return "Workspace." .. self.Name
			end
			function instance:IsDescendantOf(ancestor)
				return ancestor == dataModel and self.attached
			end
			if id then
				instanceMap.fromIds[id] = instance
				instanceMap.fromInstances[instance] = id
			end
			return instance
		end
		return StudioControls.new(instanceMap, {
			game = dataModel,
			runService = runService,
			selection = selection,
		}),
			dataModel,
			runService,
			selection,
			instanceMap,
			addInstance
	end

	it("reads current Studio status for each request", function()
		local controls, dataModel, runService = createFixture()
		local status = controls:execute("getStatus")
		expect(status.placeId).to.equal(123)
		expect(status.gameId).to.equal(456)
		expect(status.placeName).to.equal("Test place")
		expect(status.isRunning).to.equal(false)
		expect(status.isEdit).to.equal(true)

		dataModel.PlaceId = 789
		dataModel.Name = "Updated place"
		runService.running = true
		runService.edit = false
		status = controls:execute("getStatus")
		expect(status.placeId).to.equal(789)
		expect(status.placeName).to.equal("Updated place")
		expect(status.isRunning).to.equal(true)
		expect(status.isEdit).to.equal(false)
	end)

	it("includes both synced and unsynced selected instances", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		selection.instances = { addInstance("known", "Synced"), addInstance(nil, "Unsynced") }
		local result = controls:execute("getSelection")
		expect(#result.selection).to.equal(2)
		expect(result.selection[1].id).to.equal("known")
		expect(result.selection[1].name).to.equal("Synced")
		expect(result.selection[1].className).to.equal("Folder")
		expect(result.selection[1].path).to.equal("Workspace.Synced")
		expect(result.selection[2].id).to.equal(nil)
		expect(result.selection[2].name).to.equal("Unsynced")
	end)

	it("sets selection by Rojo IDs and deduplicates repeated IDs", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		local first = addInstance("first", "First")
		local second = addInstance("second", "Second")
		local result = controls:execute("setSelection", { "second", "first", "second" })
		expect(selection.setCalls).to.equal(1)
		expect(#selection.instances).to.equal(2)
		expect(selection.instances[1]).to.equal(second)
		expect(selection.instances[2]).to.equal(first)
		expect(result.selection[1].id).to.equal("second")
	end)

	it("rejects a selection larger than the response limit instead of truncating it", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		for index = 1, 129 do
			table.insert(selection.instances, addInstance(nil, tostring(index)))
		end
		expect(pcall(function()
			controls:execute("getSelection")
		end)).to.equal(false)
		expect(selection.setCalls).to.equal(0)
	end)

	it("clears selection when given an empty array", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		selection.instances = { addInstance("first", "First") }
		local result = controls:execute("setSelection", {})
		expect(selection.setCalls).to.equal(1)
		expect(#selection.instances).to.equal(0)
		expect(#result.selection).to.equal(0)
	end)

	it("returns the actual Studio selection after setting it", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		local first = addInstance("first", "First")
		addInstance("second", "Second")
		function selection:Set()
			self.instances = { first }
		end
		local result = controls:execute("setSelection", { "first", "second" })
		expect(#result.selection).to.equal(1)
		expect(result.selection[1].id).to.equal("first")
	end)

	it("rejects commands while any sync batch can still restore the old selection", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		addInstance("first", "First")
		controls:beginSync()
		controls:beginSync()
		controls:endSync()
		expect(pcall(function()
			controls:execute("setSelection", { "first" })
		end)).to.equal(false)
		expect(selection.setCalls).to.equal(0)
		controls:endSync()
		controls:execute("setSelection", { "first" })
		expect(selection.setCalls).to.equal(1)
	end)

	it("leaves selection untouched if any requested ID is unknown", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		local previous = { addInstance("first", "First") }
		selection.instances = previous
		local success = pcall(function()
			controls:execute("setSelection", { "first", "missing" })
		end)
		expect(success).to.equal(false)
		expect(selection.setCalls).to.equal(0)
		expect(selection.instances).to.equal(previous)
	end)

	it("rejects detached instances and stale reverse mappings", function()
		local controls, _, _, selection, instanceMap, addInstance = createFixture()
		local instance = addInstance("first", "First")
		instance.attached = false
		expect(pcall(function()
			controls:execute("setSelection", { "first" })
		end)).to.equal(false)
		instance.attached = true
		instanceMap.fromInstances[instance] = nil
		expect(pcall(function()
			controls:execute("setSelection", { "first" })
		end)).to.equal(false)
		expect(selection.setCalls).to.equal(0)
	end)

	it("rejects malformed and excessive ID arrays without changing selection", function()
		local controls, _, _, selection, _, addInstance = createFixture()
		addInstance("first", "First")
		for _, ids in ipairs({ "first", { 5 }, { named = "first" }, { [2] = "first" }, table.create(129, "first") }) do
			expect(pcall(function()
				controls:execute("setSelection", ids)
			end)).to.equal(false)
		end
		expect(pcall(function()
			controls:execute("setSelection")
		end)).to.equal(false)
		expect(selection.setCalls).to.equal(0)
	end)

	it("rejects unsupported commands without changing selection", function()
		local controls, _, _, selection = createFixture()
		expect(pcall(function()
			controls:execute("runScript")
		end)).to.equal(false)
		expect(selection.setCalls).to.equal(0)
	end)
end

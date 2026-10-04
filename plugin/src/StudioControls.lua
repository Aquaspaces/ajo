local MAX_SELECTION_IDS = 128

local StudioControls = {}
StudioControls.__index = StudioControls

function StudioControls.new(instanceMap, services)
	return setmetatable({
		__instanceMap = instanceMap,
		__game = services.game,
		__runService = services.runService,
		__selection = services.selection,
		__syncDepth = 0,
	}, StudioControls)
end

function StudioControls:beginSync()
	self.__syncDepth += 1
end

function StudioControls:endSync()
	self.__syncDepth -= 1
end

function StudioControls:getStatus()
	return {
		placeId = self.__game.PlaceId,
		gameId = self.__game.GameId,
		placeName = self.__game.Name,
		isRunning = self.__runService:IsRunning(),
		isEdit = self.__runService:IsEdit(),
	}
end

function StudioControls:getSelection()
	local instances = self.__selection:Get()
	if #instances > MAX_SELECTION_IDS then
		error("Studio selection contains more than 128 instances; reduce the selection and try again", 0)
	end
	local selection = {}
	for _, instance in ipairs(instances) do
		table.insert(selection, {
			id = self.__instanceMap.fromInstances[instance],
			name = instance.Name,
			className = instance.ClassName,
			path = instance:GetFullName(),
		})
	end
	return { selection = selection }
end

function StudioControls:setSelection(ids)
	if type(ids) ~= "table" then
		error("setSelection requires an array of Rojo instance IDs", 0)
	end

	local count = 0
	for index, id in pairs(ids) do
		if type(index) ~= "number" or index % 1 ~= 0 or index < 1 or type(id) ~= "string" then
			error("setSelection requires an array of Rojo instance IDs", 0)
		end
		count += 1
	end
	if count > MAX_SELECTION_IDS then
		error("setSelection accepts at most 128 instance IDs", 0)
	end

	local instances = {}
	local seen = {}
	for index = 1, count do
		local id = ids[index]
		if id == nil then
			error("setSelection requires an array of Rojo instance IDs", 0)
		end

		local instance = self.__instanceMap.fromIds[id]
		if
			instance == nil
			or self.__instanceMap.fromInstances[instance] ~= id
			or (instance ~= self.__game and not instance:IsDescendantOf(self.__game))
		then
			error("Rojo instance is not available in this Studio session: " .. id, 0)
		end

		if not seen[instance] then
			seen[instance] = true
			table.insert(instances, instance)
		end
	end

	-- Resolve every ID before changing Studio selection so a stale ID cannot
	-- cause a partial mutation.
	self.__selection:Set(instances)
	return self:getSelection()
end

function StudioControls:execute(command, ids)
	-- Sync fallback can yield while replacing Instances and restoring the
	-- previous selection. Acknowledging a mutation during that work would
	-- allow it to be overwritten after the agent receives success.
	if self.__syncDepth > 0 then
		error("Studio is applying a sync update; try the command again after it finishes", 0)
	end
	if command == "getStatus" then
		return self:getStatus()
	elseif command == "getSelection" then
		return self:getSelection()
	elseif command == "setSelection" then
		return self:setSelection(ids)
	end
	error("Unsupported Studio command: " .. tostring(command), 0)
end

return StudioControls

local Rojo = script:FindFirstAncestor("Rojo")
local Log = require(Rojo.Packages.Log)

return function(notification, action)
	-- Shared App actions own removal and cleanup. In particular, fullscreen
	-- dismissal removes the notification synchronously, before an action can run.
	if action.onClick then
		local success, err = pcall(action.onClick, notification)
		if not success then
			Log.warn("Error in notification action: " .. tostring(err))
			return
		end
	end
	if not action.handlesDismissal then
		notification:dismiss()
	end
end

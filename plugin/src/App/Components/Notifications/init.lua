local Rojo = script:FindFirstAncestor("Rojo")

local Packages = Rojo.Packages
local Roact = require(Packages.Roact)

local e = Roact.createElement

local Notification = require(script.Notification)
local FullscreenNotification = require(script.FullscreenNotification)

local Notifications = Roact.Component:extend("Notifications")

function Notifications:render()
	local popupNotifs = {}
	local fullscreenNotifs = {}

	for id, notif in self.props.notifications do
		local actions = {}
		for name, action in notif.actions or {} do
			local sharedAction = table.clone(action)
			sharedAction.handlesDismissal = true
			sharedAction.onClick = function()
				self.props.onAction(id, name)
			end
			actions[name] = sharedAction
		end
		local targetTable = if notif.isFullscreen then fullscreenNotifs else popupNotifs
		local targetComponent = if notif.isFullscreen then FullscreenNotification else Notification
		targetTable["NotifID_" .. id] = e(targetComponent, {
			soundPlayer = self.props.soundPlayer,
			text = notif.text,
			timeout = notif.timeout,
			actions = if notif.actions then actions else nil,
			layoutOrder = id,
			onClose = function()
				self.props.onClose(id)
			end,
		})
	end

	return e("Frame", {
		Size = UDim2.fromScale(1, 1),
		BackgroundTransparency = 1,
	}, {
		Fullscreen = e("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundTransparency = 1,
		}, {
			notifs = Roact.createFragment(fullscreenNotifs),
		}),
		Popups = e("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundTransparency = 1,
		}, {
			Layout = e("UIListLayout", {
				SortOrder = Enum.SortOrder.LayoutOrder,
				HorizontalAlignment = Enum.HorizontalAlignment.Right,
				VerticalAlignment = Enum.VerticalAlignment.Bottom,
				Padding = UDim.new(0, 5),
			}),
			Padding = e("UIPadding", {
				PaddingTop = UDim.new(0, 5),
				PaddingBottom = UDim.new(0, 5),
				PaddingLeft = UDim.new(0, 5),
				PaddingRight = UDim.new(0, 5),
			}),
			notifs = Roact.createFragment(popupNotifs),
		}),
	})
end

return Notifications

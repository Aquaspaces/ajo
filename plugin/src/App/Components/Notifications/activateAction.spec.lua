return function()
	local activateAction = require(script.Parent.activateAction)

	for _, kind in { "popup", "fullscreen" } do
		it("runs the shared " .. kind .. " action before removal and cleans up once", function()
			local available = true
			local cleanups = 0
			local notification = {
				dismiss = function()
					available = false
					cleanups += 1
				end,
			}
			activateAction(notification, {
				handlesDismissal = true,
				onClick = function()
					assert(available, "Action needs its notification to still exist")
					available = false
					cleanups += 1
				end,
			})
			expect(available).to.equal(false)
			expect(cleanups).to.equal(1)
		end)
	end

	it("leaves a rejected notification action available for retry", function()
		local dismissed = false
		activateAction({
			dismiss = function()
				dismissed = true
			end,
		}, {
			onClick = function()
				error("Sync in progress")
			end,
		})
		expect(dismissed).to.equal(false)
	end)
end

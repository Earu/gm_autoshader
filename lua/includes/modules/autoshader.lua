if SERVER then return end
if not util.IsBinaryModuleInstalled("autoshader.core") then return end

require("autoshader.core")

function Shader(name)
	local most_recent = 0
	local shader_name
	for _, f in pairs(file.Find("shaders/fxc/" .. name .. "*.vcs", "GAME")) do
		if f:match(name .. "_%d+%.vcs") then
			local timestamp = tonumber(f:match(name .. "_(%d+)%.vcs"))
			if timestamp and timestamp > most_recent then
				most_recent = timestamp
				shader_name = f
			end
		end
	end

	-- for the case where there isnt a timestamped shader
	if not shader_name then
		shader_name = name
	end

	return shader_name:StripExtension()
end
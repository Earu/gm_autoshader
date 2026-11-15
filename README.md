# gm_autoshader

Garry's Mod client module that compiles `.hlsl` shaders whenever they are saved, so they can be reloaded without restarting the game.

Windows and Proton only.

## Install

Download `gm_autoshader.zip` from the latest release and extract it into `garrysmod/`. It contains:

- `lua/bin/gmcl_autoshader.core_win64.dll` and `gmcl_autoshader.core_win32.dll`
- `lua/includes/modules/autoshader.lua`

## Usage

```lua
require("autoshader")
```

Save shaders directly in `garrysmod/shaders/` (subfolders are not watched, but includes can live in them). Names ending in `30` compile as shader model 3.0, everything else as 2.0b.

Each compile writes `garrysmod/shaders/fxc/<name>_<timestamp>.vcs`. The game keeps the first shader it loaded for a name, so a new name is what makes a change show up. `Shader(name)` returns the newest one:

```lua
local mat = CreateMaterial("my_effect", "screenspace_general", {
	["$pixshader"] = Shader("my_effect_ps30"),
})
```

On first use the module downloads `ShaderCompile.exe` into `bin/autoshader/` and copies the game's `d3dcompiler_47.dll` next to it.

## Console output

```
autoshader: my_effect_ps30 compiled in 0.5s -> shaders/fxc/my_effect_ps30_1794752593.vcs
```

Compile errors point at the source line:

```
autoshader: my_effect_ps30 failed to compile (1 error)
error[X3004]: undeclared identifier 'notDeclared'
 --> shaders\my_effect_ps30.hlsl:3:16
  |
3 |     return uv.x * notDeclared;
  |                   ^^^^^^^^^^^
```

Shaders are also checked against ToGL, the layer native Linux and macOS clients translate shaders with. A shader it cannot translate crashes those clients, so it gets a warning:

```
warning[tex3dlod]: `tex3Dlod` is not supported by ToGL
 --> shaders\my_effect_ps30.hlsl:5:9
  |
5 |     return tex3Dlod(Noise, float4(p, 0)).r;
  |            ^^^^^^^^
  |
  = help: ToGL only translates the LOD form for 2D samplers (`tex2Dlod`)
```

`autoshader_togl 0` turns the check off. It uses [gm_shadercompat](https://github.com/Earu/gm_shadercompat).

## Building

Needs nightly Rust (set by `rust-toolchain.toml`) and a C++ compiler for gm_shadercompat's ToGL code.

```
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc
```

From Linux, the `-pc-windows-gnu` targets work with MinGW-w64. Rename `gm_autoshader.dll` to `gmcl_autoshader.core_win64.dll` (or `_win32`) and put it in `garrysmod/lua/bin/`.

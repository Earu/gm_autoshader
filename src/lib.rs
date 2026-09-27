use std::path::PathBuf;
use std::process::Command;
use crossbeam::queue::SegQueue;
use notify::{RecursiveMode, Watcher, recommended_watcher};

#[macro_use]
extern crate gmod;

fn get_game_path() -> Result<String, String> {
    let mut current = std::env::current_dir()
        .map_err(|e| format!("Failed to get current directory: {e}"))?;

    // The binary might be running from subdirectories like:
    // - GarrysMod/bin/win64/
    // - GarrysMod/bin/
    // - GarrysMod/
    // We need to find the GarrysMod root directory which contains the "garrysmod" folder

    // Try up to 5 parent directories
    for _ in 0..5 {
        // Check if this directory contains "garrysmod" subdirectory
        let garrysmod_path = current.join("garrysmod");
        if garrysmod_path.exists() && garrysmod_path.is_dir() {
            return Ok(current.to_string_lossy().to_string());
        }

        // Go up one directory
        if !current.pop() {
            break;
        }
    }

    Err(format!("Garry's Mod root directory not found"))
}

fn compile_shader(game_path: &str, shader_path: &PathBuf) -> Result<String, String> {
    let game_root = PathBuf::from(game_path);
    let compiler = game_root.join("bin/ShaderCompile.exe");
    let shader_source_dir = game_root.join("garrysmod/shaders");
    let is30 = shader_path.to_str().unwrap().ends_with("30.hlsl"); // check what shader version is being compiled

    let output = Command::new(compiler)
        .current_dir(&game_root)
        .args([
            "/O",
            "3",
            "-ver",
            if is30 { "30" } else { "20b" },
            "-shaderpath",
        ])
        .arg(&shader_source_dir)
        .arg(shader_path)
        .output();

    match output {
        Ok(output) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Err(error) => Err(format!("Failed to execute shader compiler: {error}"))
    }
}

static mut WATCHER: Option<notify::RecommendedWatcher> = None;
static mut COMPILE_RESULTS: Option<SegQueue<Result<String, String>>> = None;

#[lua_function]
fn handle_compile_result(lua: gmod::lua::State) -> i32 {
    unsafe {
        if let Some(queue) = &COMPILE_RESULTS {
            if let Some(result) = queue.pop() {
                match result {
                    Ok(output) => {
                        lua.get_global(lua_string!("MsgN"));
                        if lua.is_function(-1) {
                            lua.push_string(&output);
                            lua.call(1, 0);
                        } else {
                            lua.pop();
                        }

                        return 1; // Return the output string
                    }
                    Err(err) => {
                        lua.error(&format!("Shader compilation error: {err}"));
                    }
                }
            }
        }
    }

    0
}

fn create_queue_handler(lua: gmod::lua::State) {
    unsafe {
        lua.get_global(lua_string!("hook"));
        if !lua.is_table(-1) {
            lua.pop();
            return;
        };

        lua.get_field(-1, lua_string!("Add"));
        if !lua.is_function(-1) {
            lua.pop_n(2);
            return;
        }

        lua.push_string("Think");
        lua.push_string("__auto_shader_compile");
        lua.push_function(handle_compile_result);
        lua.call(3, 0);
    }
}

fn remove_queue_handler(lua: gmod::lua::State) {
    unsafe {
        lua.get_global(lua_string!("hook"));
        if !lua.is_table(-1) {
            lua.pop();
            return;
        };

        lua.get_field(-1, lua_string!("Remove"));
        if !lua.is_function(-1) {
            lua.pop_n(2);
            return;
        }

        lua.push_string("Think");
        lua.push_string("__auto_shader_compile");
        lua.call(2, 0);
    }
}

#[gmod13_open]
fn gmod13_open(lua: gmod::lua::State) -> i32 {
    unsafe {
        let game_path = match get_game_path() {
            Ok(path) => path,
            Err(err) => {
                lua.error(&format!("Failed to get game path: {err}"))
            }
        };

        COMPILE_RESULTS = Some(SegQueue::new());
        let game_path_clone = game_path.clone();
        let watcher = recommended_watcher(move | res: Result<notify::Event, notify::Error> | {
            if let Ok(ev) = res {
                match ev.kind {
                    notify::EventKind::Create(_) |
                    notify::EventKind::Modify(_) => {
                        // Reload shaders
                        if let Some(shader_path) = ev.paths.last() {
                            if shader_path.extension().map(|ext| ext == "hlsl").unwrap_or(false) {
                                let compile_result = compile_shader(&game_path_clone, shader_path);
                                COMPILE_RESULTS.as_ref().unwrap().push(compile_result);
                            }
                        }
                    }
                    _ => {}

                }
            }
        });

        match watcher {
            Ok(mut w) => {
                let path = PathBuf::from(&game_path);

                if let Err(e) = w.watch(&path.join("garrysmod/shaders"), RecursiveMode::NonRecursive) {
                    lua.error(&format!("Failed to watch diretory: {e:?}"));
                }

                WATCHER = Some(w);
                create_queue_handler(lua);
            }
            Err(e) => {
                lua.error(&format!("Failed to create watcher: {e:?}"));
            }
        }


        0
    }
}

#[gmod13_close]
fn gmod13_close(lua: gmod::lua::State) -> i32 {
    unsafe {
        if let Some(w) = WATCHER.take() {
            drop(w);
        }

        remove_queue_handler(lua);
        if let Some(queue) = COMPILE_RESULTS.take() {
            while queue.pop().is_some() {} // Clear the queue
            drop(queue);
        }

        0
    }
}
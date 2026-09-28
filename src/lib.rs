use std::error;
use std::io::Write;
use std::time::Instant;
use std::{fs, path::PathBuf};
use std::process::Command;
use crossbeam::queue::SegQueue;
use notify::event::ModifyKind;
use notify::{RecursiveMode, Watcher, recommended_watcher};
use reqwest::blocking::get;

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

    for _ in 0..5 {
        let garrysmod_path = current.join("garrysmod");
        if garrysmod_path.exists() && garrysmod_path.is_dir() {
            return Ok(current.to_string_lossy().to_string());
        }

        if !current.pop() {
            break;
        }
    }

    Err(format!("Garry's Mod root directory not found"))
}

fn download(url: &str, path: &PathBuf) -> Result<(), Box<dyn error::Error>> {
    let now = Instant::now();
    let response = get(url)?;
    let content = response.bytes()?;

    let mut downloaded_file = fs::File::create(path)?;
    downloaded_file.write_all(&content)?;

    let duration = now.elapsed();
    println!("Downloaded file in {duration:?}");
    Ok(())
}

const COMPILER_URL: &str = "https://raw.githubusercontent.com/Earu/gm_autoshader/refs/heads/main/ShaderCompile.exe";
const COMPILER_NAME: &str = "ShaderCompile_standalone.exe";
fn compile_shader(game_path: &str, shader_path: &PathBuf) -> Result<String, String> {
    let game_root = PathBuf::from(game_path);
    let compiler = game_root.join(format!("bin/{}", COMPILER_NAME));

    match fs::exists(&compiler) {
        Ok(exists) => {
            if !exists {
                if let Err(e) = download(COMPILER_URL, &compiler) {
                    return Err(format!("Failed to download shader compiler: {e}"));
                }
            }
        }
        Err(e) => return Err(format!("Could not check whether shader compiler exists (missing permissions?): {e}"))
    }

    let shader_source_dir = game_root.join("garrysmod/shaders");
    let is30 = shader_path.to_str().map_or(false, |s| s.ends_with("30.hlsl"));
    let shader_name = shader_path.file_stem();
    if let None = shader_name {
        return Err(format!("Shader path does not have a valid file name: {:?}", shader_path));
    }

    let target_vcs_path = game_root.join(format!("garrysmod/shaders/shaders/fxc/{}.vcs", shader_name.unwrap().to_string_lossy()));
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


    let mut compile_result = match output {
        Ok(output) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Err(error) => Err(format!("Failed to execute shader compiler: {error}"))
    };

    if let Ok(_) = compile_result {
        if fs::exists(&target_vcs_path).unwrap_or(false) {
            let unix_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
            if let Err(e) = unix_now {
                return Err(format!("Failed to get current time: {e}"));
            }

            let newer_shader_name = format!("{}_{}.vcs", shader_name.unwrap().to_string_lossy(), unix_now.unwrap().as_secs());
            match fs::copy(&target_vcs_path, &shader_source_dir.join(format!("fxc/{}", newer_shader_name))) {
                Err(e) => {
                    compile_result = Err(format!("Failed to copy compiled shader to shaders/fxc: {e}"));
                }
                _ => match fs::remove_file(&target_vcs_path) {
                    Err(e) => {
                        compile_result = Err(format!("Failed to remove temporary compiled shader: {e}"));
                    }
                    _ => {},
                }
            };
        }
    }

    return compile_result;
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
                        lua.get_global(lua_string!("Msg"));
                        if lua.is_function(-1) {
                            lua.push_string(&output);
                            lua.call(1, 0);
                        } else {
                            lua.pop();
                        }

                        return 0;
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
                    notify::EventKind::Modify(ModifyKind::Data(_)) => {
                        if let Some(shader_path) = ev.paths.last() {
                            if shader_path.extension().map(|ext| ext == "hlsl").unwrap_or(false) {
                                let compile_result = compile_shader(&game_path_clone, shader_path);
                                if let Some(queue) = &COMPILE_RESULTS {
                                    queue.push(compile_result);
                                }
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
            while queue.pop().is_some() {}
            drop(queue);
        }

        0
    }
}
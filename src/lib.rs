use std::error;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::time::Instant;
use std::{fs, path::PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, Once};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
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
const COMPILER_DIR: &str = "bin/autoshader";
const HLSL_COMPILER_DLL: &str = "d3dcompiler_47.dll";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn windowless_command(program: impl AsRef<OsStr>) -> Command {
	#[allow(unused_mut)]
	let mut command = Command::new(program);
	#[cfg(windows)]
	command.creation_flags(CREATE_NO_WINDOW);

	command
}

static RUNNING_COMPILER: Mutex<Option<Child>> = Mutex::new(None);
static CLOSING: AtomicBool = AtomicBool::new(false);

fn run_compiler(mut child: Child) -> io::Result<(ExitStatus, Vec<u8>)> {
	let mut stdout = child.stdout.take();
	if let Ok(mut running) = RUNNING_COMPILER.lock() {
		*running = Some(child);
	}

	let mut log = Vec::new();
	let read = match stdout.as_mut() {
		Some(pipe) => pipe.read_to_end(&mut log).map(|_| ()),
		None => Ok(()),
	};

	let child = RUNNING_COMPILER.lock().ok().and_then(|mut running| running.take());
	read?;

	match child {
		Some(mut child) => Ok((child.wait()?, log)),
		None => Err(io::Error::new(io::ErrorKind::Other, "the shader compiler handle was lost")),
	}
}

fn stop_running_compiler() {
	if let Ok(mut running) = RUNNING_COMPILER.lock() {
		if let Some(child) = running.as_mut() {
			let _ = child.kill();
		}
	}
}

fn is_wine() -> bool {
	let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
	PathBuf::from(system_root).join("system32/winecfg.exe").exists()
}

static WINE_OVERRIDE: Once = Once::new();
fn force_native_hlsl_compiler_under_wine() {
	WINE_OVERRIDE.call_once(|| {
		if !is_wine() {
			return;
		}

		let key = format!("HKCU\\Software\\Wine\\AppDefaults\\{}\\DllOverrides", COMPILER_NAME);
		let result = windowless_command("reg")
			.args(["add", &key, "/v", "d3dcompiler_47", "/t", "REG_SZ", "/d", "native,builtin", "/f"])
			.output();

		if let Err(e) = result {
			println!("Failed to set the Wine override for {HLSL_COMPILER_DLL}, shaders will not work on Windows: {e}");
		}
	});
}

fn readable_output(raw: &[u8]) -> String {
	let text = String::from_utf8_lossy(raw);
	let mut lines: Vec<&str> = text
		.split(|c| c == '\r' || c == '\n')
		.map(|line| line.trim_end())
		.filter(|line| !line.is_empty())
		.collect();

	lines.push("");
	lines.join("\n")
}

fn compile_shader(game_path: &str, shader_path: &PathBuf) -> Result<String, String> {
	let game_root = PathBuf::from(game_path);
	let compiler_dir = game_root.join(COMPILER_DIR);
	if let Err(e) = fs::create_dir_all(&compiler_dir) {
		return Err(format!("Failed to create the shader compiler directory: {e}"));
	}

	let legacy_compiler = game_root.join(format!("bin/{}", COMPILER_NAME));
	let compiler = compiler_dir.join(COMPILER_NAME);

	let game_dll = game_root.join(format!("bin/win64/{}", HLSL_COMPILER_DLL));
	let compiler_dll = compiler_dir.join(HLSL_COMPILER_DLL);
	if !compiler_dll.exists() && game_dll.exists() {
		if let Err(e) = fs::copy(&game_dll, &compiler_dll) {
			return Err(format!("Failed to copy {HLSL_COMPILER_DLL} next to the shader compiler: {e}"));
		}
	}

	match fs::exists(&compiler) {
		Ok(exists) => {
			if !exists {
				let copied = legacy_compiler.exists() && fs::copy(&legacy_compiler, &compiler).is_ok();
				if !copied {
					if let Err(e) = download(COMPILER_URL, &compiler) {
						return Err(format!("Failed to download shader compiler: {e}"));
					}
				}
			}
		}
		Err(e) => return Err(format!("Could not check whether shader compiler exists (missing permissions?): {e}"))
	}

	force_native_hlsl_compiler_under_wine();

	let shader_source_dir = game_root.join("garrysmod/shaders");
	let is30 = shader_path.to_str().map_or(false, |s| s.ends_with("30.hlsl"));
	let shader_name = shader_path.file_stem();
	if let None = shader_name {
		return Err(format!("Shader path does not have a valid file name: {:?}", shader_path));
	}

	let target_vcs_path = game_root.join(format!("garrysmod/shaders/shaders/fxc/{}.vcs", shader_name.unwrap().to_string_lossy()));
	let output = windowless_command(compiler)
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
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.and_then(run_compiler);


	let mut compile_result = match output {
		Ok((status, log)) => {
			let log = readable_output(&log);
			if status.success() { Ok(log) } else { Err(log) }
		}
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

const DEBOUNCE: Duration = Duration::from_millis(500);

fn compile_changes(game_path: String, changes: Receiver<PathBuf>) {
	while let Ok(first) = changes.recv() {
		let mut pending = vec![first];
		while let Ok(shader_path) = changes.recv_timeout(DEBOUNCE) {
			if !pending.contains(&shader_path) {
				pending.push(shader_path);
			}
		}

		for shader_path in pending {
			if CLOSING.load(Ordering::SeqCst) {
				return;
			}

			let compile_result = compile_shader(&game_path, &shader_path);
			unsafe {
				if let Some(queue) = &COMPILE_RESULTS {
					queue.push(compile_result);
				}
			}
		}
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
						lua.get_global(lua_string!("ErrorNoHalt"));
						if lua.is_function(-1) {
							lua.push_string(&format!("Shader compilation error:\n{}\n", err.trim_end()));
							lua.call(1, 0);
						} else {
							lua.pop();
						}

						return 0;
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
		CLOSING.store(false, Ordering::SeqCst);
		let (changes, pending_changes) = channel::<PathBuf>();
		let game_path_clone = game_path.clone();
		std::thread::spawn(move || compile_changes(game_path_clone, pending_changes));

		let watcher = recommended_watcher(move | res: Result<notify::Event, notify::Error> | {
			if let Ok(ev) = res {
				match ev.kind {
					notify::EventKind::Create(_) |
					notify::EventKind::Modify(ModifyKind::Any) |
					notify::EventKind::Modify(ModifyKind::Data(_)) => {
						if let Some(shader_path) = ev.paths.last() {
							if shader_path.extension().map(|ext| ext == "hlsl").unwrap_or(false) {
								let _ = changes.send(shader_path.clone());
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
		CLOSING.store(true, Ordering::SeqCst);
		if let Some(w) = WATCHER.take() {
			drop(w);
		}

		stop_running_compiler();
		remove_queue_handler(lua);
		if let Some(queue) = COMPILE_RESULTS.take() {
			while queue.pop().is_some() {}
			drop(queue);
		}

		0
	}
}
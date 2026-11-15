use std::error;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Instant;
use std::{fs, path::PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Mutex, Once};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use crossbeam::queue::SegQueue;
use gm_shadercompat::{Checker, Compiler, Diagnostic, Status, Style};
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
const TOGL_CONVAR: &str = "autoshader_togl";

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
static TOGL_CHECK: AtomicBool = AtomicBool::new(true);

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

fn spawn_compiler(command: &mut Command) -> io::Result<(ExitStatus, Vec<u8>)> {
	command.spawn().and_then(run_compiler)
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

enum Output {
	Compiled(String),
	Warning(String),
	Error(String),
	Notice(String),
	Failed(String),
}

fn prepare_compiler(game_root: &Path) -> Result<PathBuf, String> {
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
		Ok(true) => {}
		Ok(false) => {
			let copied = legacy_compiler.exists() && fs::copy(&legacy_compiler, &compiler).is_ok();
			if !copied {
				if let Err(e) = download(COMPILER_URL, &compiler) {
					return Err(format!("Failed to download shader compiler: {e}"));
				}
			}
		}
		Err(e) => return Err(format!("Could not check whether shader compiler exists (missing permissions?): {e}")),
	}

	force_native_hlsl_compiler_under_wine();
	Ok(compiler)
}

fn rendered(diagnostics: &[Diagnostic], label: &str, root: &Path) -> String {
	let style = Style { color: false };
	diagnostics.iter().map(|d| d.render_as(style, label, Some(root)) + "\n").collect()
}

fn togl_warnings(shader_path: &Path, name: &str, compiler: &Compiler, release: &[u8], root: &Path) -> Vec<Output> {
	let mut checker = Checker::new();
	if !matches!(checker.check_vcs(shader_path, release, None), Status::Fail(_)) {
		return Vec::new();
	}

	if !CLOSING.load(Ordering::SeqCst) {
		if let Ok(debug) = gm_shadercompat::compile(shader_path, compiler, true, spawn_compiler) {
			let mut detailed = Checker::new();
			if matches!(detailed.check_vcs(shader_path, &debug.vcs, None), Status::Fail(_)) {
				checker = detailed;
			}
		}
	}

	vec![
		Output::Warning(rendered(checker.diagnostics(), "warning", root)),
		Output::Notice(format!("autoshader: {name} will crash native Linux and macOS clients (ToGL), {TOGL_CONVAR} 0 hides these warnings\n")),
	]
}

fn compile_shader(game_path: &str, shader_path: &PathBuf) -> Vec<Output> {
	let game_root = PathBuf::from(game_path);
	let root = game_root.join("garrysmod");
	let shader_source_dir = root.join("shaders");

	let Some(name) = shader_path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
		return vec![Output::Failed(format!("Shader path does not have a valid file name: {:?}", shader_path))];
	};

	let compiler = match prepare_compiler(&game_root) {
		Ok(exe) => Compiler { exe, wine: None },
		Err(e) => return vec![Output::Failed(e)],
	};

	let started = Instant::now();
	let compiled = match gm_shadercompat::compile(shader_path, &compiler, false, spawn_compiler) {
		Ok(compiled) => compiled,
		Err(failure) => {
			let mut checker = Checker::new();
			checker.add_compile_failure(shader_path, failure);
			let count = checker.diagnostics().len();
			let errors = if count == 1 { "1 error".to_string() } else { format!("{count} errors") };
			return vec![
				Output::Failed(format!("autoshader: {name} failed to compile ({errors})\n")),
				Output::Error(rendered(checker.diagnostics(), "error", &root)),
			];
		}
	};
	let elapsed = started.elapsed().as_secs_f32();

	let unix_now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
		Ok(now) => now.as_secs(),
		Err(e) => return vec![Output::Failed(format!("Failed to get current time: {e}"))],
	};

	let stamped_name = format!("{name}_{unix_now}.vcs");
	if let Err(e) = fs::write(shader_source_dir.join("fxc").join(&stamped_name), &compiled.vcs) {
		return vec![Output::Failed(format!("autoshader: failed to write the compiled shader to shaders/fxc: {e}\n"))];
	}

	let mut output = vec![Output::Compiled(format!("autoshader: {name} compiled in {elapsed:.1}s -> shaders/fxc/{stamped_name}\n"))];
	if TOGL_CHECK.load(Ordering::SeqCst) {
		output.extend(togl_warnings(shader_path, &name, &compiler, &compiled.vcs, &root));
	}
	output
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
static mut COMPILE_RESULTS: Option<SegQueue<Vec<Output>>> = None;

unsafe fn call_global(lua: gmod::lua::State, name: gmod::lua::LuaString, text: &str) {
	lua.get_global(name);
	if lua.is_function(-1) {
		lua.push_string(text);
		lua.call(1, 0);
	} else {
		lua.pop();
	}
}

unsafe fn print_colored(lua: gmod::lua::State, rgb: (f64, f64, f64), text: &str) {
	lua.get_global(lua_string!("MsgC"));
	if !lua.is_function(-1) {
		lua.pop();
		call_global(lua, lua_string!("Msg"), text);
		return;
	}

	lua.get_global(lua_string!("Color"));
	if !lua.is_function(-1) {
		lua.pop_n(2);
		call_global(lua, lua_string!("Msg"), text);
		return;
	}

	lua.push_number(rgb.0);
	lua.push_number(rgb.1);
	lua.push_number(rgb.2);
	lua.call(3, 1);
	lua.push_string(text);
	lua.call(2, 0);
}

const WARNING_COLOR: (f64, f64, f64) = (255.0, 190.0, 60.0);
const ERROR_COLOR: (f64, f64, f64) = (255.0, 100.0, 100.0);
const BODY_COLOR: (f64, f64, f64) = (210.0, 210.0, 210.0);
const OK_COLOR: (f64, f64, f64) = (120.0, 220.0, 120.0);

unsafe fn print_diagnostics(lua: gmod::lua::State, header: (f64, f64, f64), text: &str) {
	for block in text.split_inclusive("\n\n") {
		let (first, rest) = block.split_at(block.find('\n').map_or(block.len(), |i| i + 1));
		print_colored(lua, header, first);
		if !rest.is_empty() {
			print_colored(lua, BODY_COLOR, rest);
		}
	}
}

#[lua_function]
fn handle_compile_result(lua: gmod::lua::State) -> i32 {
	unsafe {
		let Some(queue) = &COMPILE_RESULTS else { return 0 };
		let Some(outputs) = queue.pop() else { return 0 };

		for output in outputs {
			match output {
				Output::Compiled(text) => print_colored(lua, OK_COLOR, &text),
				Output::Warning(text) => print_diagnostics(lua, WARNING_COLOR, &text),
				Output::Error(text) => print_diagnostics(lua, ERROR_COLOR, &text),
				Output::Notice(text) => print_colored(lua, WARNING_COLOR, &text),
				Output::Failed(text) => call_global(lua, lua_string!("ErrorNoHalt"), &text),
			}
		}
	}

	0
}

#[lua_function]
fn togl_convar_changed(lua: gmod::lua::State) -> i32 {
	unsafe {
		let enabled = lua.get_string(3).and_then(|v| v.trim().parse::<f64>().ok()).map_or(false, |v| v != 0.0);
		TOGL_CHECK.store(enabled, Ordering::SeqCst);
	}

	0
}

fn create_togl_convar(lua: gmod::lua::State) {
	unsafe {
		lua.get_global(lua_string!("CreateClientConVar"));
		if !lua.is_function(-1) {
			lua.pop();
			return;
		}

		lua.push_string(TOGL_CONVAR);
		lua.push_string("1");
		lua.push_boolean(true);
		lua.push_boolean(false);
		lua.push_string("Warn when a compiled shader would crash native Linux and macOS clients (ToGL)");
		lua.call(5, 1);
		if !lua.is_nil(-1) {
			lua.get_field(-1, lua_string!("GetBool"));
			if lua.is_function(-1) {
				lua.push_value(-2);
				lua.call(1, 1);
				TOGL_CHECK.store(lua.get_boolean(-1), Ordering::SeqCst);
			}
			lua.pop();
		}
		lua.pop();

		lua.get_global(lua_string!("cvars"));
		if !lua.is_table(-1) {
			lua.pop();
			return;
		}

		lua.get_field(-1, lua_string!("AddChangeCallback"));
		if !lua.is_function(-1) {
			lua.pop_n(2);
			return;
		}

		lua.push_string(TOGL_CONVAR);
		lua.push_function(togl_convar_changed);
		lua.push_string("autoshader");
		lua.call(3, 0);
		lua.pop();
	}
}

fn remove_togl_callback(lua: gmod::lua::State) {
	unsafe {
		lua.get_global(lua_string!("cvars"));
		if !lua.is_table(-1) {
			lua.pop();
			return;
		}

		lua.get_field(-1, lua_string!("RemoveChangeCallback"));
		if !lua.is_function(-1) {
			lua.pop_n(2);
			return;
		}

		lua.push_string(TOGL_CONVAR);
		lua.push_string("autoshader");
		lua.call(2, 0);
		lua.pop();
	}
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
		create_togl_convar(lua);
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
		remove_togl_callback(lua);
		if let Some(queue) = COMPILE_RESULTS.take() {
			while queue.pop().is_some() {}
			drop(queue);
		}

		0
	}
}

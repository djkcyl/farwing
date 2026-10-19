use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Options {
    build_directory: Option<PathBuf>,
    native: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            build_directory: None,
            native: false,
        }
    }
}

fn main() -> Result<()> {
    let mut options = Options::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--native") => options.native = true,
            Some("--build-directory") => {
                options.build_directory = Some(
                    args.next()
                        .context("--build-directory needs a path")?
                        .into(),
                );
            }
            Some("--help" | "-h") => {
                println!(
                    "cargo dist [--build-directory PATH] [--native]\n\
                    Builds and verifies a single-file self-extracting Windows release in target/dist.\n\
                    --native writes only the native executable to target/dist/native."
                );
                return Ok(());
            }
            _ => bail!("unknown release option: {}", arg.to_string_lossy()),
        }
    }
    ensure!(
        cfg!(windows),
        "native release packaging is currently implemented for Windows only"
    );
    let root = dunce::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .context("xtask must be inside the project")?,
    )?;
    let (source, version) = build(&root, options.build_directory)?;
    ensure!(
        source.is_file()
            && source
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")),
        "expected a Windows executable"
    );
    let original_hash = file_hash(&source)?;
    let original_bytes = source.metadata()?.len();
    let architecture = pe_architecture(&source)?;
    let file_name = format!("Farwing-v{version}-windows-{architecture}.exe");
    check_manifest(&source, &root)?;
    check_startup(&source, &root, &version)?;

    let target = root.join("target");
    fs::create_dir_all(&target)?;
    let stage = tempfile::Builder::new()
        .prefix(".dist-")
        .tempdir_in(&target)?;
    ensure!(
        stage
            .path()
            .canonicalize()?
            .starts_with(target.canonicalize()?),
        "invalid staging directory"
    );
    let candidate = stage.path().join(&file_name);
    if options.native {
        fs::copy(&source, &candidate)?;
    } else {
        ensure!(
            architecture == "x86_64",
            "the launcher currently supports Windows x86_64 only"
        );
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        ensure!(
            command(cargo)
                .current_dir(&root)
                .args(["build", "--release", "--locked", "--manifest-path"])
                .arg(root.join("packaging/Cargo.toml"))
                .arg("--target-dir")
                .arg(target.join("launcher"))
                .env("FARWING_PAYLOAD", &source)
                .status()?
                .success(),
            "launcher build failed; native image retained"
        );
        fs::copy(
            target.join("launcher/release/Farwing-launcher.exe"),
            &candidate,
        )?;
    }
    check_manifest(&candidate, &root)?;
    check_startup(&candidate, &root, &version)?;
    ensure!(
        file_hash(&source)? == original_hash,
        "original build changed during packaging"
    );

    let destination = if !options.native {
        target.join("dist")
    } else {
        target.join("dist").join("native")
    };
    fs::create_dir_all(&destination)?;
    let published = destination.join(file_name);
    fs::copy(&candidate, &published).context("publish executable (close it first if in use)")?;

    println!(
        "EXE: {:.2} -> {:.2} MiB; output: {}",
        original_bytes as f64 / 1048576.0,
        candidate.metadata()?.len() as f64 / 1048576.0,
        published.display()
    );
    // Only this freshly created directory is removed; build and published files remain.
    stage.close()?;
    Ok(())
}

// Verify the PE resource, not just the source XML or a successful RC build.
// A missing native manifest makes maintenance look like a legacy installer.
#[cfg(windows)]
fn check_manifest(image: &Path, root: &Path) -> Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(path: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FindResourceW(module: *mut c_void, name: *const u16, kind: *const u16) -> *mut c_void;
        fn LoadResource(module: *mut c_void, resource: *mut c_void) -> *mut c_void;
        fn SizeofResource(module: *mut c_void, resource: *mut c_void) -> u32;
        fn LockResource(resource: *mut c_void) -> *const c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    struct Module(*mut c_void);
    impl Drop for Module {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
    let path: Vec<u16> = image.as_os_str().encode_wide().chain(Some(0)).collect();
    // DATAFILE | IMAGE_RESOURCE maps resources without executing the image.
    let module = Module(unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), 0x22) });
    ensure!(
        !module.0.is_null(),
        "cannot inspect manifest: {}",
        image.display()
    );
    let resource = unsafe { FindResourceW(module.0, 1usize as *const u16, 24usize as *const u16) };
    ensure!(
        !resource.is_null(),
        "application manifest missing: {}",
        image.display()
    );
    let size = unsafe { SizeofResource(module.0, resource) } as usize;
    ensure!(
        size > 0 && size < 65536,
        "invalid application manifest size"
    );
    let loaded = unsafe { LoadResource(module.0, resource) };
    ensure!(!loaded.is_null(), "cannot load application manifest");
    let bytes = unsafe { LockResource(loaded) }.cast::<u8>();
    ensure!(!bytes.is_null(), "cannot read application manifest");
    let expected = fs::read(root.join("assets/windows.manifest"))?;
    ensure!(
        unsafe { std::slice::from_raw_parts(bytes, size) } == expected,
        "embedded application manifest differs from shared source: {}",
        image.display()
    );
    Ok(())
}

#[cfg(not(windows))]
fn check_manifest(_: &Path, _: &Path) -> Result<()> {
    bail!("Windows manifest verification requires Windows")
}

fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);

    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW for build/check helpers.
    }
    command
}

fn build(root: &Path, directory: Option<PathBuf>) -> Result<(PathBuf, String)> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = command(&cargo)
        .current_dir(root)
        .args([
            "build",
            "--release",
            "--locked",
            "--bin",
            "Farwing",
            "--message-format=json",
        ])
        .arg("--target-dir")
        .arg(root.join(directory.unwrap_or_else(|| "target/build-release".into())))
        .stderr(Stdio::inherit())
        .output()
        .context("build release executable")?;
    let mut executables = Vec::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: serde_json::Value =
            serde_json::from_slice(line).context("read Cargo build message")?;
        if message["reason"] == "compiler-message" {
            if let Some(rendered) = message["message"]["rendered"].as_str() {
                eprint!("{rendered}");
            }
        } else if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "Farwing"
            && let Some(path) = message["executable"].as_str()
        {
            executables.push((
                PathBuf::from(path),
                message["package_id"]
                    .as_str()
                    .context("artifact package ID missing")?
                    .to_owned(),
            ));
        }
    }
    ensure!(output.status.success(), "Cargo release build failed");
    ensure!(
        executables.len() == 1,
        "Cargo did not identify one Farwing executable"
    );
    let (path, package_id) = executables.pop().unwrap();
    let metadata = command(cargo)
        .current_dir(root)
        .args(["metadata", "--locked", "--no-deps", "--format-version=1"])
        .stderr(Stdio::inherit())
        .output()
        .context("read built package version")?;
    ensure!(metadata.status.success(), "Cargo metadata failed");
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let package = metadata["packages"]
        .as_array()
        .context("missing Cargo packages")?
        .iter()
        .find(|package| package["id"] == package_id)
        .context("built package no longer matches Cargo metadata")?;
    let version = package["version"]
        .as_str()
        .context("missing package version")?
        .to_owned();
    ensure!(
        version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
        "package version is not a valid filename component"
    );
    // Keep artifact paths usable by the Windows packaging toolchain.
    Ok((
        dunce::canonicalize(path).context("resolve built executable")?,
        version,
    ))
}

fn pe_architecture(path: &Path) -> Result<&'static str> {
    let mut file = File::open(path)?;
    let mut dos = [0; 64];
    file.read_exact(&mut dos)
        .context("read executable DOS header")?;
    ensure!(&dos[..2] == b"MZ", "executable is not a PE image");
    let offset = u32::from_le_bytes(dos[60..64].try_into()?);
    file.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe)
        .context("read executable PE header")?;
    ensure!(&pe[..4] == b"PE\0\0", "invalid PE signature");
    // Read the built image, not the host running this task, to label cross builds correctly.
    match u16::from_le_bytes([pe[4], pe[5]]) {
        0x8664 => Ok("x86_64"),
        0x014c => Ok("i686"),
        0xaa64 => Ok("aarch64"),
        machine => bail!("unsupported Windows architecture: {machine:#x}"),
    }
}

fn file_hash(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().to_vec())
}

fn check_startup(executable: &Path, root: &Path, version: &str) -> Result<()> {
    // This unattended probe must report the actual loader exit code rather than
    // wait behind a Windows application-error dialog until our timeout expires.
    #[cfg(windows)]
    let previous_error_mode = unsafe { SetErrorMode(GetErrorMode() | 0x0001 | 0x0002) };
    let spawned = command(executable)
        .arg("--version")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    #[cfg(windows)]
    unsafe {
        SetErrorMode(previous_error_mode);
    }
    let mut child = spawned.context("start release check")?;
    let mut stdout = child.stdout.take().context("missing check output pipe")?;
    let mut stderr = child.stderr.take().context("missing check error pipe")?;
    let output = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(15) {
            // This child was created here with --version and has no GUI session.
            let _ = child.kill();
            let _ = child.wait();
            let _ = output.join();
            let _ = errors.join();
            bail!("release startup check timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("check output reader failed"))??;
    let errors = errors
        .join()
        .map_err(|_| anyhow::anyhow!("check error reader failed"))??;
    ensure!(
        status.success()
            && String::from_utf8_lossy(&output).trim() == format!("Farwing {version}"),
        "release startup check failed ({status}): stdout={} stderr={}",
        String::from_utf8_lossy(&output),
        String::from_utf8_lossy(&errors)
    );
    Ok(())
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetErrorMode() -> u32;
    fn SetErrorMode(mode: u32) -> u32;
}

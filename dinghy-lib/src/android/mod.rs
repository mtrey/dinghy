use config::PlatformConfiguration;
use platform::regular_platform::RegularPlatform;
use std::{env, fs, path, process, sync};
use toolchain::ToolchainConfig;
use {Compiler, Device, Platform, PlatformManager, Result};

pub use self::device::AndroidDevice;

mod device;

pub struct AndroidManager {
    compiler: sync::Arc<Compiler>,
    adb: path::PathBuf,
}

impl PlatformManager for AndroidManager {
    fn devices(&self) -> Result<Vec<Box<Device>>> {
        let result = process::Command::new(&self.adb).arg("devices").output()?;
        let mut devices = vec![];
        let device_regex = ::regex::Regex::new(r#"^(\S+)\tdevice\r?$"#)?;
        for line in String::from_utf8(result.stdout)?.split("\n").skip(1) {
            if let Some(caps) = device_regex.captures(line) {
                let d = AndroidDevice::from_id(self.adb.clone(), &caps[1])?;
                debug!("Discovered Android device {}", d);
                devices.push(Box::new(d) as Box<Device>);
            }
        }
        Ok(devices)
    }
    fn platforms(&self) -> Result<Vec<Box<Platform>>> {
        if let Some(ndk) = ndk()? {
            let default_api_level = "21";
            debug!("Android NDK: {:?}", ndk);
            let version = ndk_version(&ndk)?;
            let major = version
                .split(".")
                .next()
                .ok_or_else(|| format!("Invalid version found for ndk {:?}", &ndk))?;
            let major: usize = major
                .parse()
                .map_err(|_| format!("Invalid version found for ndk {:?}", &ndk))?;
            debug!(
                "Android ndk: {:?}, ndk version: {}, major: {}",
                ndk, version, major
            );
            if major >= 19 {
                let mut platforms = vec![];
                let prebuilt = ndk.join("toolchains/llvm/prebuilt");
                let tools = prebuilt
                    .read_dir()?
                    .next()
                    .ok_or("No tools in toolchain")??;
                let bin = tools.path().join("bin");
                debug!("Android tools bin: {:?}", bin);
                for (rustc_cpu, cc_cpu, binutils_cpu, abi_kind) in &[
                    ("aarch64", "aarch64", "aarch64", "android"),
                    ("armv7", "armv7a", "arm", "androideabi"),
                    ("i686", "i686", "i686", "android"),
                    ("x86_64", "x86_64", "x86_64", "android"),
                ] {
                    let mut api_levels : Vec<String> = Vec::new();
                    for entry in tools.path()
                        .join(format!("sysroot/usr/lib/{}-linux-{}",binutils_cpu,abi_kind)).read_dir()? {
                            let entry = entry?;
                            if entry.file_type()?.is_dir() {
                                let folder_name = entry.file_name().into_string().unwrap();
                                match folder_name.parse::<u32>() {
                                    Ok(_) => api_levels.push(folder_name),
                                    Err(_) => {}
                                }
                            }
                    }
                    api_levels.sort();
                    let create_platform = |api: &str, suffix: &str| {
                        let id = format!("auto-android-{}{}", rustc_cpu, suffix);
                        let tc = ToolchainConfig {
                            bin_dir: bin.clone(),
                            rustc_triple: format!("{}-linux-{}", rustc_cpu, abi_kind),
                            root: prebuilt.clone(),
                            sysroot: tools.path().join("sysroot"),
                            cc: "clang".to_string(),
                            binutils_prefix: format!("{}-linux-{}", binutils_cpu, abi_kind),
                            cc_prefix: format!("{}-linux-{}{}", cc_cpu, abi_kind, api),
                        };
                        RegularPlatform::new_with_tc(
                            self.compiler.clone(),
                            PlatformConfiguration::default(),
                            id,
                            tc,
                        )

                    };
                    for api in api_levels.iter() {
                        platforms.push(create_platform(&api,&format!("-api{}",api))?);
                    }
                    if !api_levels.is_empty() {
                        platforms.push(create_platform(api_levels.first().expect("The api level vector shouldn't be empty"),"-min")?);
                        platforms.push(create_platform(api_levels.last().expect("The api level vector shouldn't be empty"),"-latest")?);
                    }
                    platforms.push(create_platform(default_api_level,"")?);
                }
                return Ok(platforms);
            }
        }
        return Ok(vec![]);
    }
}


impl AndroidManager {
    pub fn probe(compiler: sync::Arc<Compiler>) -> Option<AndroidManager> {
        match adb() {
            Ok(adb) => {
                debug!("ADB found: {:?}", adb);
                Some(AndroidManager { adb, compiler })
            }
            Err(_) => {
                debug!("adb not found in path, android disabled");
                None
            }
        }
    }
}

fn probable_sdk_locs() -> Result<Vec<path::PathBuf>> {
    let mut v = vec![];
    for var in &[
        "ANDROID_HOME",
        "ANDROID_SDK",
        "ANDROID_SDK_ROOT",
        "ANDROID_SDK_HOME",
    ] {
        if let Ok(path) = env::var(var) {
            let path = path::Path::new(&path);
            if path.is_dir() {
                v.push(path.to_path_buf())
            }
        }
    }
    if let Ok(home) = env::var("HOME") {
        let mac = path::Path::new(&home).join("/Library/Android/sdk");
        if mac.is_dir() {
            v.push(mac);
        }
    }
    let casks = path::PathBuf::from("/usr/local/Caskroom/android-sdk");
    if casks.is_dir() {
        for kid in casks.read_dir()? {
            let kid = kid?;
            if kid.file_name() != ".metadata" {
                v.push(kid.path());
            }
        }
    }
    debug!("Candidates SDK: {:?}", v);
    Ok(v)
}

fn sdk() -> Result<Option<path::PathBuf>> {
    Ok(probable_sdk_locs()?.into_iter().next())
}

fn ndk() -> Result<Option<path::PathBuf>> {
    if let Ok(path) = env::var("ANDROID_NDK_HOME") {
        return Ok(Some(path.into()));
    }
    for sdk in probable_sdk_locs()? {
        if sdk.join("ndk-bundle/source.properties").is_file() {
            return Ok(Some(sdk.join("ndk-bundle")));
        }
    }
    Ok(None)
}

fn ndk_version(ndk: &path::Path) -> Result<String> {
    let sources_prop_file = ndk.join("source.properties");
    let props = fs::read_to_string(sources_prop_file)?;
    let revision_line = props
        .split("\n")
        .find(|l| l.starts_with("Pkg.Revision"))
        .ok_or(format!(
            "Android NDK at {:?} does not contains a valid ndk-bundle: no source.properties",
            ndk
        ))?;
    Ok(revision_line.split(" ").last().unwrap().to_string())
}

fn adb() -> Result<path::PathBuf> {
    fn try_out(command: &path::Path) -> bool {
        match process::Command::new(command)
            .arg("--version")
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .status()
        {
            Ok(_) => true,
            Err(_) => false,
        }
    }
    if let Ok(adb) = env::var("DINGHY_ANDROID_ADB") {
        return Ok(adb.into());
    }
    if let Ok(adb) = ::which::which("adb") {
        return Ok(adb);
    }
    for loc in probable_sdk_locs()? {
        let adb = loc.join("platform-tools/adb");
        if try_out(&adb) {
            return Ok(adb.into());
        }
    }
    Err("Adb could be found")?
}

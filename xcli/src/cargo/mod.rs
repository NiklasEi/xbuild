use crate::android::AndroidNdk;
use crate::{Arch, CompileTarget, Opt, Platform};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

mod artifact;
mod config;
mod manifest;
pub mod readelf;
mod utils;

pub use artifact::{Artifact, CrateType};

pub struct Cargo {
    package: String,
    manifest: PathBuf,
    target_dir: PathBuf,
}

impl Cargo {
    pub fn new(
        package: Option<&str>,
        manifest_path: Option<PathBuf>,
        target_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let (manifest, package) = utils::find_package(
            &manifest_path.unwrap_or_else(|| std::env::current_dir().unwrap()),
            package.as_ref().map(|s| &**s),
        )?;
        let root_dir = manifest.parent().unwrap();
        let target_dir = target_dir
            .or_else(|| {
                std::env::var_os("CARGO_BUILD_TARGET_DIR")
                    .or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
                    .map(|os_str| os_str.into())
            })
            .map(|target_dir| {
                if target_dir.is_relative() {
                    std::env::current_dir().unwrap().join(target_dir)
                } else {
                    target_dir
                }
            });
        let target_dir = target_dir.unwrap_or_else(|| {
            utils::find_workspace(&manifest, &package)
                .unwrap()
                .unwrap_or_else(|| manifest.clone())
                .parent()
                .unwrap()
                .join(utils::get_target_dir_name(&root_dir).unwrap())
        });
        Ok(Self {
            package,
            manifest,
            target_dir,
        })
    }

    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn manifest(&self) -> &Path {
        &self.manifest
    }

    pub fn root_dir(&self) -> &Path {
        self.manifest.parent().unwrap()
    }

    pub fn examples(&self) -> Result<Vec<Artifact>> {
        let mut artifacts = vec![];
        for file in utils::list_rust_files(&self.root_dir().join("examples"))? {
            artifacts.push(Artifact::Example(file));
        }
        Ok(artifacts)
    }

    pub fn bins(&self) -> Result<Vec<Artifact>> {
        let mut artifacts = vec![];
        for file in utils::list_rust_files(&self.root_dir().join("src").join("bin"))? {
            artifacts.push(Artifact::Root(file));
        }
        Ok(artifacts)
    }

    pub fn build(&self, target: CompileTarget, target_dir: &Path) -> Result<CargoBuild> {
        CargoBuild::new(target, self.root_dir(), target_dir)
    }

    pub fn artifact(
        &self,
        target_dir: &Path,
        target: CompileTarget,
        artifact: Option<Artifact>,
        ty: CrateType,
    ) -> Result<PathBuf> {
        let arch_dir = if target.platform() == Platform::host()? && target.arch() == Arch::host()? {
            target_dir.to_path_buf()
        } else {
            target_dir.join(target.rust_triple()?)
        };
        let opt_dir = arch_dir.join(target.opt().to_string());
        let artifact = artifact.unwrap_or_else(|| Artifact::Root(self.package.clone()));
        let triple = target.rust_triple()?;
        let bin_path = opt_dir
            .join(artifact.as_ref())
            .join(artifact.file_name(ty, triple));
        if !bin_path.exists() {
            anyhow::bail!("failed to locate bin {}", bin_path.display());
        }
        Ok(bin_path)
    }
}

pub struct CargoBuild {
    cmd: Command,
    target: CompileTarget,
    triple: Option<&'static str>,
    c_flags: String,
    rust_flags: String,
}

impl CargoBuild {
    fn new(target: CompileTarget, root_dir: &Path, target_dir: &Path) -> Result<Self> {
        let triple = if target.platform() != Platform::host()? || target.arch() != Arch::host()? {
            Some(target.rust_triple()?)
        } else {
            None
        };
        let mut cmd = Command::new("cargo");
        cmd.current_dir(root_dir);
        cmd.arg("build");
        cmd.arg("--target-dir").arg(target_dir);
        if target.opt() == Opt::Release {
            cmd.arg("--release");
        }
        if let Some(triple) = triple.as_ref() {
            cmd.arg("--target").arg(triple);
        }
        Ok(Self {
            cmd,
            target,
            triple,
            c_flags: "".into(),
            rust_flags: "".into(),
        })
    }

    pub fn use_ndk_tools(&mut self, ndk: &AndroidNdk, sdk_version: u32) -> Result<()> {
        let android_abi = self.target.android_abi()?;
        let (clang, clang_pp) = ndk.clang(android_abi, sdk_version)?;
        self.cfg_tool(Tool::Cc, &clang);
        self.cfg_tool(Tool::Cxx, &clang_pp);
        self.cfg_tool(Tool::Linker, &clang);
        self.cfg_tool(Tool::Ar, &ndk.toolchain_bin("ar", android_abi)?);
        Ok(())
    }

    pub fn use_xwin(&mut self, path: &Path) -> Result<()> {
        let path = path.canonicalize()?;
        self.cfg_tool(Tool::Cc, "clang");
        self.cfg_tool(Tool::Cxx, "clang++");
        self.cfg_tool(Tool::Ar, "llvm-lib");
        self.cfg_tool(Tool::Linker, "rust-lld");
        self.use_ld("lld-link");
        self.add_target_feature("+crt-static");
        self.add_include_dir(&path.join("crt").join("include"));
        self.add_include_dir(&path.join("sdk").join("include").join("um"));
        self.add_include_dir(&path.join("sdk").join("include").join("ucrt"));
        self.add_include_dir(&path.join("sdk").join("include").join("shared"));
        self.add_lib_dir(&path.join("crt").join("lib").join("x86_64"));
        self.add_lib_dir(&path.join("sdk").join("lib").join("um").join("x86_64"));
        self.add_lib_dir(&path.join("sdk").join("lib").join("ucrt").join("x86_64"));
        Ok(())
    }

    pub fn use_macos_sdk(&mut self, path: &Path, minimum_version: &str) -> Result<()> {
        let path = path.canonicalize()?;
        self.cfg_tool(Tool::Cc, "clang");
        self.cfg_tool(Tool::Cxx, "clang++");
        self.cfg_tool(Tool::Ar, "llvm-ar");
        self.cfg_tool(Tool::Linker, "clang");
        self.use_ld("lld");
        self.set_sysroot(&path);
        self.add_define("TARGET_OS_OSX", "1");
        self.add_cflag(&format!("-mmacosx-version-min={}", minimum_version));
        self.add_link_arg("--target=x86_64-apple-darwin");
        self.add_link_arg(&format!("-mmacosx-version-min={}", minimum_version));
        self.add_link_arg(&format!("--sysroot={}", path.display()));
        self.add_link_arg("-rpath");
        self.add_link_arg("@executable_path/../Frameworks");
        self.add_include_dir(&path.join("usr").join("include"));
        self.add_lib_dir(&path.join("usr").join("lib"));
        self.add_lib_dir(&path.join("usr").join("lib").join("system"));
        self.add_framework_dir(&path.join("System").join("Library").join("Frameworks"));
        self.add_framework_dir(
            &path
                .join("System")
                .join("Library")
                .join("PrivateFrameworks"),
        );
        Ok(())
    }

    pub fn cfg_tool<P: AsRef<Path>>(&mut self, tool: Tool, path: P) {
        match tool {
            Tool::Cc | Tool::Cxx | Tool::Ar => {
                self.cc_triple_env(&tool.to_string(), path.as_ref().to_str().unwrap());
            }
            Tool::Linker => {
                self.cargo_target_env("LINKER", path.as_ref().to_str().unwrap());
            }
        }
    }

    /// Configures a cargo target specific environment variable.
    fn cargo_target_env(&mut self, name: &str, value: &str) {
        if let Some(triple) = self.triple {
            let utarget = triple.replace("-", "_");
            let env = format!("CARGO_TARGET_{}_{}", &utarget, name);
            self.cmd.env(env.to_uppercase(), value);
        } else {
            self.cmd.env(name, value);
        }
    }

    /// Configures an environment variable for the `cc` crate.
    fn cc_triple_env(&mut self, name: &str, value: &str) {
        if let Some(triple) = self.triple {
            self.cmd.env(format!("{}_{}", name, triple), value);
        } else {
            self.cmd.env(name, value);
        }
    }

    pub fn add_lib_dir(&mut self, path: &Path) {
        self.rust_flags
            .push_str(&format!("-Lnative={} ", path.display()));
    }

    pub fn add_framework_dir(&mut self, path: &Path) {
        self.rust_flags
            .push_str(&format!("-Lframework={} ", path.display()));
    }

    pub fn link_lib(&mut self, name: &str) {
        self.rust_flags.push_str(&format!("-l{}", name));
    }

    pub fn link_framework(&mut self, name: &str) {
        self.rust_flags.push_str(&format!("-lframework={}", name));
    }

    pub fn add_target_feature(&mut self, target_feature: &str) {
        self.rust_flags
            .push_str(&format!("-Ctarget-feature={} ", target_feature));
    }

    pub fn add_link_arg(&mut self, link_arg: &str) {
        self.rust_flags
            .push_str(&format!("-Clink-arg={} ", link_arg));
    }

    pub fn add_define(&mut self, name: &str, value: &str) {
        self.c_flags.push_str(&format!("-D{}={} ", name, value));
    }

    pub fn add_include_dir(&mut self, path: &Path) {
        self.c_flags.push_str(&format!("-I{} ", path.display()));
    }

    pub fn set_sysroot(&mut self, path: &Path) {
        let arg = format!("--sysroot={}", path.display());
        self.add_cflag(&arg);
        self.add_link_arg(&arg);
    }

    pub fn add_cflag(&mut self, flag: &str) {
        self.c_flags.push_str(flag);
        self.c_flags.push(' ');
    }

    pub fn use_ld(&mut self, name: &str) {
        self.add_link_arg(&format!("-fuse-ld={}", name));
    }

    pub fn exec(mut self) -> Result<()> {
        self.cargo_target_env("RUSTFLAGS", &self.rust_flags.clone());
        self.cc_triple_env("CFLAGS", &self.c_flags.clone());
        self.cc_triple_env("CXXFLAGS", &self.c_flags.clone());
        if !self.cmd.status()?.success() {
            anyhow::bail!("cargo build failed");
        }
        Ok(())
    }

    pub fn search_paths(&self, target_dir: &Path) -> Result<Vec<PathBuf>> {
        let mut paths = vec![];
        let opt = self.target.opt().to_string();
        let target_dir = if let Some(triple) = self.triple.as_ref() {
            target_dir.join(triple).join(&opt)
        } else {
            target_dir.join(&opt)
        };
        let deps_dir = target_dir.join("build");

        for dep_dir in deps_dir.read_dir()? {
            let output_file = dep_dir?.path().join("output");
            if output_file.is_file() {
                use std::{
                    fs::File,
                    io::{BufRead, BufReader},
                };
                for line in BufReader::new(File::open(output_file)?).lines() {
                    let line = line?;
                    if let Some(link_search) = line.strip_prefix("cargo:rustc-link-search=") {
                        let mut pie = link_search.split('=');
                        let (kind, path) = match (pie.next(), pie.next()) {
                            (Some(kind), Some(path)) => (kind, path),
                            (Some(path), None) => ("all", path),
                            _ => unreachable!(),
                        };
                        match kind {
                            // FIXME: which kinds of search path we interested in
                            "dependency" | "native" | "all" => paths.push(path.into()),
                            _ => (),
                        };
                    }
                }
            }
        }
        Ok(paths)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    Cc,
    Cxx,
    Linker,
    Ar,
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Cc => write!(f, "CC"),
            Self::Cxx => write!(f, "CXX"),
            Self::Linker => write!(f, "LINKER"),
            Self::Ar => write!(f, "AR"),
        }
    }
}
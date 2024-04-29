use crate::cargo::CrateType;
use crate::{task, BuildEnv, Format, Opt};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

static BUILD_GRADLE: &[u8] = include_bytes!("./build.gradle");
static GRADLE_PROPERTIES: &[u8] = include_bytes!("./gradle.properties");
static SETTINGS_GRADLE: &[u8] = include_bytes!("./settings.gradle");
static IC_LAUNCHER: &[u8] = include_bytes!("./ic_launcher.xml");
static DUMMY_CPP: &[u8] = include_bytes!("./dummy.cpp");
static DUMMY_CMAKE_LISTS: &[u8] = include_bytes!("./CMakeLists.txt");

pub fn prepare(env: &BuildEnv) -> Result<()> {
    let config = env.config().android();
    if config.wry {
        let package = config.manifest.package.as_ref().unwrap();
        let wry = env.platform_dir().join("wry");
        std::fs::create_dir_all(&wry)?;
        if !env.cargo().package_root().join("kotlin").exists() {
            let main_activity = format!(
                r#"
                    package {}
                    class MainActivity : TauriActivity()
                "#,
                package,
            );
            std::fs::write(wry.join("MainActivity.kt"), main_activity)?;
        }
        let (package, name) = package.rsplit_once('.').unwrap();
        std::env::set_var("WRY_ANDROID_REVERSED_DOMAIN", package);
        std::env::set_var("WRY_ANDROID_APP_NAME_SNAKE_CASE", name);
        std::env::set_var("WRY_ANDROID_KOTLIN_FILES_OUT_DIR", wry);
    }
    Ok(())
}

pub fn build(env: &BuildEnv, out: &Path) -> Result<()> {
    let platform_dir = env.platform_dir();
    let gradle = platform_dir.join("gradle");
    let app = gradle.join("app");
    let main = app.join("src").join("main");
    let dummy_cpp = main.join("cpp");
    let kotlin = main.join("kotlin");
    let jnilibs = main.join("jniLibs");
    let res = main.join("res");

    std::fs::create_dir_all(&kotlin)?;
    std::fs::write(gradle.join("build.gradle"), BUILD_GRADLE)?;
    std::fs::write(gradle.join("gradle.properties"), GRADLE_PROPERTIES)?;
    std::fs::write(gradle.join("settings.gradle"), SETTINGS_GRADLE)?;

    // this is so gradle includes libc++_shared.so
    std::fs::create_dir_all(&dummy_cpp)?;
    std::fs::write(dummy_cpp.join("dummy.cpp"), DUMMY_CPP)?;
    std::fs::write(app.join("CMakeLists.txt"), DUMMY_CMAKE_LISTS)?;

    let config = env.config().android();
    let mut manifest = config.manifest.clone();

    let package = manifest.package.take().unwrap_or_default();
    let target_sdk = manifest.sdk.target_sdk_version.take().unwrap();
    let min_sdk = manifest.sdk.min_sdk_version.take().unwrap();
    let version_code = manifest.version_code.take().unwrap();
    let version_name = manifest.version_name.take().unwrap();

    manifest.compile_sdk_version = None;
    manifest.compile_sdk_version_codename = None;
    manifest.platform_build_version_code = None;
    manifest.platform_build_version_name = None;
    manifest.application.debuggable = None;

    let mut dependencies = String::new();
    for dep in &config.dependencies {
        dependencies.push_str(&format!("implementation '{}'\n", dep));
    }

    let app_build_gradle = format!(
        r#"
            plugins {{
                id 'com.android.application'
                id 'org.jetbrains.kotlin.android'
            }}
            android {{
                namespace '{package}'
                compileSdk {target_sdk}
                defaultConfig {{
                    applicationId '{package}'
                    minSdk {min_sdk}
                    targetSdk {target_sdk}
                    versionCode {version_code}
                    versionName '{version_name}'
                    ndk {{
                        abiFilters 'arm64-v8a', 'armeabi-v7a'
                    }}
                    externalNativeBuild {{
                        cmake {{
                            arguments "-DANDROID_STL=c++_shared"
                        }}
                    }}
                }}
                sourceSets {{
                    main {{
                        assets.srcDirs = ['../../../../../../assets']
                    }}
                }}
                externalNativeBuild {{
                    cmake {{
                        path "CMakeLists.txt"
                    }}
                }}
            }}
            dependencies {{
                {dependencies}
            }}
        "#,
        package = package,
        target_sdk = target_sdk,
        min_sdk = min_sdk,
        version_code = version_code,
        version_name = version_name,
        dependencies = dependencies,
    );

    if let Some(icon_path) = env.icon.as_ref() {
        let mut scaler = xcommon::Scaler::open(icon_path)?;
        scaler.optimize();
        let anydpi = res.join("mipmap-anydpi-v26");
        std::fs::create_dir_all(&anydpi)?;
        std::fs::write(anydpi.join("ic_launcher.xml"), IC_LAUNCHER)?;
        let dpis = [
            ("m", 48),
            ("h", 72),
            ("xh", 96),
            ("xxh", 144),
            ("xxxh", 192),
        ];
        for (name, size) in dpis {
            let dir_name = format!("mipmap-{}dpi", name);
            let dir = res.join(dir_name);
            std::fs::create_dir_all(&dir)?;
            for variant in ["foreground", "monochrome"] {
                let mut icon =
                    std::fs::File::create(dir.join(format!("ic_launcher_{}.png", variant)))?;
                scaler.write(
                    &mut icon,
                    xcommon::ScalerOptsBuilder::new(size, size).build(),
                )?;
            }
        }
        manifest.application.icon = Some("@mipmap/ic_launcher".into());
    }

    std::fs::write(app.join("build.gradle"), app_build_gradle)?;
    std::fs::write(
        main.join("AndroidManifest.xml"),
        quick_xml::se::to_string(&manifest)?,
    )?;

    let srcs = [
        env.cargo().package_root().join("kotlin"),
        env.platform_dir().join("wry"),
    ];
    for src in srcs {
        if !src.exists() {
            continue;
        }
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            std::fs::copy(entry.path(), kotlin.join(entry.file_name()))?;
        }
    }

    for target in env.target().compile_targets() {
        let arch_dir = platform_dir.join(target.arch().to_string());
        let lib = env.cargo_artefact(&arch_dir.join("cargo"), target, CrateType::Cdylib)?;
        let lib_name = lib.file_name().unwrap();
        let lib_dir = jnilibs.join(target.android_abi().android_abi());
        std::fs::create_dir_all(&lib_dir)?;
        std::fs::copy(&lib, lib_dir.join(lib_name))?;
    }

    let opt = env.target().opt();
    let format = env.target().format();

    #[cfg(target_os = "windows")]
    let mut cmd = Command::new("gradle.bat");
    #[cfg(not(target_os = "windows"))]
    let mut cmd = Command::new("gradle");
    
    cmd.current_dir(&gradle);
    cmd.arg(match format {
        Format::Aab => "bundle",
        Format::Apk => "assemble",
        _ => unreachable!(),
    });
    cmd.arg("--warning-mode").arg("all");
    task::run(cmd, true)?;
    let output = gradle
        .join("app")
        .join("build")
        .join("outputs")
        .join(match format {
            Format::Aab => "bundle",
            Format::Apk => "apk",
            _ => unreachable!(),
        })
        .join(opt.to_string())
        .join(match (format, opt) {
            (Format::Apk, Opt::Debug) => "app-debug.apk",
            (Format::Apk, Opt::Release) => "app-release-unsigned.apk",
            (Format::Aab, Opt::Debug) => "app-debug.aab",
            (Format::Aab, Opt::Release) => "app-release.aab",
            _ => unreachable!(),
        });
    std::fs::copy(output, out)?;
    Ok(())
}

//! Embeds a Windows version resource and an application manifest in the exe.
//!
//! Unsigned binaries without any metadata score badly with antivirus heuristics,
//! so the exe carries a product name, file description, version and an
//! `asInvoker` manifest. This needs no extra crates: the resource script is
//! compiled with the Windows SDK's `rc.exe` (or the one named by `RC`) and the
//! resulting `.res` file is handed to the linker. When no resource compiler is
//! found the build carries on without the resources and prints a warning.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=victory.manifest");
    println!("cargo:rerun-if-env-changed=RC");

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=no rc.exe found; victory.exe gets no version resource or manifest");
        return;
    };

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let manifest = manifest_dir.join("victory.manifest");

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts: Vec<u16> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    parts.resize(4, 0);
    let numeric = format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3]);
    let dotted = format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]);

    let script = format!(
        r#"#pragma code_page(65001)
1 24 "{manifest}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "VIctory contributors"
            VALUE "FileDescription", "VIctory runtime and command-line front end"
            VALUE "FileVersion", "{dotted}"
            VALUE "InternalName", "victory"
            VALUE "LegalCopyright", "VIctory contributors, GPL-3.0-or-later"
            VALUE "OriginalFilename", "victory.exe"
            VALUE "ProductName", "VIctory"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        manifest = escape(&manifest),
    );

    let rc_file = out.join("victory.rc");
    let res_file = out.join("victory.res");
    fs::write(&rc_file, script).expect("write victory.rc");

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe failed ({s}); victory.exe gets no version resource or manifest"),
        Err(e) => println!("cargo:warning=could not run rc.exe ({e}); victory.exe gets no version resource or manifest"),
    }
}

/// Doubles backslashes so a path can sit inside an `.rc` string.
fn escape(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

/// `RC` if set, else the newest x64 `rc.exe` in the Windows 10/11 SDK.
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        return Some(PathBuf::from(rc));
    }
    let roots = [
        env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        env::var_os("ProgramFiles").map(PathBuf::from),
    ];
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for root in roots.into_iter().flatten() {
        let bin = root.join("Windows Kits").join("10").join("bin");
        let Ok(entries) = fs::read_dir(&bin) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(key) = name
                .split('.')
                .map(|p| p.parse::<u32>().ok())
                .collect::<Option<Vec<u32>>>()
            else {
                continue;
            };
            let rc = entry.path().join("x64").join("rc.exe");
            if rc.is_file() && best.as_ref().is_none_or(|(k, _)| key > *k) {
                best = Some((key, rc));
            }
        }
    }
    best.map(|(_, rc)| rc).or_else(|| {
        // Fall back to whatever `rc.exe` is on PATH (e.g. a developer prompt).
        Command::new("rc.exe").arg("/?").output().ok().map(|_| PathBuf::from("rc.exe"))
    })
}

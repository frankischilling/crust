fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("lua");

    let mut build = cc::Build::new();
    build.include(&root).std("c99").warnings(false);

    match target_os.as_str() {
        "windows" => {
            build.define("LUA_USE_WINDOWS", None);
        }
        "macos" | "ios" => {
            build
                .define("_POSIX_C_SOURCE", Some("200809L"))
                .define("LUA_USE_MACOSX", None);
        }
        _ => {
            build
                .define("_POSIX_C_SOURCE", Some("200809L"))
                .define("LUA_USE_LINUX", None);
        }
    }

    for file in [
        "lapi.c",
        "lcode.c",
        "lctype.c",
        "ldebug.c",
        "ldo.c",
        "ldump.c",
        "lfunc.c",
        "lgc.c",
        "llex.c",
        "lmem.c",
        "lobject.c",
        "lopcodes.c",
        "lparser.c",
        "lstate.c",
        "lstring.c",
        "ltable.c",
        "ltm.c",
        "lundump.c",
        "lvm.c",
        "lzio.c",
        "lauxlib.c",
        "lbaselib.c",
        "lcorolib.c",
        "ldblib.c",
        "liolib.c",
        "lmathlib.c",
        "loslib.c",
        "lstrlib.c",
        "ltablib.c",
        "lutf8lib.c",
        "loadlib.c",
        "linit.c",
    ] {
        build.file(root.join(file));
        println!("cargo:rerun-if-changed={}", root.join(file).display());
    }
    println!("cargo:rerun-if-changed={}", root.join("lua.h").display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("lauxlib.h").display()
    );
    println!("cargo:rerun-if-changed={}", root.join("lualib.h").display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("luaconf.h").display()
    );

    build.compile("lua");
    println!("cargo:rustc-link-lib=static=lua");

    #[cfg(not(target_os = "windows"))]
    println!("cargo:rustc-link-lib=m");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-lib=dl");
    #[cfg(unix)]
    println!("cargo:rustc-link-lib=pthread");
}

fn main() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("lua");

    let mut build = cc::Build::new();
    build
        .include(&root)
        .define("_POSIX_C_SOURCE", Some("200809L"))
        .define("LUA_USE_LINUX", None)
        .std("c99")
        .warnings(false);

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
    println!(
        "cargo:rerun-if-changed={}",
        root.join("lua.h").display()
    );
    println!("cargo:rerun-if-changed={}", root.join("lauxlib.h").display());
    println!("cargo:rerun-if-changed={}", root.join("lualib.h").display());
    println!("cargo:rerun-if-changed={}", root.join("luaconf.h").display());

    build.compile("lua");
    println!("cargo:rustc-link-lib=static=lua");

    println!("cargo:rustc-link-lib=m");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-lib=dl");
    #[cfg(unix)]
    println!("cargo:rustc-link-lib=pthread");
}

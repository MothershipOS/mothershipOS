// installed as libEGL.so: gfxstream dlopen()s it; every entry point it doesn't define resolves to the real libEGL.so.1
fn main() {
    println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libEGL.so");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--no-as-needed");
    println!("cargo:rustc-cdylib-link-arg=-l:libEGL.so.1");
}

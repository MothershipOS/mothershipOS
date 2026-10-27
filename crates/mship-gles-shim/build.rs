// installed as libGLESv2.so: gfxstream dlopen()s it and dlsym()s every host GL function from it; what this doesn't
// define resolves to the real libGLESv2.so.2
fn main() {
    println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libGLESv2.so");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--no-as-needed");
    println!("cargo:rustc-cdylib-link-arg=-l:libGLESv2.so.2");
    println!("cargo:rustc-cdylib-link-arg=-l:libEGL.so.1");
}

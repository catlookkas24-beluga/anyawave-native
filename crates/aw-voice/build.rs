fn main() {
    // libdave is built with CMake (vendored in third_party/libdave, pinned tag).
    // Then our thin C++ bridge is compiled against it.
    let dave = std::path::Path::new("../../third_party/libdave");
    if dave.exists() {
        let dst = cmake_build(dave);
        println!("cargo:rustc-link-search=native={}/lib", dst);
        println!("cargo:rustc-link-lib=static=dave");
    }
    cc::Build::new().cpp(true).std("c++17")
        .file("../../cpp/dave_bridge.cpp")
        .include("../../third_party/libdave/cpp/includes")
        .compile("aw_dave_bridge");
    println!("cargo:rerun-if-changed=../../cpp/dave_bridge.cpp");
}
fn cmake_build(_p: &std::path::Path) -> String { // replace with the `cmake` crate: cmake::build(p).display().to_string()
    String::from("target/libdave")
}

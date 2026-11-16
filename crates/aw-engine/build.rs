fn main() {
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("../../cpp/dsp.cpp")
        .flag_if_supported("-O3")
        .flag_if_supported("-ffast-math")
        .compile("aw_dsp");
    println!("cargo:rerun-if-changed=../../cpp/dsp.cpp");
}

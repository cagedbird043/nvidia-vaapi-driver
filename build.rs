fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let bindings = bindgen::Builder::default()
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .header_contents("rustnv-va.h", "#include <va/va.h>\n#include <va/va_backend.h>\n#include <va/va_drmcommon.h>\n")
        .allowlist_type("VA.*|VADriver.*|drm_state")
        .allowlist_var("VA_.*|VAProfile.*|VAEntrypoint.*|VAConfigAttrib.*|VASurface.*|VAGeneric.*|VAPicture.*|VAIQ.*|VASlice.*|VAImage.*")
        .allowlist_function("^$")
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .generate().expect("generate libva ABI bindings");
    bindings
        .write_to_file(std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("va.rs"))
        .unwrap();
}

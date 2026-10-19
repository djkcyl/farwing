fn main() {
    println!("cargo:rerun-if-changed=dispatcher");
    let mut cpp = cc::Build::new();
    cpp.cpp(true)
        .std("c++17")
        .opt_level(3)
        .warnings(false)
        .include("dispatcher/api")
        .include("dispatcher/libvpl")
        .define("MFX_DEPRECATED_OFF", None)
        .define("NDEBUG", None)
        .flag_if_supported("/EHsc")
        .flag_if_supported("/Zc:__cplusplus");
    for source in [
        "windows/main.cpp",
        "windows/mfx_critical_section.cpp",
        "windows/mfx_dispatcher.cpp",
        "windows/mfx_dispatcher_log.cpp",
        "windows/mfx_driver_store_loader.cpp",
        "windows/mfx_dxva2_device.cpp",
        "windows/mfx_function_table.cpp",
        "windows/mfx_library_iterator.cpp",
        "windows/mfx_load_dll.cpp",
        "windows/mfx_win_reg_key.cpp",
        "mfx_dispatcher_vpl.cpp",
        "mfx_dispatcher_vpl_loader.cpp",
        "mfx_dispatcher_vpl_config.cpp",
        "mfx_dispatcher_vpl_lowlatency.cpp",
        "mfx_dispatcher_vpl_log.cpp",
        "mfx_dispatcher_vpl_msdk.cpp",
        "mfx_config_interface/mfx_config_interface.cpp",
        "mfx_config_interface/mfx_config_interface_string_api.cpp",
    ] {
        cpp.file(format!("dispatcher/libvpl/src/{source}"));
    }
    cpp.compile("farwing_vpl_dispatcher");
    for library in ["advapi32", "dxgi", "d3d11", "cfgmgr32"] {
        println!("cargo:rustc-link-lib={library}");
    }
}

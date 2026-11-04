fn main() {
    println!("cargo:rerun-if-changed=icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winres::WindowsResource::new();
    resource.set_icon("icon.ico");
    resource
        .compile()
        .expect("无法把 icon.ico 编译进 Windows 资源");
}

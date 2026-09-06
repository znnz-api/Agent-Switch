fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/gui/logo.ico");
        resource.set("ProductName", "znnz.net Agent Launcher");
        resource.set("FileDescription", "znnz.net Agent Launcher");
        resource
            .compile()
            .expect("无法将 assets/gui/logo.ico 写入 Windows EXE 资源");
    }
}

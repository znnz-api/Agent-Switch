fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/gui/logo.ico");
        resource.set("ProductName", "Agent-Switch");
        resource.set("FileDescription", "Agent-Switch");
        resource
            .compile()
            .expect("无法将 assets/gui/logo.ico 写入 Windows EXE 资源");
    }
}

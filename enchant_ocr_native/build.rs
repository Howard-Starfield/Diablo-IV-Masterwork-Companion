fn main() {
    #[cfg(windows)]
    {
        let icon = std::path::Path::new("assets").join("app_icon.ico");
        if icon.exists() {
            let mut resource = winresource::WindowsResource::new();
            resource.set_icon(icon.to_string_lossy().as_ref());
            if let Err(error) = resource.compile() {
                println!("cargo:warning=failed to embed app icon: {error}");
            }
        }
    }
}

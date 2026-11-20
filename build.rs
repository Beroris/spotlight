fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=assets/spotlight.ico");
    println!("cargo:rerun-if-changed=assets/fonts/GeistVariable.ttf");
    // The Slint compiler recurses deeply on nested layouts; Windows' default
    // 1 MB main-thread stack is not enough, so compile on a bigger thread.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| slint_build::compile("ui/app.slint").expect("slint ui build"))
        .expect("spawn slint compiler thread")
        .join()
        .expect("slint compiler thread panicked");

    // This runs on the computer that builds, which could be building for another system.
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon().expect("embed windows icon");
    }
}

#[cfg(target_os = "windows")]
fn embed_windows_icon() -> Result<(), Box<dyn std::error::Error>> {
    // The .ico is used as drawn: its small sizes are a separate, bolder drawing that a shrunk
    // copy of the large logo would not match.
    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/spotlight.ico");
    // Task Manager shows this as the app's name.
    res.set("FileDescription", "Spotlight");
    res.set("ProductName", "Spotlight");
    res.compile()?;
    Ok(())
}


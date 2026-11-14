use std::{env, fs::File, path::{Path, PathBuf}};

fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=assets/spotlight-logo.png");
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
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon().expect("embed windows icon");
    }
}

#[cfg(target_os = "windows")]
fn embed_windows_icon() -> Result<(), Box<dyn std::error::Error>> {
    let icon_path = generate_ico(
        Path::new("assets").join("spotlight-logo.png"),
        PathBuf::from(env::var("OUT_DIR")?).join("spotlight.ico"),
    )?;

    let mut res = winres::WindowsResource::new();
    res.set_icon(icon_path.to_string_lossy().as_ref());
    // Task Manager shows this as the app's name.
    res.set("FileDescription", "Spotlight");
    res.set("ProductName", "Spotlight");
    res.compile()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn generate_ico(source_png: PathBuf, output_ico: PathBuf) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let image = image::open(&source_png)?.into_rgba8();
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

    for size in [16_u32, 24, 32, 48, 64, 128, 256] {
        let resized = image::imageops::resize(&image, size, size, image::imageops::FilterType::Lanczos3);
        let icon_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        icon_dir.add_entry(ico::IconDirEntry::encode(&icon_image)?);
    }

    let mut file = File::create(&output_ico)?;
    icon_dir.write(&mut file)?;
    Ok(output_ico)
}

use image::imageops::FilterType;
use image::DynamicImage;
use std::io::Write;
use std::path::Path;

/// iOS icon sizes (filename, size in pixels)
/// These are the required sizes for App Store submission
const IOS_ICON_SIZES: &[(&str, u32)] = &[
    ("Icon-20@2x.png", 40),
    ("Icon-20@3x.png", 60),
    ("Icon-29@2x.png", 58),
    ("Icon-29@3x.png", 87),
    ("Icon-40@2x.png", 80),
    ("Icon-40@3x.png", 120),
    ("Icon-60@2x.png", 120),
    ("Icon-60@3x.png", 180),
    ("Icon-76.png", 76),
    ("Icon-76@2x.png", 152),
    ("Icon-83.5@2x.png", 167),
    ("Icon-1024.png", 1024),
];

/// Generate all required iOS icon sizes from a source icon
pub fn generate_ios_icons(icon_path: &Path, output_dir: &Path) -> Result<(), String> {
    let img = image::open(icon_path).map_err(|e| format!("Failed to open icon: {e}"))?;

    if img.width() < 1024 || img.height() < 1024 {
        return Err(format!(
            "Icon must be at least 1024x1024, got {}x{}",
            img.width(),
            img.height()
        ));
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create icon output dir: {e}"))?;

    for (filename, size) in IOS_ICON_SIZES {
        let resized = img.resize_exact(*size, *size, FilterType::Lanczos3);
        let output_path = output_dir.join(filename);
        resized
            .save(&output_path)
            .map_err(|e| format!("Failed to save {filename}: {e}"))?;
    }

    Ok(())
}

/// Android icon density buckets (path relative to res/, size in pixels)
const ANDROID_ICON_SIZES: &[(&str, u32)] = &[
    ("mipmap-mdpi/ic_launcher.png", 48),
    ("mipmap-hdpi/ic_launcher.png", 72),
    ("mipmap-xhdpi/ic_launcher.png", 96),
    ("mipmap-xxhdpi/ic_launcher.png", 144),
    ("mipmap-xxxhdpi/ic_launcher.png", 192),
    ("playstore-icon.png", 512),
];

/// Generate all required Android icon sizes from a source icon
pub fn generate_android_icons(icon_path: &Path, output_dir: &Path) -> Result<(), String> {
    let img = image::open(icon_path).map_err(|e| format!("Failed to open icon: {e}"))?;

    if img.width() < 1024 || img.height() < 1024 {
        return Err(format!(
            "Icon must be at least 1024x1024, got {}x{}",
            img.width(),
            img.height()
        ));
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create icon output dir: {e}"))?;

    for (rel_path, size) in ANDROID_ICON_SIZES {
        let resized = img.resize_exact(*size, *size, FilterType::Lanczos3);
        let output_path = output_dir.join(rel_path);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create icon dir {}: {e}", parent.display()))?;
        }
        resized
            .save(&output_path)
            .map_err(|e| format!("Failed to save {rel_path}: {e}"))?;
    }

    Ok(())
}

/// ICNS icon type tags and their sizes
const ICNS_ENTRIES: &[(&[u8; 4], u32)] = &[
    (b"ic07", 128),   // 128x128 PNG
    (b"ic08", 256),   // 256x256 PNG
    (b"ic09", 512),   // 512x512 PNG
    (b"ic10", 1024),  // 1024x1024 PNG (retina 512x512)
    (b"ic11", 32),    // 32x32 PNG (retina 16x16)
    (b"ic12", 64),    // 64x64 PNG (retina 32x32)
    (b"ic13", 256),   // 256x256 PNG (retina 128x128)
    (b"ic14", 512),   // 512x512 PNG (retina 256x256)
];

/// ICNS file magic number
const ICNS_MAGIC: &[u8; 4] = b"icns";

pub fn generate_icns(icon_path: &Path, output_path: &Path) -> Result<(), String> {
    let img = image::open(icon_path).map_err(|e| format!("Failed to open icon: {e}"))?;

    if img.width() < 1024 || img.height() < 1024 {
        return Err(format!(
            "Icon must be at least 1024x1024, got {}x{}",
            img.width(),
            img.height()
        ));
    }

    let mut entries: Vec<(Vec<u8>, &[u8; 4])> = Vec::new();

    for (tag, size) in ICNS_ENTRIES {
        let resized = img.resize_exact(*size, *size, FilterType::Lanczos3);
        let png_data = encode_png(&resized)?;
        entries.push((png_data, tag));
    }

    write_icns(output_path, &entries)
}

pub fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode PNG: {e}"))?;
    Ok(buf)
}

fn write_icns(output_path: &Path, entries: &[(Vec<u8>, &[u8; 4])]) -> Result<(), String> {
    let mut file =
        std::fs::File::create(output_path).map_err(|e| format!("Failed to create icns: {e}"))?;

    // Calculate total size: 8 (header) + sum of (8 + data_len) per entry
    let total_size: u32 = 8 + entries
        .iter()
        .map(|(data, _)| 8 + data.len() as u32)
        .sum::<u32>();

    // Write header
    file.write_all(ICNS_MAGIC)
        .map_err(|e| format!("Write error: {e}"))?;
    file.write_all(&total_size.to_be_bytes())
        .map_err(|e| format!("Write error: {e}"))?;

    // Write each entry
    for (data, tag) in entries {
        let entry_size = 8 + data.len() as u32;
        file.write_all(*tag)
            .map_err(|e| format!("Write error: {e}"))?;
        file.write_all(&entry_size.to_be_bytes())
            .map_err(|e| format!("Write error: {e}"))?;
        file.write_all(data)
            .map_err(|e| format!("Write error: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icns_byte_layout() {
        // Create a minimal 1024x1024 test image
        let img = DynamicImage::new_rgba8(1024, 1024);
        let tmpdir = std::env::temp_dir().join("perry-test-icns");
        std::fs::create_dir_all(&tmpdir).unwrap();

        let icon_path = tmpdir.join("test.png");
        img.save(&icon_path).unwrap();

        let output_path = tmpdir.join("test.icns");
        generate_icns(&icon_path, &output_path).unwrap();

        let data = std::fs::read(&output_path).unwrap();

        // Check magic
        assert_eq!(&data[0..4], b"icns");

        // Check total size matches file size
        let total_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        assert_eq!(total_size as usize, data.len());

        // Check first entry tag
        assert_eq!(&data[8..12], b"ic07");

        // Cleanup
        std::fs::remove_dir_all(&tmpdir).ok();
    }

    #[test]
    fn test_android_icon_generation() {
        let img = DynamicImage::new_rgba8(1024, 1024);
        let tmpdir = std::env::temp_dir().join("perry-test-android-icons");
        std::fs::create_dir_all(&tmpdir).unwrap();

        let icon_path = tmpdir.join("test.png");
        img.save(&icon_path).unwrap();

        let output_dir = tmpdir.join("res");
        generate_android_icons(&icon_path, &output_dir).unwrap();

        // Check that all density bucket files were created
        assert!(output_dir.join("mipmap-mdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-hdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-xhdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-xxhdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-xxxhdpi/ic_launcher.png").exists());
        assert!(output_dir.join("playstore-icon.png").exists());

        // Verify a specific size
        let mdpi = image::open(output_dir.join("mipmap-mdpi/ic_launcher.png")).unwrap();
        assert_eq!(mdpi.width(), 48);
        assert_eq!(mdpi.height(), 48);

        std::fs::remove_dir_all(&tmpdir).ok();
    }

    #[test]
    fn test_rejects_small_icon() {
        let tmpdir = std::env::temp_dir().join("perry-test-icns-small");
        std::fs::create_dir_all(&tmpdir).unwrap();

        let img = DynamicImage::new_rgba8(512, 512);
        let icon_path = tmpdir.join("small.png");
        img.save(&icon_path).unwrap();

        let output_path = tmpdir.join("small.icns");
        let result = generate_icns(&icon_path, &output_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("1024x1024"));

        std::fs::remove_dir_all(&tmpdir).ok();
    }
}

use image::RgbImage;
use log::{info, warn};

pub fn check_liveness(frames: &[RgbImage]) -> bool {
    if frames.len() < 2 {
        warn!("Not enough frames for liveness check");
        return false;
    }

    let mut total_diff = 0.0;
    let mut comparisons = 0;

    for i in 0..frames.len() - 1 {
        let diff = image_difference(&frames[i], &frames[i+1]);
        total_diff += diff;
        comparisons += 1;
    }

    let avg_diff = total_diff / comparisons as f32;
    info!("Liveness score (avg frame diff): {:.5}", avg_diff);

    // Thresholds:
    // Too low (< 0.002): Likely a static photo (was 0.005 - too strict, caused false failures)
    // Too high (> 0.15): Likely excessive movement or lighting changes
    if avg_diff > 0.002 && avg_diff < 0.15 {
        true
    } else {
        warn!("Liveness check failed. Score: {:.5}", avg_diff);
        false
    }
}

fn image_difference(img1: &RgbImage, img2: &RgbImage) -> f32 {
    if img1.dimensions() != img2.dimensions() {
        return 1.0; // Max difference if dimensions mismatch
    }

    let mut diff_sum = 0u64;
    let (width, height) = img1.dimensions();
    let total_pixels = (width * height) as u64;

    for (p1, p2) in img1.pixels().zip(img2.pixels()) {
        let r_diff = (p1[0] as i32 - p2[0] as i32).abs();
        let g_diff = (p1[1] as i32 - p2[1] as i32).abs();
        let b_diff = (p1[2] as i32 - p2[2] as i32).abs();
        diff_sum += (r_diff + g_diff + b_diff) as u64;
    }

    // Normalize to 0.0 - 1.0 range
    // Max difference per pixel is 255 * 3 = 765
    (diff_sum as f32) / (total_pixels as f32 * 765.0)
}

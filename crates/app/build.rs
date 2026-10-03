fn main() {
    // `tauri_build` only re-runs for tauri.conf.json and capabilities/, so a
    // CSS/JS/HTML edit would otherwise ship stale embedded assets. The dashboard
    // lives outside this crate (frontendDist), so track it explicitly.
    println!("cargo:rerun-if-changed=../core/assets/dashboard");
    tauri_build::build();
}

fn main() {
    // bake the build date into the binary. worked out here instead of asking
    // PowerShell (as the Windows-only apps do) so Linux builds get a real date too
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400);
    println!("cargo:rustc-env=BUILD_DATE={}", ymd(days));

    // embed the icon so ofm.exe isn't a blank icon in Explorer and the Start menu.
    // non-fatal if the resource compiler is unavailable
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/ofm.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=Failed to embed exe icon: {e}");
        }
    }
}

// days since 1970-01-01 to YYYY-MM-DD (Howard Hinnant's civil_from_days)
fn ymd(days: u64) -> String {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
